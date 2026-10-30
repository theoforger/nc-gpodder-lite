//! Authentication the way Nextcloud clients expect it: HTTP Basic auth with either
//! the account password or an app password (as issued by Login Flow v2), plus a
//! session cookie for the web UI.

use std::net::IpAddr;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use argon2::Argon2;
use argon2::password_hash::phc::PasswordHash;
use argon2::password_hash::{PasswordHasher, PasswordVerifier};
use axum::extract::FromRequestParts;
use axum::http::request::Parts;
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use base64::Engine;
use rand::distr::{Alphanumeric, SampleString};
use rusqlite::{OptionalExtension, params};
use sha2::{Digest, Sha256};

use crate::AppState;
use crate::db::now;
use crate::json::PhpJson;
use crate::security::{TooManyAttempts, client_ip, peer_addr};

pub const SESSION_COOKIE: &str = "ncgl_session";
pub const SESSION_LIFETIME_SECS: i64 = 30 * 24 * 60 * 60;

#[derive(Clone, Debug)]
pub struct AuthUser {
    pub id: i64,
    pub username: String,
}

pub fn hash_password(password: &str) -> String {
    Argon2::default()
        .hash_password(password.as_bytes())
        .expect("argon2 hashing with default params cannot fail")
        .to_string()
}

fn verify_password(password: &str, hash: &str) -> bool {
    PasswordHash::new(hash)
        .map(|parsed| {
            Argon2::default()
                .verify_password(password.as_bytes(), &parsed)
                .is_ok()
        })
        .unwrap_or(false)
}

pub fn random_token(len: usize) -> String {
    Alphanumeric.sample_string(&mut rand::rng(), len)
}

pub fn sha256_hex(input: &str) -> String {
    Sha256::digest(input.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// Which secrets a login form or endpoint accepts.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Credentials {
    /// The account password or an app password. Used by the sync API.
    AnyPassword,
    /// Only the account password. App passwords are meant for a single device and
    /// must not unlock the web UI, issue new app passwords, or change the password.
    AccountPasswordOnly,
}

pub enum LoginError {
    Invalid,
    Throttled,
}

/// Argon2 hash checked when the username doesn't exist, so the response takes as
/// long as for a wrong password and doesn't reveal which usernames exist.
fn dummy_hash() -> &'static str {
    static HASH: OnceLock<String> = OnceLock::new();
    HASH.get_or_init(|| hash_password("dummy password for timing equalization"))
}

/// Checks a username and password. Failed account-password attempts are counted
/// per username and client IP and delayed; once too many accumulate, further
/// attempts fail with [`LoginError::Throttled`] without checking the password.
pub async fn authenticate(
    state: &AppState,
    client_ip: Option<IpAddr>,
    username: &str,
    password: &str,
    accepted: Credentials,
) -> Result<AuthUser, LoginError> {
    let token_hash = sha256_hex(password);
    let lookup = state.db.with(|conn| -> rusqlite::Result<_> {
        let Some((id, password_hash)) = conn
            .query_row(
                "SELECT id, password_hash FROM users WHERE username = ?1",
                [username],
                |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)),
            )
            .optional()?
        else {
            return Ok(None);
        };
        let app_password_used = accepted == Credentials::AnyPassword
            && conn.execute(
                "UPDATE app_passwords SET last_used = ?1 WHERE user_id = ?2 AND token_hash = ?3",
                params![now(), id, token_hash],
            )? > 0;
        Ok(Some((id, password_hash, app_password_used)))
    });
    let found = lookup.unwrap_or_else(|err| {
        tracing::error!("database error during login: {err}");
        None
    });
    if let Some((id, _, true)) = found {
        return Ok(AuthUser {
            id,
            username: username.to_owned(),
        });
    }

    if !state.login_throttle.begin_attempt(username, client_ip) {
        return Err(LoginError::Throttled);
    }
    let hash = found
        .as_ref()
        .map_or_else(|| dummy_hash().to_owned(), |(_, hash, _)| hash.clone());
    let password = password.to_owned();
    let valid = tokio::task::spawn_blocking(move || verify_password(&password, &hash))
        .await
        .unwrap_or(false);
    match found {
        Some((id, _, _)) if valid => {
            state.login_throttle.record_success(username, client_ip);
            Ok(AuthUser {
                id,
                username: username.to_owned(),
            })
        }
        _ => {
            // Slow down password guessing, like Nextcloud's brute-force protection.
            tokio::time::sleep(Duration::from_secs(1)).await;
            Err(LoginError::Invalid)
        }
    }
}

fn basic_credentials(headers: &HeaderMap) -> Option<(String, String)> {
    let value = headers.get(header::AUTHORIZATION)?.to_str().ok()?;
    let (scheme, encoded) = value.split_once(' ')?;
    if !scheme.eq_ignore_ascii_case("basic") {
        return None;
    }
    let decoded = base64::engine::general_purpose::STANDARD
        .decode(encoded.trim())
        .ok()?;
    let decoded = String::from_utf8(decoded).ok()?;
    let (user, pass) = decoded.split_once(':')?;
    Some((user.to_owned(), pass.to_owned()))
}

pub fn cookie<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(';'))
        .filter_map(|pair| pair.trim().split_once('='))
        .find(|(k, _)| *k == name)
        .map(|(_, v)| v)
}

/// The user owning the session cookie on this request, if any.
pub fn session_user(state: &AppState, headers: &HeaderMap) -> Option<AuthUser> {
    let token = cookie(headers, SESSION_COOKIE)?;
    state
        .db
        .with(|conn| {
            conn.query_row(
                "SELECT users.id, users.username FROM sessions
                 JOIN users ON users.id = sessions.user_id
                 WHERE sessions.token_hash = ?1 AND sessions.created_at >= ?2",
                rusqlite::params![sha256_hex(token), now() - SESSION_LIFETIME_SECS],
                |r| {
                    Ok(AuthUser {
                        id: r.get(0)?,
                        username: r.get(1)?,
                    })
                },
            )
            .optional()
        })
        .ok()
        .flatten()
}

/// Rejection for API routes, matching Nextcloud's response for unauthenticated requests.
pub struct NotLoggedIn;

impl IntoResponse for NotLoggedIn {
    fn into_response(self) -> Response {
        let mut res = PhpJson(
            StatusCode::UNAUTHORIZED,
            serde_json::json!({ "message": "Current user is not logged in" }),
        )
        .into_response();
        res.headers_mut().insert(
            header::WWW_AUTHENTICATE,
            header::HeaderValue::from_static("Basic realm=\"Nextcloud\", charset=\"UTF-8\""),
        );
        res
    }
}

impl FromRequestParts<Arc<AppState>> for AuthUser {
    type Rejection = Response;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &Arc<AppState>,
    ) -> Result<Self, Self::Rejection> {
        if let Some((username, password)) = basic_credentials(&parts.headers) {
            let ip = client_ip(state, &parts.headers, peer_addr(&parts.extensions));
            return match authenticate(state, ip, &username, &password, Credentials::AnyPassword)
                .await
            {
                Ok(user) => Ok(user),
                Err(LoginError::Invalid) => Err(NotLoggedIn.into_response()),
                Err(LoginError::Throttled) => Err(TooManyAttempts.into_response()),
            };
        }
        session_user(state, &parts.headers).ok_or_else(|| NotLoggedIn.into_response())
    }
}
