//! Cross-cutting protections: login throttling, client IP detection behind
//! trusted proxies, same-origin checks for cookie-authenticated requests (CSRF),
//! and security response headers.

use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::str::FromStr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::extract::{ConnectInfo, Request, State};
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, Uri, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};

use crate::AppState;
use crate::auth::SESSION_COOKIE;
use crate::json::PhpJson;

/// An IP network in CIDR notation, e.g. `10.0.0.0/8` or `::1/128`. A bare address
/// is a single-host network.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct IpNet {
    addr: IpAddr,
    prefix: u8,
}

impl IpNet {
    pub fn contains(&self, ip: IpAddr) -> bool {
        let ip = canonical(ip);
        match (self.addr, ip) {
            (IpAddr::V4(net), IpAddr::V4(ip)) => {
                prefix_eq(&net.octets(), &ip.octets(), self.prefix)
            }
            (IpAddr::V6(net), IpAddr::V6(ip)) => {
                prefix_eq(&net.octets(), &ip.octets(), self.prefix)
            }
            _ => false,
        }
    }
}

fn prefix_eq(a: &[u8], b: &[u8], prefix: u8) -> bool {
    let (bytes, bits) = ((prefix / 8) as usize, prefix % 8);
    if a[..bytes] != b[..bytes] {
        return false;
    }
    bits == 0 || {
        let mask = 0xffu8 << (8 - bits);
        a[bytes] & mask == b[bytes] & mask
    }
}

/// IPv4-mapped IPv6 addresses (`::ffff:a.b.c.d`) are compared as IPv4.
fn canonical(ip: IpAddr) -> IpAddr {
    match ip {
        IpAddr::V6(v6) => v6.to_ipv4_mapped().map_or(ip, IpAddr::V4),
        v4 => v4,
    }
}

impl FromStr for IpNet {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let (addr, prefix) = match s.split_once('/') {
            Some((addr, prefix)) => (addr, Some(prefix)),
            None => (s, None),
        };
        let addr = canonical(
            addr.trim()
                .parse::<IpAddr>()
                .map_err(|e| format!("invalid IP address '{addr}': {e}"))?,
        );
        let max = if addr.is_ipv4() { 32 } else { 128 };
        let prefix = match prefix {
            Some(p) => p
                .trim()
                .parse::<u8>()
                .ok()
                .filter(|p| *p <= max)
                .ok_or_else(|| format!("invalid prefix length '/{p}'"))?,
            None => max,
        };
        Ok(IpNet { addr, prefix })
    }
}

/// The address of the client that made the request. If the direct peer is a
/// trusted proxy, the right-most untrusted address in `X-Forwarded-For` is used.
pub fn client_ip(
    state: &AppState,
    headers: &HeaderMap,
    peer: Option<SocketAddr>,
) -> Option<IpAddr> {
    let mut ip = canonical(peer?.ip());
    let trusted = |ip: IpAddr| {
        state
            .config
            .trusted_proxies
            .iter()
            .any(|net| net.contains(ip))
    };
    if !trusted(ip) {
        return Some(ip);
    }
    let forwarded: Vec<IpAddr> = headers
        .get_all("x-forwarded-for")
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(','))
        .filter_map(|v| v.trim().parse().ok())
        .collect();
    for hop in forwarded.into_iter().rev() {
        ip = canonical(hop);
        if !trusted(ip) {
            break;
        }
    }
    Some(ip)
}

pub fn peer_addr(extensions: &axum::http::Extensions) -> Option<SocketAddr> {
    extensions.get::<ConnectInfo<SocketAddr>>().map(|c| c.0)
}

/// Failed password attempts allowed per window before further attempts are refused.
const MAX_FAILURES_PER_USER: u32 = 10;
const MAX_FAILURES_PER_IP: u32 = 30;
const FAILURE_WINDOW: Duration = Duration::from_secs(15 * 60);
const MAX_TRACKED_KEYS: usize = 100_000;

