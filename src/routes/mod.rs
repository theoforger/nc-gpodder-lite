mod dashboard;
mod episode_action;
mod login_flow;
mod personal_settings;
mod status;
mod subscription;
mod web;

pub use login_flow::purge_expired_login_flows;

use std::collections::HashMap;
use std::sync::Arc;

use axum::extract::DefaultBodyLimit;
use axum::routing::{get, post};
use axum::{Router, middleware};
use tower_http::CompressionLevel;
use tower_http::compression::CompressionLayer;
use tower_http::trace::TraceLayer;

use crate::json::ApiError;
use crate::{AppState, security};

pub fn router(state: Arc<AppState>) -> Router {
    // Nextcloud serves everything both with and without the `/index.php` prefix.
    let nextcloud = Router::new()
        .route(
            "/apps/gpoddersync/episode_action",
            get(episode_action::list),
        )
        .route(
            "/apps/gpoddersync/episode_action/create",
            post(episode_action::create).layer(DefaultBodyLimit::max(SYNC_UPLOAD_LIMIT)),
        )
        .route("/apps/gpoddersync/subscriptions", get(subscription::list))
        .route(
            "/apps/gpoddersync/subscription_change/create",
            post(subscription::create).layer(DefaultBodyLimit::max(SYNC_UPLOAD_LIMIT)),
        )
        .route(
            "/apps/gpoddersync/personal_settings/metrics",
            get(personal_settings::metrics),
        )
        .route(
            "/apps/gpoddersync/personal_settings/podcast_data",
            get(personal_settings::podcast_data),
        )
        .route("/login/v2", post(login_flow::init))
        .route("/login/v2/poll", post(login_flow::poll))
        .route("/login/v2/flow/{token}", get(login_flow::page))
        .route("/login/v2/grant", post(login_flow::grant));

    Router::new()
        .merge(nextcloud.clone())
        .nest("/index.php", nextcloud)
        .route("/status.php", get(status::status))
        .merge(web::router())
        .merge(dashboard::router())
        .layer(middleware::from_fn_with_state(
            state.clone(),
            security::csrf_protection,
        ))
        .layer(middleware::from_fn(security::security_headers))
        // Only when the client sends `Accept-Encoding: gzip`; full syncs are large
        // and compress well. The decoded responses are unchanged.
        .layer(
            CompressionLayer::new()
                .gzip(true)
                .quality(CompressionLevel::Fastest),
        )
        .layer(TraceLayer::new_for_http())
        .with_state(state)
}

/// Body limit for sync uploads, which can be large on a client's first sync.
/// Every other route keeps axum's default limit of 2 MB.
const SYNC_UPLOAD_LIMIT: usize = 64 * 1024 * 1024;

/// Reads an `int` controller parameter the way Nextcloud coerces it: numeric
/// strings are truncated to integers, anything else is rejected.
fn int_param(query: &HashMap<String, String>, name: &str, default: i64) -> Result<i64, ApiError> {
    let Some(raw) = query.get(name) else {
        return Ok(default);
    };
    let raw = raw.trim();
    if let Ok(v) = raw.parse::<i64>() {
        return Ok(v);
    }
    match raw.parse::<f64>() {
        Ok(v) if v.is_finite() && is_numeric(raw) => Ok(v as i64),
        _ => Err(ApiError::bad_request(format!(
            "Parameter '{name}' must be an integer"
        ))),
    }
}

/// Reads a `bool` controller parameter like PHP's `FILTER_VALIDATE_BOOLEAN`.
fn bool_param(query: &HashMap<String, String>, name: &str) -> bool {
    query.get(name).is_some_and(|v| {
        matches!(
            v.trim().to_ascii_lowercase().as_str(),
            "1" | "true" | "on" | "yes"
        )
    })
}

/// PHP's `is_numeric` for strings (decimal integers and floats, no hex/inf/nan).
fn is_numeric(s: &str) -> bool {
    let t = s.trim_start_matches([' ', '\t', '\n', '\r', '\x0b', '\x0c']);
    let t = t.trim_end_matches([' ', '\t', '\n', '\r', '\x0b', '\x0c']);
    !t.is_empty()
        && t.bytes()
            .all(|b| b.is_ascii_digit() || b"+-.eE".contains(&b))
        && t.parse::<f64>().is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn php_is_numeric() {
        assert!(is_numeric("0"));
        assert!(is_numeric(" 12"));
        assert!(is_numeric("-1.5e3"));
        assert!(!is_numeric("abc"));
        assert!(!is_numeric("inf"));
        assert!(!is_numeric(""));
    }
}
