//! `episode_action` endpoints, mirroring nextcloud-gpodder's
//! `EpisodeActionController`, `EpisodeActionReader` and `EpisodeActionSaver`.

use std::collections::HashMap;
use std::sync::Arc;

use axum::body::Bytes;
use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::Response;
use rusqlite::{Connection, OptionalExtension, params};
use serde::Serialize;
use serde_json::{Map, Value, json};

use super::{bool_param, int_param, is_numeric};
use crate::AppState;
use crate::auth::AuthUser;
use crate::db::{is_unique_violation, now};
use crate::json::{ApiError, PhpJson, php_json};
use crate::time::{format_timestamp, parse_timestamp};

pub async fn list(
    user: AuthUser,
    State(state): State<Arc<AppState>>,
    Query(query): Query<HashMap<String, String>>,
) -> Result<Response, ApiError> {
    let since = int_param(&query, "since", 0)?;
    let all_actions = bool_param(&query, "allActions");

    let actions = state.db.with(|conn| -> rusqlite::Result<Vec<ActionRow>> {
        // Actions are stored uppercased, so `action = 'PLAY'` matches upstream's
        // case-insensitive "play" filter.
        let mut stmt = conn.prepare_cached(
            "SELECT podcast, episode, timestamp_epoch, guid, position, started, total, action
             FROM gpodder_episode_action
             WHERE user_id = ?1 AND timestamp_epoch > ?2 AND (?3 OR action = 'PLAY')
             ORDER BY id",
        )?;
        stmt.query_map(params![user.username, since, all_actions], |r| {
            Ok(ActionRow {
                podcast: r.get(0)?,
                episode: r.get(1)?,
                timestamp: format_timestamp(r.get(2)?),
                guid: r.get(3)?,
                position: r.get(4)?,
                started: r.get(5)?,
                total: r.get(6)?,
                action: r.get(7)?,
            })
        })?
        .collect()
    })?;

    tracing::info!(
        user = %user.username,
        since,
        all_actions,
        count = actions.len(),
        "episode actions downloaded"
    );
    Ok(php_json(
        StatusCode::OK,
        &ListResponse {
            actions,
            timestamp: now(),
        },
    ))
}

/// One action as returned by `EpisodeAction::toArray()`; field order matters.
#[derive(Serialize)]
struct ActionRow {
    podcast: String,
    episode: String,
    timestamp: String,
    guid: Option<String>,
    position: i64,
    started: i64,
    total: i64,
    action: String,
}

#[derive(Serialize)]
struct ListResponse {
    actions: Vec<ActionRow>,
    timestamp: i64,
}

pub async fn create(
    user: AuthUser,
    State(state): State<Arc<AppState>>,
    body: Bytes,
) -> Result<PhpJson, ApiError> {
    // Upstream reads `$request->getParams()` and keeps only numeric keys, so a JSON
    // list (or an object with numeric keys) carries the actions; anything else is
    // silently ignored.
    let entries: Vec<Value> = match serde_json::from_slice::<Value>(&body) {
        Ok(Value::Array(items)) => items,
        Ok(Value::Object(map)) => map
            .into_iter()
            .filter(|(k, _)| is_numeric(k))
            .map(|(_, v)| v)
            .collect(),
        _ => Vec::new(),
    };
    let actions = entries
        .iter()
        .map(EpisodeAction::from_value)
        .collect::<Result<Vec<_>, _>>()
        .inspect_err(|err| {
            tracing::warn!(user = %user.username, error = %err.1, "episode action upload rejected");
        })?;

    let (mut inserted, mut updated, mut skipped) = (0, 0, 0);
    state.db.with(|conn| -> rusqlite::Result<()> {
        let tx = conn.transaction()?;
        for action in &actions {
            match save(&tx, &user.username, action)? {
                Saved::Inserted => inserted += 1,
                Saved::Updated => updated += 1,
                Saved::Skipped => skipped += 1,
            }
        }
        tx.commit()
    })?;

    tracing::info!(
        user = %user.username,
        received = actions.len(),
        inserted,
        updated,
        skipped,
        "episode actions uploaded"
    );
    Ok(PhpJson::ok(json!({ "timestamp": now() })))
}

struct EpisodeAction {
    podcast: String,
    episode: String,
    action: String,
    timestamp_epoch: i64,
    started: i64,
    position: i64,
    total: i64,
    guid: Option<String>,
}

impl EpisodeAction {
    /// Validates one uploaded action like upstream's `EpisodeActionReader::fromArray`
    /// (with `strict_types`), where any failure is an uncaught exception (HTTP 500).
    fn from_value(value: &Value) -> Result<Self, ApiError> {
        let invalid =
            || ApiError::internal(format!("Client sent incomplete or invalid data: {value}"));
        let obj = value.as_object().ok_or_else(invalid)?;
        if ["podcast", "episode", "action", "timestamp"]
            .iter()
            .any(|k| !obj.contains_key(*k))
        {
            return Err(invalid());
        }
        let timestamp = required_str(obj, "timestamp")?;
        Ok(EpisodeAction {
            podcast: required_str(obj, "podcast")?,
            episode: required_str(obj, "episode")?,
            action: required_str(obj, "action")?.to_ascii_uppercase(),
            timestamp_epoch: parse_timestamp(&timestamp).ok_or_else(|| {
                ApiError::internal(format!("Failed to parse time string ({timestamp})"))
            })?,
            started: optional_int(obj, "started")?,
            position: optional_int(obj, "position")?,
            total: optional_int(obj, "total")?,
            guid: match obj.get("guid") {
                None | Some(Value::Null) => None,
                Some(Value::String(s)) => Some(s.clone()),
                Some(_) => return Err(type_error("guid", "?string")),
            },
        })
    }
}