/// Counts failed password logins per username and per client IP, like Nextcloud's
/// brute-force protection. App-password logins are never throttled, so an attack
/// on a username can't lock that user's synced devices out.
#[derive(Default)]
pub struct LoginThrottle {
    failures: Mutex<HashMap<String, (u32, Instant)>>,
}

impl LoginThrottle {
    fn keys(username: &str, ip: Option<IpAddr>) -> [(Option<String>, u32); 2] {
        [
            (Some(format!("user:{username}")), MAX_FAILURES_PER_USER),
            (ip.map(|ip| format!("ip:{ip}")), MAX_FAILURES_PER_IP),
        ]
    }

    /// Atomically checks the limits and counts a new password attempt. Counting
    /// before the (slow) password check means concurrent attempts can't all slip
    /// past the limit. Returns false if the attempt must be refused.
    pub fn begin_attempt(&self, username: &str, ip: Option<IpAddr>) -> bool {
        let mut failures = self.failures.lock().unwrap();
        let keys = Self::keys(username, ip);
        let blocked = keys.iter().any(|(key, max)| {
            key.as_ref()
                .and_then(|k| failures.get(k))
                .is_some_and(|(count, since)| *count >= *max && since.elapsed() < FAILURE_WINDOW)
        });
        if blocked {
            return false;
        }
        if failures.len() >= MAX_TRACKED_KEYS {
            failures.retain(|_, (_, since)| since.elapsed() < FAILURE_WINDOW);
            if failures.len() >= MAX_TRACKED_KEYS {
                // Under a flood of distinct keys, keep only username entries so
                // per-user protection still holds.
                failures.retain(|k, _| k.starts_with("user:"));
            }
        }
        for (key, _) in keys {
            let Some(key) = key else { continue };
            let entry = failures.entry(key).or_insert((0, Instant::now()));
            if entry.1.elapsed() >= FAILURE_WINDOW {
                *entry = (0, Instant::now());
            }
            entry.0 += 1;
        }
        true
    }

    /// Undoes the count of a successful attempt and clears the username's failures.
    pub fn record_success(&self, username: &str, ip: Option<IpAddr>) {
        let mut failures = self.failures.lock().unwrap();
        failures.remove(&format!("user:{username}"));
        if let Some(ip) = ip
            && let Some(entry) = failures.get_mut(&format!("ip:{ip}"))
        {
            entry.0 = entry.0.saturating_sub(1);
        }
    }
}

pub struct TooManyAttempts;

impl IntoResponse for TooManyAttempts {
    fn into_response(self) -> Response {
        PhpJson(
            StatusCode::TOO_MANY_REQUESTS,
            serde_json::json!({ "message": "Too many failed login attempts. Try again later." }),
        )
        .into_response()
    }
}

/// `scheme://host[:port]` of the URL, lowercased, with default ports omitted.
fn origin_of(url: &str) -> Option<String> {
    let url = url::Url::parse(url).ok()?;
    match url.origin() {
        url::Origin::Tuple(..) => Some(url.origin().ascii_serialization()),
        url::Origin::Opaque(_) => None,
    }
}

fn authority_of(origin: &str) -> &str {
    origin.split_once("://").map_or(origin, |(_, rest)| rest)
}

/// Whether the browser-supplied `Origin` (or `Referer`) belongs to this server.
fn is_same_origin(state: &AppState, headers: &HeaderMap) -> bool {
    let source = headers
        .get(header::ORIGIN)
        .or_else(|| headers.get(header::REFERER))
        .and_then(|v| v.to_str().ok())
        .and_then(origin_of);
    let Some(source) = source else {
        return false;
    };
    let Some(own) = origin_of(&state.base_url(headers)) else {
        return false;
    };
    if state
        .config
        .public_url
        .as_deref()
        .is_some_and(|u| !u.is_empty())
    {
        source == own
    } else {
        // Without a configured public URL the scheme is a guess (a proxy may not send
        // X-Forwarded-Proto), so compare host and port only.
        authority_of(&source) == authority_of(&own)
    }
}

