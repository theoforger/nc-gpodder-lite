pub mod auth;
pub mod db;
pub mod json;
pub mod podcast_data;
pub mod routes;
pub mod security;
pub mod time;
pub mod users;

use std::sync::Arc;

use axum::Router;
use axum::http::HeaderMap;

use crate::auth::VerifiedPasswords;
use crate::db::Db;
use crate::podcast_data::PodcastDataReader;
use crate::security::{IpNet, LoginThrottle};

pub struct Config {
    /// Externally visible base URL (e.g. `https://pods.example.com`), used in
    /// Login Flow v2 responses. If unset it is derived from request headers.
    pub public_url: Option<String>,
    /// Allow fetching feeds from loopback/private addresses (Nextcloud's
    /// `allow_local_remote_servers`).
    pub allow_local_remote_servers: bool,
    /// Reverse proxies whose `X-Forwarded-For` header is trusted for the client IP
    /// (used by login throttling).
    pub trusted_proxies: Vec<IpNet>,
}

pub struct AppState {
    pub db: Db,
    pub config: Config,
    pub podcast_data: PodcastDataReader,
    pub login_throttle: LoginThrottle,
    pub verified_passwords: VerifiedPasswords,
}

impl AppState {
    pub fn new(db: Db, config: Config) -> Self {
        let podcast_data = PodcastDataReader::new(config.allow_local_remote_servers);
        if let Err(err) = db.with(|conn| routes::purge_expired_login_flows(conn)) {
            tracing::error!("failed to purge expired login flows: {err}");
        }
        AppState {
            db,
            config,
            podcast_data,
            login_throttle: LoginThrottle::default(),
            verified_passwords: VerifiedPasswords::default(),
        }
    }

    /// Base URL of this server as seen by the client, without a trailing slash.
    pub fn base_url(&self, headers: &HeaderMap) -> String {
        if let Some(url) = self.config.public_url.as_deref().filter(|u| !u.is_empty()) {
            return url.trim_end_matches('/').to_owned();
        }
        let header = |name: &str| {
            headers
                .get(name)
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.split(',').next())
                .map(|v| v.trim().to_owned())
        };
        let proto = header("x-forwarded-proto").unwrap_or_else(|| "http".to_owned());
        let host = header("x-forwarded-host")
            .or_else(|| header("host"))
            .unwrap_or_else(|| "localhost".to_owned());
        format!("{proto}://{host}")
    }

    pub fn is_https(&self, headers: &HeaderMap) -> bool {
        self.base_url(headers).starts_with("https://")
    }
}

pub fn app(state: Arc<AppState>) -> Router {
    routes::router(state)
}
