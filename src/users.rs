//! User and app-password management shared by the CLI and the web UI.

use rusqlite::{Connection, OptionalExtension, params};

use crate::auth::{hash_password, random_token, sha256_hex};
use crate::db::now;

pub struct AppPassword {
    pub id: i64,
    pub name: String,
    pub created_at: i64,
    pub last_used: Option<i64>,
}

pub fn create_user(conn: &Connection, username: &str, password: &str) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT INTO users (username, password_hash, created_at) VALUES (?1, ?2, ?3)",
        params![username, hash_password(password), now()],
    )?;
    Ok(())
}

/// Returns false if the user does not exist.
pub fn set_password(conn: &Connection, username: &str, password: &str) -> rusqlite::Result<bool> {
    Ok(conn.execute(
        "UPDATE users SET password_hash = ?1 WHERE username = ?2",
        params![hash_password(password), username],
    )? > 0)
}

/// Deletes the user, their app passwords and sessions, and all their sync data.
pub fn delete_user(conn: &mut Connection, username: &str) -> rusqlite::Result<bool> {
    let tx = conn.transaction()?;
    tx.execute(
        "DELETE FROM gpodder_episode_action WHERE user_id = ?1",
        [username],
    )?;
    tx.execute(
        "DELETE FROM gpodder_subscriptions WHERE user_id = ?1",
        [username],
    )?;
    let deleted = tx.execute("DELETE FROM users WHERE username = ?1", [username])? > 0;
    tx.commit()?;
    Ok(deleted)
}

pub fn list_users(conn: &Connection) -> rusqlite::Result<Vec<String>> {
    let mut stmt = conn.prepare("SELECT username FROM users ORDER BY username")?;
    stmt.query_map([], |r| r.get(0))?.collect()
}

pub fn user_id(conn: &Connection, username: &str) -> rusqlite::Result<Option<i64>> {
    conn.query_row(
        "SELECT id FROM users WHERE username = ?1",
        [username],
        |r| r.get(0),
    )
    .optional()
}

/// Creates an app password and returns it in plain text; only its hash is stored.
/// Format matches Nextcloud: 72 random alphanumeric characters.
pub fn create_app_password(
    conn: &Connection,
    user_id: i64,
    name: &str,
) -> rusqlite::Result<String> {
    let token = random_token(72);
    conn.execute(
        "INSERT INTO app_passwords (user_id, name, token_hash, created_at) VALUES (?1, ?2, ?3, ?4)",
        params![user_id, name, sha256_hex(&token), now()],
    )?;
    Ok(token)
}

pub fn list_app_passwords(conn: &Connection, user_id: i64) -> rusqlite::Result<Vec<AppPassword>> {
    let mut stmt = conn.prepare(
        "SELECT id, name, created_at, last_used FROM app_passwords WHERE user_id = ?1 ORDER BY id",
    )?;
    stmt.query_map([user_id], |r| {
        Ok(AppPassword {
            id: r.get(0)?,
            name: r.get(1)?,
            created_at: r.get(2)?,
            last_used: r.get(3)?,
        })
    })?
    .collect()
}

pub fn delete_app_password(conn: &Connection, user_id: i64, id: i64) -> rusqlite::Result<()> {
    conn.execute(
        "DELETE FROM app_passwords WHERE id = ?1 AND user_id = ?2",
        params![id, user_id],
    )?;
    Ok(())
}