/// Rejects state-changing browser requests from other origins (CSRF).
///
/// A request is checked when it carries the session cookie, or when it has an
/// `Origin` header (only browsers send one). Podcast clients use Basic auth without
/// cookies or `Origin`, so they are unaffected.
pub async fn csrf_protection(
    State(state): State<Arc<AppState>>,
    request: Request,
    next: Next,
) -> Response {
    let safe_method = matches!(
        *request.method(),
        Method::GET | Method::HEAD | Method::OPTIONS
    );
    let headers = request.headers();
    let has_session = crate::auth::cookie(headers, SESSION_COOKIE).is_some();
    let from_browser = headers.contains_key(header::ORIGIN);
    if !safe_method && (has_session || from_browser) && !is_same_origin(&state, headers) {
        tracing::warn!(
            "rejected cross-origin {} {}",
            request.method(),
            redact(request.uri())
        );
        return PhpJson(
            StatusCode::FORBIDDEN,
            serde_json::json!({ "message": "Cross-origin request rejected" }),
        )
        .into_response();
    }
    next.run(request).await
}

/// Path without the query string, and without the token in login-flow URLs.
fn redact(uri: &Uri) -> String {
    let path = uri.path();
    match path.find("/login/v2/flow/") {
        Some(i) => format!("{}/login/v2/flow/…", &path[..i]),
        None => path.to_owned(),
    }
}

/// Adds headers that keep browsers from sniffing, framing, or leaking URLs.
/// HTML pages set their own `Content-Security-Policy` (see `routes::web::layout`).
///
/// The referrer policy is `same-origin`, not `no-referrer`: under `no-referrer`
/// browsers send `Origin: null` even on same-origin form posts, which
/// [`csrf_protection`] would reject. `same-origin` still sends nothing to other
/// sites, so the login-flow token in page URLs can't leak.
pub async fn security_headers(request: Request, next: Next) -> Response {
    let mut response = next.run(request).await;
    let headers = response.headers_mut();
    for (name, value) in [
        (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
        (header::X_FRAME_OPTIONS, "DENY"),
        (header::REFERRER_POLICY, "same-origin"),
    ] {
        headers
            .entry(name)
            .or_insert(HeaderValue::from_static(value));
    }
    response
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ip_nets() {
        let net: IpNet = "10.0.0.0/8".parse().unwrap();
        assert!(net.contains("10.1.2.3".parse().unwrap()));
        assert!(net.contains("::ffff:10.1.2.3".parse().unwrap()));
        assert!(!net.contains("11.0.0.1".parse().unwrap()));
        let net: IpNet = "172.16.0.0/12".parse().unwrap();
        assert!(net.contains("172.31.255.255".parse().unwrap()));
        assert!(!net.contains("172.32.0.0".parse().unwrap()));
        let host: IpNet = "::1".parse().unwrap();
        assert!(host.contains("::1".parse().unwrap()));
        assert!("10.0.0.0/33".parse::<IpNet>().is_err());
        assert!("nope".parse::<IpNet>().is_err());
    }

    #[test]
    fn throttles_after_repeated_failures() {
        let throttle = LoginThrottle::default();
        let ip = Some("203.0.113.9".parse().unwrap());
        for _ in 0..MAX_FAILURES_PER_USER {
            assert!(throttle.begin_attempt("alice", ip));
        }
        assert!(!throttle.begin_attempt("alice", ip));
        assert!(!throttle.begin_attempt("alice", None));
        assert!(throttle.begin_attempt("bob", Some("203.0.113.10".parse().unwrap())));
        throttle.record_success("alice", ip);
        assert!(throttle.begin_attempt("alice", None));
    }

    #[test]
    fn successful_logins_do_not_count_against_the_ip() {
        let throttle = LoginThrottle::default();
        let ip = Some("203.0.113.9".parse().unwrap());
        for i in 0..(MAX_FAILURES_PER_IP * 2) {
            assert!(throttle.begin_attempt(&format!("user{i}"), ip));
            throttle.record_success(&format!("user{i}"), ip);
        }
    }
}
