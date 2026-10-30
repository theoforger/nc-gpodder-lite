//! `subscriptions` / `subscription_change` endpoints, mirroring nextcloud-gpodder's
//! `SubscriptionChangeController` and `SubscriptionChangeSaver`.

use std::collections::HashMap;
use std::sync::Arc;

use axum::body::Bytes;
use axum::extract::{Query, State};
use rusqlite::{Connection, params};
use serde_json::{Value, json};

use super::int_param;
use crate::AppState;
use crate::auth::AuthUser;
use crate::db::now;
use crate::json::{ApiError, PhpJson};

pub async fn list(
    user: AuthUser,
    State(state): State<Arc<AppState>>,
    Query(query): Query<HashMap<String, String>>,
) -> Result<PhpJson, ApiError> {
    let since = int_param(&query, "since", 0)?;
    let (add, remove) = state.db.with(|conn| -> rusqlite::Result<_> {
        Ok((
            urls_with_state(conn, &user.username, true, since)?,
            urls_with_state(conn, &user.username, false, since)?,
        ))
    })?;
    Ok(PhpJson::ok(json!({
        "add": add,
        "remove": remove,
        "timestamp": now(),
    })))
}

pub fn urls_with_state(
    conn: &Connection,
    user_id: &str,
    subscribed: bool,
    since: i64,
) -> rusqlite::Result<Vec<String>> {
    let mut stmt = conn.prepare(
        "SELECT url FROM gpodder_subscriptions
         WHERE subscribed = ?1 AND updated > ?2 AND user_id = ?3
         ORDER BY id",
    )?;
    stmt.query_map(params![subscribed, since, user_id], |r| r.get(0))?
        .collect()
}

pub async fn create(
    user: AuthUser,
    State(state): State<Arc<AppState>>,
    body: Bytes,
) -> Result<PhpJson, ApiError> {
    // `create(array $add, array $remove)`: both parameters are required arrays of
    // strings; a missing or mistyped value is a TypeError (HTTP 500) upstream.
    let params: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
    let add = url_list(&params, "add")?;
    let remove = url_list(&params, "remove")?;

    state.db.with(|conn| -> rusqlite::Result<()> {
        let tx = conn.transaction()?;
        save_changes(&tx, &user.username, &add, &remove)?;
        tx.commit()
    })?;

    Ok(PhpJson::ok(json!({ "timestamp": now() })))
}

fn url_list(params: &Value, name: &str) -> Result<Vec<String>, ApiError> {
    let items: Vec<&Value> = match params.get(name) {
        Some(Value::Array(items)) => items.iter().collect(),
        // PHP decodes JSON objects into arrays too.
        Some(Value::Object(map)) => map.values().collect(),
        _ => {
            return Err(ApiError::internal(format!(
                "Parameter '{name}' must be an array"
            )));
        }
    };
    items
        .into_iter()
        .map(|v| {
            v.as_str()
                .map(str::to_owned)
                .ok_or_else(|| ApiError::internal(format!("Entries of '{name}' must be strings")))
        })
        .collect()
}

/// Writes adds first, then removes, so a URL in both lists ends up unsubscribed.
/// Invalid URLs are silently dropped.
pub fn save_changes(
    conn: &Connection,
    user_id: &str,
    add: &[String],
    remove: &[String],
) -> rusqlite::Result<()> {
    let updated = now();
    let changes = add
        .iter()
        .map(|u| (u, true))
        .chain(remove.iter().map(|u| (u, false)));
    for (url, subscribed) in changes {
        if !is_valid_url(url) {
            continue;
        }
        conn.execute(
            "INSERT INTO gpodder_subscriptions (url, subscribed, updated, user_id)
             VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT (url, user_id)
             DO UPDATE SET subscribed = excluded.subscribed, updated = excluded.updated",
            params![url, subscribed, updated, user_id],
        )?;
    }
    Ok(())
}

/// Approximates PHP's `filter_var($url, FILTER_VALIDATE_URL)`.
pub fn is_valid_url(input: &str) -> bool {
    // Only printable ASCII without spaces survives PHP's URL sanitizer check.
    if input.is_empty() || !input.bytes().all(|b| (0x21..=0x7e).contains(&b)) {
        return false;
    }
    let Some((scheme, rest)) = input.split_once(':') else {
        return false;
    };
    let mut scheme_chars = scheme.chars();
    if !scheme_chars.next().is_some_and(|c| c.is_ascii_alphabetic())
        || !scheme_chars.all(|c| c.is_ascii_alphanumeric() || "+-.".contains(c))
    {
        return false;
    }
    let scheme = scheme.to_ascii_lowercase();
    if matches!(scheme.as_str(), "mailto" | "news" | "file") {
        return !rest.is_empty();
    }
    // Every other scheme needs an authority with a host.
    let Some(authority) = rest.strip_prefix("//") else {
        return false;
    };
    let Ok(url) = url::Url::parse(input) else {
        return false;
    };
    if matches!(scheme.as_str(), "http" | "https") {
        let host_part = authority
            .split(['/', '?', '#'])
            .next()
            .unwrap_or_default()
            .rsplit('@')
            .next()
            .unwrap_or_default();
        return match url.host() {
            Some(url::Host::Domain(_)) => is_valid_domain(host_part),
            Some(_) => true,
            None => false,
        };
    }
    url.host_str().is_some_and(|h| !h.is_empty())
}

/// PHP's host check for http(s) URLs: dot-separated alphanumeric labels that may
/// contain but not start or end with `-`, plus an optional port.
fn is_valid_domain(host_and_port: &str) -> bool {
    let host = match host_and_port.rsplit_once(':') {
        Some((host, port)) if port.bytes().all(|b| b.is_ascii_digit()) => host,
        Some(_) => return false,
        None => host_and_port,
    };
    let host = host.strip_suffix('.').unwrap_or(host);
    !host.is_empty()
        && host.len() <= 253
        && host.split('.').all(|label| {
            !label.is_empty()
                && label.len() <= 63
                && !label.starts_with('-')
                && !label.ends_with('-')
                && label
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'-')
        })
}

#[cfg(test)]
mod tests {
    use super::is_valid_url;

    #[test]
    fn validates_urls_like_php() {
        assert!(is_valid_url("https://example.com/feed.xml"));
        assert!(is_valid_url("http://user:pw@example.com:8080/a?b=c#d"));
        assert!(is_valid_url("http://127.0.0.1/feed"));
        assert!(is_valid_url("http://[::1]/feed"));
        assert!(is_valid_url("ftp://example.com/x"));
        assert!(!is_valid_url("example.com/feed"));
        assert!(!is_valid_url("https://exa mple.com"));
        assert!(!is_valid_url("https://exämple.com"));
        assert!(!is_valid_url("http:/example.com"));
        assert!(!is_valid_url("https://exa_mple.com"));
        assert!(!is_valid_url("https://-example.com"));
        assert!(!is_valid_url("https://"));
    }
}
