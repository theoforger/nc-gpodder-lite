//! `episode_action` endpoints, mirroring nextcloud-gpodder's
//! `EpisodeActionController`, `EpisodeActionReader` and `EpisodeActionSaver`.

use std::collections::HashMap;
use std::sync::Arc;

use axum::body::Bytes;
use axum::extract::{Query, State};
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::{Map, Value, json};

use super::{bool_param, int_param, is_numeric};
use crate::AppState;
use crate::auth::AuthUser;
use crate::db::{is_unique_violation, now};
use crate::json::{ApiError, PhpJson};
use crate::time::{format_timestamp, parse_timestamp};

pub async fn list(
    user: AuthUser,
    State(state): State<Arc<AppState>>,
    Query(query): Query<HashMap<String, String>>,
) -> Result<PhpJson, ApiError> {
    let since = int_param(&query, "since", 0)?;
    let all_actions = bool_param(&query, "allActions");

    let actions = state.db.with(|conn| -> rusqlite::Result<Vec<Value>> {
        let mut stmt = conn.prepare(
            "SELECT podcast, episode, timestamp_epoch, guid, position, started, total, action
             FROM gpodder_episode_action
             WHERE timestamp_epoch > ?1 AND user_id = ?2
             ORDER BY id",
        )?;
        let rows = stmt.query_map(params![since, user.username], |r| {
            Ok((
                json!({
                    "podcast": r.get::<_, String>(0)?,
                    "episode": r.get::<_, String>(1)?,
                    "timestamp": format_timestamp(r.get(2)?),
                    "guid": r.get::<_, Option<String>>(3)?,
                    "position": r.get::<_, i64>(4)?,
                    "started": r.get::<_, i64>(5)?,
                    "total": r.get::<_, i64>(6)?,
                    "action": r.get::<_, String>(7)?,
                }),
                r.get::<_, String>(7)?,
            ))
        })?;
        let mut actions = Vec::new();
        for row in rows {
            let (action, name) = row?;
            if all_actions || name.eq_ignore_ascii_case("play") {
                actions.push(action);
            }
        }
        Ok(actions)
    })?;

    Ok(PhpJson::ok(
        json!({ "actions": actions, "timestamp": now() }),
    ))
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
        .collect::<Result<Vec<_>, _>>()?;

    state.db.with(|conn| -> rusqlite::Result<()> {
        let tx = conn.transaction()?;
        for action in &actions {
            save(&tx, &user.username, action)?;
        }
        tx.commit()
    })?;

    Ok(PhpJson::ok(json!({ "timestamp": now() })))
}

#[derive(Debug)]
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

/// Insert-or-update, following upstream `EpisodeActionSaver::saveEpisodeActions`.
fn save(conn: &Connection, user_id: &str, a: &EpisodeAction) -> rusqlite::Result<()> {
    let inserted = conn.execute(
        "INSERT INTO gpodder_episode_action
            (podcast, episode, action, position, started, total, timestamp_epoch, guid, user_id)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
        params![
            a.podcast,
            a.episode,
            a.action,
            a.position,
            a.started,
            a.total,
            a.timestamp_epoch,
            a.guid,
            user_id
        ],
    );
    match inserted {
        Ok(_) => Ok(()),
        Err(err) if is_unique_violation(&err) => update_existing(conn, user_id, a),
        // Upstream swallows any other database error for an individual action.
        Err(err) => {
            tracing::warn!("failed to save episode action {:?}: {err}", a.episode);
            Ok(())
        }
    }
}

fn update_existing(conn: &Connection, user_id: &str, a: &EpisodeAction) -> rusqlite::Result<()> {
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
        return Ok(());
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
            }
            Ok(())
        }
        other => other.map(|_| ()),
    }
}

fn find_by(
    conn: &Connection,
    column: &str,
    value: &str,
    user_id: &str,
    action: &str,
) -> rusqlite::Result<Option<(i64, Option<String>)>> {
    conn.query_row(
        &format!(
            "SELECT id, guid FROM gpodder_episode_action
             WHERE {column} = ?1 AND user_id = ?2 AND action = ?3"
        ),
        params![value, user_id, action],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )
    .optional()
}

fn update_row(
    conn: &Connection,
    id: i64,
    user_id: &str,
    a: &EpisodeAction,
    guid: &Option<String>,
) -> rusqlite::Result<usize> {
    conn.execute(
        "UPDATE gpodder_episode_action
         SET podcast = ?1, episode = ?2, action = ?3, position = ?4, started = ?5, total = ?6,
             timestamp_epoch = ?7, guid = ?8, user_id = ?9
         WHERE id = ?10",
        params![
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
        ],
    )
}
