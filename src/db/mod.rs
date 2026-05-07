use anyhow::Result;
use rusqlite::Connection;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

/// Thread-safe `SQLite` database wrapper.
///
/// Uses an `Arc<Mutex<Connection>>` so the handle can be cheaply cloned and
/// shared across threads (e.g. for background file-scanning tasks) while still
/// serialising all SQLite writes through a single connection.
#[derive(Clone)]
pub struct Database {
    conn: Arc<Mutex<Connection>>,
}

impl Database {
    pub fn new(db_path: &PathBuf) -> Result<Self> {
        let conn = Connection::open(db_path)?;
        conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA foreign_keys=ON;")?;
        let db = Database {
            conn: Arc::new(Mutex::new(conn)),
        };
        db.init()?;
        Ok(db)
    }

    fn init(&self) -> Result<()> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| anyhow::anyhow!("Lock poisoned: {}", e))?;

        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS agents (
                id TEXT PRIMARY KEY,
                prompt TEXT NOT NULL,
                trigger_type TEXT,
                trigger_config TEXT,
                cli TEXT NOT NULL,
                model TEXT,
                working_dir TEXT,
                enabled BOOLEAN NOT NULL DEFAULT 1,
                created_at TEXT NOT NULL,
                log_path TEXT NOT NULL,
                timeout_minutes INTEGER NOT NULL DEFAULT 15,
                expires_at TEXT,
                last_run_at TEXT,
                last_run_ok BOOLEAN,
                last_triggered_at TEXT,
                trigger_count INTEGER NOT NULL DEFAULT 0
            );

            CREATE TABLE IF NOT EXISTS runs (
                id TEXT PRIMARY KEY,
                background_agent_id TEXT NOT NULL,
                status TEXT NOT NULL DEFAULT 'pending',
                trigger_type TEXT NOT NULL,
                summary TEXT,
                started_at TEXT NOT NULL,
                finished_at TEXT,
                exit_code INTEGER,
                timeout_at TEXT
            );

            CREATE TABLE IF NOT EXISTS daemon_state (
                key TEXT PRIMARY KEY,
                value TEXT NOT NULL
            );

            CREATE TABLE IF NOT EXISTS interactive_sessions (
                id TEXT PRIMARY KEY,
                name TEXT NOT NULL,
                cli TEXT NOT NULL,
                working_dir TEXT NOT NULL,
                args TEXT,
                started_at TEXT NOT NULL,
                exited_at TEXT,
                exit_code INTEGER,
                status TEXT NOT NULL DEFAULT 'active'
            );

            CREATE TABLE IF NOT EXISTS terminal_sessions (
                id TEXT PRIMARY KEY,
                name TEXT NOT NULL,
                shell TEXT NOT NULL,
                working_dir TEXT NOT NULL,
                created_at TEXT NOT NULL,
                last_active TEXT,
                status TEXT NOT NULL DEFAULT 'idle'
            );

            CREATE TABLE IF NOT EXISTS groups (
                id TEXT PRIMARY KEY,
                orientation TEXT NOT NULL DEFAULT 'horizontal',
                session_a TEXT NOT NULL,
                session_b TEXT NOT NULL,
                created_at TEXT NOT NULL
            );

            CREATE TABLE IF NOT EXISTS sync_messages (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                workdir TEXT NOT NULL,
                agent_id TEXT NOT NULL,
                agent_name TEXT NOT NULL,
                kind TEXT NOT NULL,
                message TEXT NOT NULL,
                payload TEXT,
                created_at INTEGER NOT NULL
            );

            CREATE TABLE IF NOT EXISTS sync_locks (
                id TEXT PRIMARY KEY,
                workdir TEXT NOT NULL,
                agent_id TEXT NOT NULL,
                lock_type TEXT NOT NULL,
                resource TEXT NOT NULL,
                acquired_at INTEGER NOT NULL,
                expires_at INTEGER,
                released_at INTEGER
            );

            CREATE TABLE IF NOT EXISTS projects (
                hash TEXT PRIMARY KEY,
                path TEXT NOT NULL,
                name TEXT NOT NULL,
                description TEXT,
                tags TEXT,
                indexed_at INTEGER,
                created_at INTEGER NOT NULL
            );

            CREATE TABLE IF NOT EXISTS rag_queue (
                source_path TEXT NOT NULL PRIMARY KEY,
                status TEXT NOT NULL,
                queued_at INTEGER NOT NULL,
                updated_at INTEGER NOT NULL
            );",
        )?;

        Ok(())
    }
}

pub mod agent;
pub mod group;
pub mod project;
pub mod run;
pub mod session;
pub mod state;
pub mod sync;

#[cfg(test)]
pub use crate::application::ports::{AgentRepository, RunRepository, StateRepository};

#[cfg(test)]
mod tests;
