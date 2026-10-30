//! `personal_settings` endpoints, mirroring nextcloud-gpodder's
//! `PersonalSettingsController`, `PodcastMetricsReader` and `PodcastDataReader`.

use std::collections::HashMap;
use std::sync::Arc;

use axum::extract::{Query, State};
use axum::http::StatusCode;
use rusqlite::{Connection, OptionalExtension, params};
use serde::Serialize;
use serde_json::json;

use super::subscription::urls_with_state;
use crate::AppState;
use crate::auth::AuthUser;
use crate::json::{ApiError, PhpJson};

#[derive(Serialize, Default, Clone)]
pub struct ActionCounts {
    pub delete: i64,
    pub download: i64,
    pub flattr: i64,
    pub new: i64,
    pub play: i64,
}

#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct PodcastMetrics {
    pub url: String,
    pub listened_seconds: i64,
    pub action_counts: ActionCounts,
}

/// Per-podcast metrics for every currently subscribed URL.
pub fn podcast_metrics(conn: &Connection, user_id: &str) -> rusqlite::Result<Vec<PodcastMetrics>> {
    let mut per_url: HashMap<String, PodcastMetrics> = HashMap::new();
    let mut stmt = conn.prepare(
        "SELECT podcast, action, position FROM gpodder_episode_action
         WHERE timestamp_epoch > 0 AND user_id = ?1 ORDER BY id",
    )?;
    let rows = stmt.query_map([user_id], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, i64>(2)?,
        ))
    })?;
    for row in rows {
        let (podcast, action, position) = row?;
        let metrics = per_url
            .entry(podcast.clone())
            .or_insert_with(|| empty_metrics(&podcast));
        let counts = &mut metrics.action_counts;
        match action.to_ascii_lowercase().as_str() {
            "delete" => counts.delete += 1,
            "download" => counts.download += 1,
            "flattr" => counts.flattr += 1,
            "new" => counts.new += 1,
            "play" => {
                counts.play += 1;
                if position != 0 && position != -1 {
                    metrics.listened_seconds += position;
                }
            }
            _ => {}
        }
    }

    Ok(urls_with_state(conn, user_id, true, 0)?
        .into_iter()
        .map(|url| {
            per_url
                .get(&url)
                .cloned()
                .unwrap_or_else(|| empty_metrics(&url))
        })
        .collect())
}

fn empty_metrics(url: &str) -> PodcastMetrics {
    PodcastMetrics {
        url: url.to_owned(),
        listened_seconds: 0,
        action_counts: ActionCounts::default(),
    }
}

pub async fn metrics(
    user: AuthUser,
    State(state): State<Arc<AppState>>,
) -> Result<PhpJson, ApiError> {
    let metrics = state
        .db
        .with(|conn| podcast_metrics(conn, &user.username))?;
    Ok(PhpJson::ok(json!({ "subscriptions": metrics })))
}

pub async fn podcast_data(
    user: AuthUser,
    State(state): State<Arc<AppState>>,
    Query(query): Query<HashMap<String, String>>,
) -> Result<PhpJson, ApiError> {
    let url = query.get("url").map(String::as_str).unwrap_or_default();
    if url.is_empty() {
        return Ok(PhpJson(
            StatusCode::BAD_REQUEST,
            json!({ "message": "Missing query parameter 'url'.", "data": null }),
        ));
    }

    // Upstream returns null for podcasts the user has never subscribed to.
    // Unlike upstream, the check also applies to cached data.
    let known = state.db.with(|conn| {
        conn.query_row(
            "SELECT 1 FROM gpodder_subscriptions WHERE url = ?1 AND user_id = ?2",
            params![url, user.username],
            |_| Ok(()),
        )
        .optional()
    })?;
    if known.is_none() {
        return Ok(PhpJson::ok(json!({ "data": null })));
    }

    let data = state
        .podcast_data
        .get_cached_or_fetch(url)
        .await
        .map_err(|err| {
            // Details stay in the log: they can reveal how internal hosts resolve.
            tracing::warn!("failed to fetch podcast data for {url}: {err}");
            ApiError::internal("Failed to fetch podcast data")
        })?;
    Ok(PhpJson::ok(json!({ "data": data })))
}