fn type_error(field: &str, expected: &str) -> ApiError {
    ApiError::internal(format!("Field '{field}' must be of type {expected}"))
}

fn required_str(obj: &Map<String, Value>, key: &str) -> Result<String, ApiError> {
    match obj.get(key) {
        Some(Value::String(s)) => Ok(s.clone()),
        _ => Err(type_error(key, "string")),
    }
}

/// `$episodeAction[$key] ?? -1`, which must then be an int.
fn optional_int(obj: &Map<String, Value>, key: &str) -> Result<i64, ApiError> {
    match obj.get(key) {
        None | Some(Value::Null) => Ok(-1),
        Some(v) => v.as_i64().ok_or_else(|| type_error(key, "int")),
    }
}

/// What happened to one uploaded action.
#[derive(Debug)]
enum Saved {
    Inserted,
    Updated,
    /// Dropped after a database error, like upstream does.
    Skipped,
}

/// Insert-or-update, following upstream `EpisodeActionSaver::saveEpisodeActions`.
fn save(conn: &Connection, user_id: &str, a: &EpisodeAction) -> rusqlite::Result<Saved> {
    let inserted = conn
        .prepare_cached(
            "INSERT INTO gpodder_episode_action
                (podcast, episode, action, position, started, total, timestamp_epoch, guid, user_id)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
        )?
        .execute(params![
            a.podcast,
            a.episode,
            a.action,
            a.position,
            a.started,
            a.total,
            a.timestamp_epoch,
            a.guid,
            user_id
        ]);
    let saved = match inserted {
        Ok(_) => Saved::Inserted,
        Err(err) if is_unique_violation(&err) => update_existing(conn, user_id, a)?,
        // Upstream swallows any other database error for an individual action.
        Err(err) => {
            tracing::warn!("failed to save episode action {:?}: {err}", a.episode);
            Saved::Skipped
        }
    };
    tracing::debug!(
        user = user_id,
        episode = a.episode,
        action = a.action,
        outcome = ?saved,
        "episode action saved"
    );
    Ok(saved)
}

fn update_existing(conn: &Connection, user_id: &str, a: &EpisodeAction) -> rusqlite::Result<Saved> {
    let by_guid = match &a.guid {
        Some(guid) => find_by(conn, "guid", guid, user_id, &a.action)?,
        None => None,
    };
    let existing = match by_guid {
        Some(row) => Some(row),
        None => find_by(conn, "episode", &a.episode, user_id, &a.action)?,
    };
    // Upstream dereferences null here and fails the whole request; skip instead.
    let Some((id, existing_guid)) = existing else {
        tracing::warn!("no conflicting episode action found for {:?}", a.episode);
        return Ok(Saved::Skipped);
    };
    // An incoming null guid never replaces a stored one.
    let guid = a.guid.clone().or(existing_guid);

    match update_row(conn, id, user_id, a, &guid) {
        Err(err) if is_unique_violation(&err) => {
            if let Some((colliding_id, _)) =
                find_by(conn, "episode", &a.episode, user_id, &a.action)?
                && colliding_id != id
            {
                conn.execute(
                    "DELETE FROM gpodder_episode_action WHERE id = ?1",
                    [colliding_id],
                )?;
            }
            if let Err(err) = update_row(conn, id, user_id, a, &guid) {
                tracing::warn!("failed to update episode action {:?}: {err}", a.episode);
                return Ok(Saved::Skipped);
            }
            Ok(Saved::Updated)
        }
        other => other.map(|_| Saved::Updated),
    }
}

fn find_by(
    conn: &Connection,
    column: &str,
    value: &str,
    user_id: &str,
    action: &str,
) -> rusqlite::Result<Option<(i64, Option<String>)>> {
    conn.prepare_cached(&format!(
        "SELECT id, guid FROM gpodder_episode_action
         WHERE {column} = ?1 AND user_id = ?2 AND action = ?3"
    ))?
    .query_row(params![value, user_id, action], |r| {
        Ok((r.get(0)?, r.get(1)?))
    })
    .optional()
}

fn update_row(
    conn: &Connection,
    id: i64,
    user_id: &str,
    a: &EpisodeAction,
    guid: &Option<String>,
) -> rusqlite::Result<usize> {
    conn.prepare_cached(
        "UPDATE gpodder_episode_action
         SET podcast = ?1, episode = ?2, action = ?3, position = ?4, started = ?5, total = ?6,
             timestamp_epoch = ?7, guid = ?8, user_id = ?9
         WHERE id = ?10",
    )?
    .execute(params![
        a.podcast,
        a.episode,
        a.action,
        a.position,
        a.started,
        a.total,
        a.timestamp_epoch,
        guid,
        user_id,
        id
    ])
}
