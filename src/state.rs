use std::{collections::HashSet, sync::Arc, time::Duration as StdDuration};

use rusqlite::Connection;
use tokio::sync::{Mutex, Semaphore};

use crate::error::AppError;

#[derive(Clone)]
pub struct AppState {
    pub db_path: String,
    pub http_github: reqwest::Client,
    pub http_ai: reqwest::Client,
    pub scanning_users: Arc<Mutex<HashSet<i64>>>,
    pub ai_semaphore: Arc<Semaphore>,
    /// Add `Secure` to session cookies (set when served over HTTPS).
    pub cookie_secure: bool,
}

impl AppState {
    pub fn db(&self) -> Result<Connection, AppError> {
        let conn = Connection::open(&self.db_path)?;
        // The busy timeout has to be set before the first lock-taking pragma,
        // otherwise `journal_mode = WAL` can return SQLITE_BUSY under load.
        conn.busy_timeout(StdDuration::from_millis(5000))?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        Ok(conn)
    }
}
