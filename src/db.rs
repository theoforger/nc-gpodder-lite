use std::path::Path;
use std::sync::{Mutex, PoisonError};

use rusqlite::Connection;

/// Schema migrations, applied in order. `PRAGMA user_version` records how many have run.
const MIGRATIONS: &[&str] = &[
    r#"
CREATE TABLE users (
    id            INTEGER PRIMARY KEY AUTOINCREMENT,
    username      TEXT NOT NULL UNIQUE,
    password_hash TEXT NOT NULL,
    created_at    INTEGER NOT NULL
);

CREATE TABLE app_passwords (
    id         INTEGER PRIMARY KEY AUTOINCREMENT,
    user_id    INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    name       TEXT NOT NULL,
    token_hash TEXT NOT NULL UNIQUE,
    created_at INTEGER NOT NULL,
    last_used  INTEGER
);

CREATE TABLE sessions (
    token_hash TEXT PRIMARY KEY,
    user_id    INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    created_at INTEGER NOT NULL
);

CREATE TABLE login_flows (
    poll_token   TEXT PRIMARY KEY,
    flow_token   TEXT NOT NULL UNIQUE,
    client_name  TEXT NOT NULL,
    created_at   INTEGER NOT NULL,
    login_name   TEXT,
    app_password TEXT
);

-- Mirrors nextcloud-gpodder's gpodder_episode_action after migration Version0008.
CREATE TABLE gpodder_episode_action (
    id              INTEGER PRIMARY KEY AUTOINCREMENT,
    podcast         TEXT NOT NULL,
    episode         TEXT NOT NULL,
    action          TEXT NOT NULL,
    position        INTEGER NOT NULL,
    started         INTEGER NOT NULL,
    total           INTEGER NOT NULL,
    timestamp_epoch INTEGER NOT NULL DEFAULT 0,
    guid            TEXT,
    user_id         TEXT NOT NULL
);
CREATE UNIQUE INDEX gpodder_episode_action_user ON gpodder_episode_action (episode, action, user_id);
CREATE UNIQUE INDEX gpodder_guid_action_user ON gpodder_episode_action (guid, action, user_id);

-- Mirrors nextcloud-gpodder's gpodder_subscriptions; `updated` is a UTC unix epoch.
CREATE TABLE gpodder_subscriptions (
    id         INTEGER PRIMARY KEY AUTOINCREMENT,
    url        TEXT NOT NULL,
    subscribed INTEGER,
    updated    INTEGER NOT NULL,
    user_id    TEXT NOT NULL
);
CREATE UNIQUE INDEX subscriptions_url_user ON gpodder_subscriptions (url, user_id);
"#,
    // Every sync query filters by user first; without these it scans all users' rows.
    r#"
CREATE INDEX gpodder_episode_action_user_time ON gpodder_episode_action (user_id, timestamp_epoch);
CREATE INDEX gpodder_subscriptions_user_updated ON gpodder_subscriptions (user_id, updated);
"#,
];

pub struct Db(Mutex<Connection>);

impl Db {
    pub fn open(path: &Path) -> rusqlite::Result<Self> {
        Self::init(Connection::open(path)?)
    }

    pub fn open_in_memory() -> rusqlite::Result<Self> {
        Self::init(Connection::open_in_memory()?)
    }

    fn init(mut conn: Connection) -> rusqlite::Result<Self> {
        conn.execute_batch(
            "PRAGMA journal_mode = WAL; PRAGMA foreign_keys = ON; PRAGMA busy_timeout = 5000;",
        )?;
        let version: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
        for (i, migration) in MIGRATIONS.iter().enumerate().skip(version as usize) {
            let tx = conn.transaction()?;
            tx.execute_batch(migration)?;
            tx.pragma_update(None, "user_version", (i + 1) as i64)?;
            tx.commit()?;
        }
        Ok(Db(Mutex::new(conn)))
    }

    /// Runs `f` with exclusive access to the connection. Never hold this across an `.await`.
    pub fn with<T>(&self, f: impl FnOnce(&mut Connection) -> T) -> T {
        let mut conn = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        f(&mut conn)
    }
}

pub fn now() -> i64 {
    chrono::Utc::now().timestamp()
}

pub fn is_unique_violation(err: &rusqlite::Error) -> bool {
    matches!(
        err,
        rusqlite::Error::SqliteFailure(e, _)
            if e.extended_code == rusqlite::ffi::SQLITE_CONSTRAINT_UNIQUE
    )
}
