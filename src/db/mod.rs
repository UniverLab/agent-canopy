use anyhow::Result;
use rusqlite::{Connection, OptionalExtension};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

/// Bumped whenever a migration changes table/column names in a way that an
/// older binary's `CREATE TABLE IF NOT EXISTS` / `ADD COLUMN IF NOT EXISTS`
/// guards can't detect (they'd silently recreate the old names as empty
/// rather than erroring) — see `check_schema_version`. Version 2 is the
/// legacy queue-table rename (see `migrate_legacy_queue_schema`).
const SCHEMA_VERSION: i64 = 2;

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
        if let Err(e) = db.backfill_project_nodes() {
            tracing::warn!("Could not backfill project intelligence nodes: {e}");
        }
        if let Err(e) = db.seed_builtin_blueprints() {
            tracing::warn!("Could not seed builtin blueprints: {e}");
        }
        if let Err(e) = db.seed_builtin_ensemble_blueprints() {
            tracing::warn!("Could not seed builtin ensemble blueprints: {e}");
        }
        Ok(db)
    }

    /// Open the database, running migrations only if no foreign daemon is live.
    /// If a daemon is running, opens the file without running migrations or
    /// seeding (reads and ordinary writes against the existing schema still
    /// work) so a newer binary's migrations don't rename columns under a live
    /// older daemon. Every non-daemon path that opens the real `~/.canopy`
    /// database must use this instead of `new` — `run_http_server` is the one
    /// exception, because it holds the daemon lock and therefore *is* the
    /// daemon.
    pub fn new_safe(db_path: &PathBuf, data_dir: &std::path::Path) -> Result<Self> {
        if crate::daemon::process::other_instance_may_be_running(data_dir) {
            let conn = Connection::open(db_path)?;
            conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA foreign_keys=ON;")?;
            return Ok(Database {
                conn: Arc::new(Mutex::new(conn)),
            });
        }
        Database::new(db_path)
    }

    /// Refuses to open a database stamped with a schema version newer than
    /// this binary knows about. Without this, an older binary meeting a
    /// renamed/restructured schema would pass every `IF NOT EXISTS` guard
    /// (the old names it looks for are simply gone) and start up believing
    /// every table is empty — silent data loss from its perspective. A
    /// missing `schema_version` row (pre-dates this check, or a brand new
    /// database) is not a mismatch; every migration below is still
    /// self-guarding for that case.
    fn check_schema_version(conn: &Connection) -> Result<()> {
        let daemon_state_exists: bool = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = 'daemon_state'",
                [],
                |row| Ok(row.get::<_, i32>(0)? > 0),
            )
            .unwrap_or(false);
        if !daemon_state_exists {
            return Ok(());
        }

        let stored: Option<i64> = conn
            .query_row(
                "SELECT value FROM daemon_state WHERE key = 'schema_version'",
                [],
                |row| row.get::<_, String>(0),
            )
            .optional()?
            .and_then(|v| v.parse::<i64>().ok());

        if let Some(stored) = stored {
            if stored > SCHEMA_VERSION {
                anyhow::bail!(
                    "Database schema version {stored} is newer than this build of canopy (v{}) supports (schema version {SCHEMA_VERSION}). Upgrade canopy before opening this database.",
                    env!("CARGO_PKG_VERSION")
                );
            }
        }
        Ok(())
    }

    fn set_schema_version(conn: &Connection) -> Result<()> {
        conn.execute(
            "INSERT INTO daemon_state (key, value) VALUES ('schema_version', ?1)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            [SCHEMA_VERSION.to_string()],
        )?;
        Ok(())
    }

    /// Renames the previous-generation queue tables and columns (retired
    /// names recorded only as literal SQL below, since renaming requires
    /// naming what's being renamed) to their current equivalents. Guarded on
    /// the old primary table's existence, so it's a no-op on both a fresh
    /// database (never had it) and an already-migrated one (already renamed
    /// away) — safe to run on every startup. Must run before the `CREATE
    /// TABLE IF NOT EXISTS` batch below, which would otherwise create empty
    /// `queues`/`queue_members` tables first and make the rename fail with
    /// "table already exists".
    // RETIRED-SCHEMA-NAME-BEGIN (see `no_retired_schema_name_identifiers_remain_outside_its_migration`)
    fn migrate_legacy_queue_schema(conn: &Connection) -> Result<()> {
        let legacy_schema_present: bool = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = 'pools'",
                [],
                |row| Ok(row.get::<_, i32>(0)? > 0),
            )
            .unwrap_or(false);
        if !legacy_schema_present {
            return Ok(());
        }

        let legacy_active_run_alias_present: bool = conn
            .query_row(
                "SELECT COUNT(*) FROM pragma_table_info('loops') WHERE name = 'active_run_pool_id'",
                [],
                |row| Ok(row.get::<_, i32>(0)? > 0),
            )
            .unwrap_or(false);
        let legacy_template_column_present: bool = conn
            .query_row(
                "SELECT COUNT(*) FROM pragma_table_info('loops') WHERE name = 'spec_pool'",
                [],
                |row| Ok(row.get::<_, i32>(0)? > 0),
            )
            .unwrap_or(false);

        let mut migration_sql = String::from(
            "PRAGMA legacy_alter_table = OFF;
             BEGIN TRANSACTION;
             ALTER TABLE pools RENAME TO queues;
             ALTER TABLE pool_members RENAME TO queue_members;
             ALTER TABLE queue_members RENAME COLUMN pool_id TO queue_id;
             DROP INDEX IF EXISTS idx_pool_members_position;
             CREATE UNIQUE INDEX IF NOT EXISTS idx_queue_members_position
                 ON queue_members(queue_id, position);
            ",
        );
        if legacy_active_run_alias_present {
            migration_sql.push_str(
                "ALTER TABLE loops RENAME COLUMN active_run_pool_id TO active_run_queue_id;\n",
            );
        }
        if legacy_template_column_present {
            migration_sql.push_str("ALTER TABLE loops RENAME COLUMN spec_pool TO spec_queue;\n");
        }
        migration_sql.push_str("COMMIT;");

        conn.execute_batch(&migration_sql)
            .map_err(|e| anyhow::anyhow!("legacy queue schema migration failed: {e}"))?;

        let fk_violations: i64 = conn
            .query_row("SELECT COUNT(*) FROM pragma_foreign_key_check", [], |row| {
                row.get(0)
            })
            .map_err(|e| anyhow::anyhow!("legacy queue schema migration failed: {e}"))?;
        if fk_violations > 0 {
            anyhow::bail!(
                "legacy queue schema migration failed: {fk_violations} foreign key violation(s) detected after migration"
            );
        }

        Ok(())
    }
    // RETIRED-SCHEMA-NAME-END

    fn init(&self) -> Result<()> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| anyhow::anyhow!("Lock poisoned: {}", e))?;

        Self::check_schema_version(&conn)?;
        Self::migrate_legacy_queue_schema(&conn)?;

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
                enable_at TEXT,
                created_at TEXT NOT NULL,
                log_path TEXT NOT NULL,
                timeout_minutes INTEGER NOT NULL DEFAULT 15,
                expires_at TEXT,
                last_run_at TEXT,
                last_run_ok BOOLEAN,
                last_triggered_at TEXT,
                trigger_count INTEGER NOT NULL DEFAULT 0,
                notify_on_success BOOLEAN NOT NULL DEFAULT 0
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
                status TEXT NOT NULL DEFAULT 'active',
                session_type TEXT NOT NULL DEFAULT 'interactive',
                pid INTEGER,
                boot_id TEXT
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

            CREATE INDEX IF NOT EXISTS idx_interactive_sessions_workdir
                ON interactive_sessions(working_dir, started_at DESC);

            CREATE INDEX IF NOT EXISTS idx_terminal_sessions_workdir
                ON terminal_sessions(working_dir, created_at DESC);

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
            );

            CREATE TABLE IF NOT EXISTS rag_file_events (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                file_path TEXT NOT NULL,
                event_type TEXT NOT NULL,
                detail TEXT,
                occurred_at INTEGER NOT NULL
            );
            CREATE INDEX IF NOT EXISTS idx_rag_file_events_path
                ON rag_file_events(file_path);

            CREATE TABLE IF NOT EXISTS intelligence_nodes (
                id TEXT PRIMARY KEY,
                kind TEXT NOT NULL,
                title TEXT NOT NULL,
                body TEXT NOT NULL,
                metadata TEXT,
                project_hash TEXT,
                session_id TEXT,
                created_at INTEGER NOT NULL,
                updated_at INTEGER NOT NULL
            );

            CREATE INDEX IF NOT EXISTS idx_intelligence_nodes_kind_updated
                ON intelligence_nodes(kind, updated_at DESC);
            CREATE INDEX IF NOT EXISTS idx_intelligence_nodes_project_hash
                ON intelligence_nodes(project_hash);
            CREATE INDEX IF NOT EXISTS idx_intelligence_nodes_session_id
                ON intelligence_nodes(session_id);

            CREATE TABLE IF NOT EXISTS intelligence_edges (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                from_node_id TEXT NOT NULL,
                to_node_id TEXT NOT NULL,
                relation TEXT NOT NULL,
                weight REAL NOT NULL DEFAULT 1.0,
                created_at INTEGER NOT NULL,
                FOREIGN KEY(from_node_id) REFERENCES intelligence_nodes(id) ON DELETE CASCADE,
                FOREIGN KEY(to_node_id) REFERENCES intelligence_nodes(id) ON DELETE CASCADE
            );

            CREATE INDEX IF NOT EXISTS idx_intelligence_edges_from
                ON intelligence_edges(from_node_id);
            CREATE INDEX IF NOT EXISTS idx_intelligence_edges_to
                ON intelligence_edges(to_node_id);

            CREATE TABLE IF NOT EXISTS loops (
                id TEXT PRIMARY KEY,
                name TEXT NOT NULL,
                description TEXT,
                workdir TEXT NOT NULL,
                status TEXT NOT NULL,
                trigger_type TEXT,
                trigger_config TEXT,
                created_at INTEGER NOT NULL,
                started_at INTEGER,
                completed_at INTEGER,
                autorun_at INTEGER,
                spec_queue TEXT,
                active_run_queue_id TEXT,
                on_completed TEXT,
                auto_continue_at INTEGER,
                auto_continue_action TEXT
            );

            CREATE INDEX IF NOT EXISTS idx_loops_workdir_created
                ON loops(workdir, created_at DESC);

            CREATE TABLE IF NOT EXISTS loop_specs (
                id TEXT PRIMARY KEY,
                loop_id TEXT REFERENCES loops(id) ON DELETE CASCADE,
                name TEXT NOT NULL,
                description TEXT,
                position INTEGER NOT NULL,
                parallelizable INTEGER NOT NULL DEFAULT 0,
                status TEXT NOT NULL,
                started_at INTEGER,
                completed_at INTEGER,
                spec_start_head TEXT,
                workdir TEXT,
                completed_via TEXT,
                completed_via_reason TEXT,
                completed_via_at INTEGER,
                spec_committed_head TEXT,
                cross_run_attempts INTEGER NOT NULL DEFAULT 0
            );

            CREATE UNIQUE INDEX IF NOT EXISTS idx_loop_specs_position
                ON loop_specs(loop_id, position);

            CREATE TABLE IF NOT EXISTS loop_nodes (
                id TEXT PRIMARY KEY,
                spec_id TEXT REFERENCES loop_specs(id) ON DELETE CASCADE,
                loop_id TEXT REFERENCES loops(id) ON DELETE CASCADE,
                name TEXT NOT NULL,
                kind TEXT NOT NULL,
                config TEXT NOT NULL,
                position INTEGER NOT NULL,
                created_at INTEGER NOT NULL,
                CHECK ((spec_id IS NULL) <> (loop_id IS NULL))
            );

            CREATE UNIQUE INDEX IF NOT EXISTS idx_loop_nodes_position
                ON loop_nodes(spec_id, position);

            CREATE TABLE IF NOT EXISTS loop_edges (
                id TEXT PRIMARY KEY,
                spec_id TEXT REFERENCES loop_specs(id) ON DELETE CASCADE,
                loop_id TEXT REFERENCES loops(id) ON DELETE CASCADE,
                from_node TEXT NOT NULL REFERENCES loop_nodes(id) ON DELETE CASCADE,
                to_node TEXT NOT NULL REFERENCES loop_nodes(id) ON DELETE CASCADE,
                condition TEXT NOT NULL,
                route TEXT,
                CHECK ((spec_id IS NULL) <> (loop_id IS NULL))
            );

            CREATE INDEX IF NOT EXISTS idx_loop_edges_spec_from
                ON loop_edges(spec_id, from_node);

            CREATE TABLE IF NOT EXISTS loop_runs (
                id TEXT PRIMARY KEY,
                loop_id TEXT NOT NULL REFERENCES loops(id) ON DELETE CASCADE,
                spec_id TEXT NOT NULL REFERENCES loop_specs(id) ON DELETE CASCADE,
                node_id TEXT NOT NULL REFERENCES loop_nodes(id) ON DELETE CASCADE,
                status TEXT NOT NULL,
                input TEXT,
                output TEXT,
                started_at INTEGER NOT NULL,
                completed_at INTEGER,
                iteration INTEGER NOT NULL DEFAULT 1,
                pid INTEGER,
                boot_id TEXT,
                session_id TEXT
            );

            CREATE INDEX IF NOT EXISTS idx_loop_runs_spec_started
                ON loop_runs(spec_id, started_at ASC);

            CREATE INDEX IF NOT EXISTS idx_loop_runs_node_iteration
                ON loop_runs(node_id, iteration DESC);

            -- Speeds the sidebar's last-activity-per-loop aggregate
            -- (MAX(started_at) GROUP BY loop_id) into a loose index scan
            -- instead of a full table scan.
            CREATE INDEX IF NOT EXISTS idx_loop_runs_loop_started
                ON loop_runs(loop_id, started_at DESC);

            -- N2: firings of a loop's `on_completed` hook. Deliberately not
            -- `loop_runs` — that table's spec_id/node_id are NOT NULL FKs into
            -- a spec's graph, which a completion hook (no spec, no graph node)
            -- can never satisfy.
            CREATE TABLE IF NOT EXISTS loop_completion_hook_runs (
                id TEXT PRIMARY KEY,
                loop_id TEXT NOT NULL REFERENCES loops(id) ON DELETE CASCADE,
                status TEXT NOT NULL,
                output TEXT,
                summary TEXT,
                started_at INTEGER NOT NULL,
                completed_at INTEGER,
                pid INTEGER,
                boot_id TEXT
            );

            CREATE INDEX IF NOT EXISTS idx_loop_completion_hook_runs_loop_started
                ON loop_completion_hook_runs(loop_id, started_at ASC);

            -- F1: an ensemble unit -- the members and join are ordinary
            -- loop_nodes/loop_edges rows (the engine's graph-walking code is
            -- reused as-is); this row is what lets loop_get/loop_update_ensemble
            -- address the whole ensemble as one thing instead of N+1 nodes.
            CREATE TABLE IF NOT EXISTS ensembles (
                id TEXT PRIMARY KEY,
                spec_id TEXT REFERENCES loop_specs(id) ON DELETE CASCADE,
                loop_id TEXT REFERENCES loops(id) ON DELETE CASCADE,
                name TEXT NOT NULL,
                prompt_template TEXT NOT NULL,
                join_node_id TEXT NOT NULL REFERENCES loop_nodes(id) ON DELETE CASCADE,
                entry_from_node TEXT NOT NULL REFERENCES loop_nodes(id) ON DELETE CASCADE,
                entry_condition TEXT NOT NULL,
                min_pass INTEGER NOT NULL,
                straggler_timeout_minutes INTEGER,
                timeout_minutes INTEGER NOT NULL,
                on_pass_to TEXT NOT NULL REFERENCES loop_nodes(id) ON DELETE CASCADE,
                on_fail_to TEXT REFERENCES loop_nodes(id) ON DELETE SET NULL,
                created_at INTEGER NOT NULL,
                CHECK ((spec_id IS NULL) <> (loop_id IS NULL))
            );

            CREATE TABLE IF NOT EXISTS ensemble_members (
                ensemble_id TEXT NOT NULL REFERENCES ensembles(id) ON DELETE CASCADE,
                node_id TEXT NOT NULL REFERENCES loop_nodes(id) ON DELETE CASCADE,
                position INTEGER NOT NULL,
                platform TEXT NOT NULL,
                model TEXT,
                prompt_override TEXT,
                PRIMARY KEY (ensemble_id, node_id)
            );

            CREATE UNIQUE INDEX IF NOT EXISTS idx_ensemble_members_position
                ON ensemble_members(ensemble_id, position);

            CREATE INDEX IF NOT EXISTS idx_ensemble_members_node
                ON ensemble_members(node_id);

            CREATE INDEX IF NOT EXISTS idx_ensembles_join_node
                ON ensembles(join_node_id);

            -- F1: ensemble blueprints -- a whole ensemble's shared prompt +
            -- member list (unlike `blueprints`, which templates a single
            -- node), seeded with the builtin ensemble-proposers pattern.
            CREATE TABLE IF NOT EXISTS ensemble_blueprints (
                id TEXT PRIMARY KEY,
                name TEXT NOT NULL UNIQUE,
                prompt_template TEXT NOT NULL,
                members TEXT NOT NULL,
                min_pass INTEGER,
                builtin INTEGER NOT NULL DEFAULT 0,
                created_at INTEGER NOT NULL
            );

            CREATE TABLE IF NOT EXISTS queues (
                id TEXT PRIMARY KEY,
                name TEXT NOT NULL,
                created_at INTEGER NOT NULL
            );

            CREATE TABLE IF NOT EXISTS queue_members (
                queue_id TEXT NOT NULL REFERENCES queues(id) ON DELETE CASCADE,
                spec_id TEXT NOT NULL REFERENCES loop_specs(id) ON DELETE CASCADE,
                position INTEGER NOT NULL,
                group_name TEXT,
                PRIMARY KEY (queue_id, spec_id)
            );

            CREATE UNIQUE INDEX IF NOT EXISTS idx_queue_members_position
                ON queue_members(queue_id, position);

            CREATE TABLE IF NOT EXISTS seed_sessions (
                session_id TEXT PRIMARY KEY,
                seed_id TEXT NOT NULL,
                bound_at TEXT NOT NULL,
                FOREIGN KEY(session_id) REFERENCES interactive_sessions(id) ON DELETE CASCADE
            );

            CREATE INDEX IF NOT EXISTS idx_seed_sessions_seed
                ON seed_sessions(seed_id);

            CREATE TABLE IF NOT EXISTS blueprints (
                id TEXT PRIMARY KEY,
                name TEXT NOT NULL UNIQUE,
                kind TEXT NOT NULL,
                config TEXT NOT NULL,
                builtin INTEGER NOT NULL DEFAULT 0,
                created_at INTEGER NOT NULL
            );

            CREATE TABLE IF NOT EXISTS scheduled_sends (
                id TEXT PRIMARY KEY,
                prompt TEXT NOT NULL,
                target_session_id TEXT NOT NULL,
                fire_at INTEGER NOT NULL,
                created_at INTEGER NOT NULL
            );

            CREATE TABLE IF NOT EXISTS failed_scheduled_sends (
                id TEXT PRIMARY KEY,
                prompt TEXT NOT NULL,
                target_session_id TEXT NOT NULL,
                workdir TEXT,
                failed_at INTEGER NOT NULL
            );

            -- U8: the prompt builder's last-sent prompt per project, recalled
            -- with Ctrl+L. Insert-only with a timestamp (rather than one row
            -- per workdir) so this can grow into a browsable history later —
            -- today's reads take the most recent row per workdir (LIMIT 1).
            CREATE TABLE IF NOT EXISTS last_prompts (
                id TEXT PRIMARY KEY,
                workdir TEXT NOT NULL,
                prompt_text TEXT NOT NULL,
                builder_state TEXT,
                created_at INTEGER NOT NULL
            );

            CREATE INDEX IF NOT EXISTS idx_last_prompts_workdir_created
                ON last_prompts(workdir, created_at DESC);",
        )?;

        // `workdir` records which project a scheduled send targeted so a
        // dead-target failure can be preserved per-project for U8's recall.
        let has_scheduled_send_workdir: bool = conn
            .query_row(
                "SELECT COUNT(*) FROM pragma_table_info('scheduled_sends') WHERE name = 'workdir'",
                [],
                |row| Ok(row.get::<_, i32>(0)? > 0),
            )
            .unwrap_or(false);
        if !has_scheduled_send_workdir {
            conn.execute("ALTER TABLE scheduled_sends ADD COLUMN workdir TEXT", [])
                .map_err(|e| anyhow::anyhow!("Migration failed: {e}"))?;
        }

        let has_scheduled_builder_state: bool = conn
            .query_row(
                "SELECT COUNT(*) FROM pragma_table_info('scheduled_sends') WHERE name = 'builder_state'",
                [],
                |row| Ok(row.get::<_, i32>(0)? > 0),
            )
            .unwrap_or(false);
        if !has_scheduled_builder_state {
            conn.execute(
                "ALTER TABLE scheduled_sends ADD COLUMN builder_state TEXT",
                [],
            )
            .map_err(|e| anyhow::anyhow!("Migration failed: {e}"))?;
        }

        let has_session_type: bool = conn
            .query_row(
                "SELECT COUNT(*) FROM pragma_table_info('interactive_sessions') WHERE name = 'session_type'",
                [],
                |row| Ok(row.get::<_, i32>(0)? > 0),
            )
            .unwrap_or(false);
        if !has_session_type {
            conn.execute(
                "ALTER TABLE interactive_sessions ADD COLUMN session_type TEXT NOT NULL DEFAULT 'interactive'",
                [],
            )
            .map_err(|e| anyhow::anyhow!("Migration failed: {e}"))?;
        }

        let has_pid: bool = conn
            .query_row(
                "SELECT COUNT(*) FROM pragma_table_info('interactive_sessions') WHERE name = 'pid'",
                [],
                |row| Ok(row.get::<_, i32>(0)? > 0),
            )
            .unwrap_or(false);
        if !has_pid {
            conn.execute(
                "ALTER TABLE interactive_sessions ADD COLUMN pid INTEGER",
                [],
            )
            .map_err(|e| anyhow::anyhow!("Migration failed: {e}"))?;
        }

        // Boot-id aware auto-resume (see should_resume_session in
        // tui::app::mod): a stored pid alone can't tell a resumed session
        // from an unrelated process that got the same pid after a reboot
        // recycled the pid space. NULL for rows written before this column
        // existed — treated as "unknown boot" (always safe to resume).
        let has_boot_id: bool = conn
            .query_row(
                "SELECT COUNT(*) FROM pragma_table_info('interactive_sessions') WHERE name = 'boot_id'",
                [],
                |row| Ok(row.get::<_, i32>(0)? > 0),
            )
            .unwrap_or(false);
        if !has_boot_id {
            conn.execute(
                "ALTER TABLE interactive_sessions ADD COLUMN boot_id TEXT",
                [],
            )
            .map_err(|e| anyhow::anyhow!("Migration failed: {e}"))?;
        }

        // Older builds registered `canopy bridge` sidecars with
        // session_type = 'interactive' (a bug — see daemon::bridge), which made
        // auto_resume_sessions try to relaunch them as chat CLIs on every TUI
        // startup. Reclassify any such rows so they're excluded from
        // get_active_sessions() going forward. Idempotent: once reclassified,
        // the WHERE clause no longer matches them.
        conn.execute(
            "UPDATE interactive_sessions SET session_type = 'bridge'
             WHERE cli = 'bridge' AND session_type != 'bridge'",
            [],
        )
        .map_err(|e| anyhow::anyhow!("Migration failed: {e}"))?;

        let has_enable_at: bool = conn
            .query_row(
                "SELECT COUNT(*) FROM pragma_table_info('agents') WHERE name = 'enable_at'",
                [],
                |row| Ok(row.get::<_, i32>(0)? > 0),
            )
            .unwrap_or(false);
        if !has_enable_at {
            conn.execute("ALTER TABLE agents ADD COLUMN enable_at TEXT", [])
                .map_err(|e| anyhow::anyhow!("Migration failed: {e}"))?;
        }

        // Per-agent opt-in to success notifications (B27). Failures always
        // notify; scheduled/watch successes stay silent unless this is set,
        // so a frequent cron agent can't spam the Action Center. Older
        // databases predate the column; default 0 keeps every existing agent
        // on the quiet-on-success policy.
        let has_notify_on_success: bool = conn
            .query_row(
                "SELECT COUNT(*) FROM pragma_table_info('agents') WHERE name = 'notify_on_success'",
                [],
                |row| Ok(row.get::<_, i32>(0)? > 0),
            )
            .unwrap_or(false);
        if !has_notify_on_success {
            conn.execute(
                "ALTER TABLE agents ADD COLUMN notify_on_success BOOLEAN NOT NULL DEFAULT 0",
                [],
            )
            .map_err(|e| anyhow::anyhow!("Migration failed: {e}"))?;
        }

        // Loops gained optional cron/watch triggers; older databases predate the
        // columns. Add them if missing (both nullable, so existing rows stay
        // manual-only).
        for column in ["trigger_type", "trigger_config"] {
            let has_column: bool = conn
                .query_row(
                    "SELECT COUNT(*) FROM pragma_table_info('loops') WHERE name = ?1",
                    [column],
                    |row| Ok(row.get::<_, i32>(0)? > 0),
                )
                .unwrap_or(false);
            if !has_column {
                conn.execute(&format!("ALTER TABLE loops ADD COLUMN {column} TEXT"), [])
                    .map_err(|e| anyhow::anyhow!("Migration failed: {e}"))?;
            }
        }

        // One-shot resume schedule for loops (mirrors agents' `enable_at`);
        // older databases predate the column.
        let has_autorun_at: bool = conn
            .query_row(
                "SELECT COUNT(*) FROM pragma_table_info('loops') WHERE name = 'autorun_at'",
                [],
                |row| Ok(row.get::<_, i32>(0)? > 0),
            )
            .unwrap_or(false);
        if !has_autorun_at {
            conn.execute("ALTER TABLE loops ADD COLUMN autorun_at INTEGER", [])
                .map_err(|e| anyhow::anyhow!("Migration failed: {e}"))?;
        }

        // One-shot deferred-resume-while-paused schedule (distinct from
        // `autorun_at`'s reset-and-relaunch — see
        // [`crate::domain::loops::Loop::auto_continue_at`]); older databases
        // predate the columns.
        for (column, sql_type) in [
            ("auto_continue_at", "INTEGER"),
            ("auto_continue_action", "TEXT"),
        ] {
            let has_column: bool = conn
                .query_row(
                    "SELECT COUNT(*) FROM pragma_table_info('loops') WHERE name = ?1",
                    [column],
                    |row| Ok(row.get::<_, i32>(0)? > 0),
                )
                .unwrap_or(false);
            if !has_column {
                conn.execute(
                    &format!("ALTER TABLE loops ADD COLUMN {column} {sql_type}"),
                    [],
                )
                .map_err(|e| anyhow::anyhow!("Migration failed: {e}"))?;
            }
        }

        // `spec_queue` (7f2efdf) was an unused template model, retired in favor
        // of the `queues`/`queue_members` tables below. Kept only so pre-R4
        // databases that already have the column don't need a destructive
        // migration; current code never reads or writes it.
        let has_spec_queue: bool = conn
            .query_row(
                "SELECT COUNT(*) FROM pragma_table_info('loops') WHERE name = 'spec_queue'",
                [],
                |row| Ok(row.get::<_, i32>(0)? > 0),
            )
            .unwrap_or(false);
        if !has_spec_queue {
            conn.execute("ALTER TABLE loops ADD COLUMN spec_queue TEXT", [])
                .map_err(|e| anyhow::anyhow!("Migration failed: {e}"))?;
        }

        // `spec_start_head` records the workdir's git HEAD when a spec starts
        // running, so `check` nodes can verify a spec actually committed
        // without relying on a file marker outside the spec row. Older
        // databases predate the column.
        let has_spec_start_head: bool = conn
            .query_row(
                "SELECT COUNT(*) FROM pragma_table_info('loop_specs') WHERE name = 'spec_start_head'",
                [],
                |row| Ok(row.get::<_, i32>(0)? > 0),
            )
            .unwrap_or(false);
        if !has_spec_start_head {
            conn.execute("ALTER TABLE loop_specs ADD COLUMN spec_start_head TEXT", [])
                .map_err(|e| anyhow::anyhow!("Migration failed: {e}"))?;
        }

        // Standalone specs (R3): a spec no longer must belong to a loop — it
        // can exist as a backlog item (optionally tagged to a workdir)
        // before being assigned. Older databases have `loop_id NOT NULL`,
        // which `ALTER TABLE ... ADD COLUMN` cannot relax, so rebuild the
        // table via SQLite's documented copy-and-rename procedure. Every
        // existing row keeps its `loop_id`; only new rows may leave it NULL.
        let loop_specs_loop_id_nullable: bool = conn
            .query_row(
                "SELECT \"notnull\" FROM pragma_table_info('loop_specs') WHERE name = 'loop_id'",
                [],
                |row| row.get::<_, i64>(0),
            )
            .map(|notnull| notnull == 0)
            .unwrap_or(false);
        if !loop_specs_loop_id_nullable {
            conn.execute_batch(
                "PRAGMA foreign_keys=OFF;
                 BEGIN TRANSACTION;

                 CREATE TABLE loop_specs_new (
                     id TEXT PRIMARY KEY,
                     loop_id TEXT REFERENCES loops(id) ON DELETE CASCADE,
                     name TEXT NOT NULL,
                     description TEXT,
                     position INTEGER NOT NULL,
                     parallelizable INTEGER NOT NULL DEFAULT 0,
                     status TEXT NOT NULL,
                     started_at INTEGER,
                     completed_at INTEGER,
                     spec_start_head TEXT
                 );
                 INSERT INTO loop_specs_new (id, loop_id, name, description, position, parallelizable, status, started_at, completed_at, spec_start_head)
                     SELECT id, loop_id, name, description, position, parallelizable, status, started_at, completed_at, spec_start_head FROM loop_specs;
                 DROP TABLE loop_specs;
                 ALTER TABLE loop_specs_new RENAME TO loop_specs;

                 CREATE UNIQUE INDEX IF NOT EXISTS idx_loop_specs_position
                     ON loop_specs(loop_id, position);

                 COMMIT;
                 PRAGMA foreign_keys=ON;",
            )
            .map_err(|e| anyhow::anyhow!("Migration failed: {e}"))?;
        }

        // Optional workdir tag on specs (R3), for backlog filtering only —
        // it never drives execution. Nullable, so existing (loop-bound)
        // specs are unaffected; older databases predate the column.
        let has_spec_workdir: bool = conn
            .query_row(
                "SELECT COUNT(*) FROM pragma_table_info('loop_specs') WHERE name = 'workdir'",
                [],
                |row| Ok(row.get::<_, i32>(0)? > 0),
            )
            .unwrap_or(false);
        if !has_spec_workdir {
            conn.execute("ALTER TABLE loop_specs ADD COLUMN workdir TEXT", [])
                .map_err(|e| anyhow::anyhow!("Migration failed: {e}"))?;
        }

        // Loop-level graphs (R1): a node/edge may now target a loop directly
        // (`loop_id`) instead of a spec, so it can be defined once per loop
        // instead of being repeated across every spec. Older databases have
        // `spec_id NOT NULL` on both tables, which `ALTER TABLE ... ADD
        // COLUMN` cannot relax, so rebuild the tables via SQLite's documented
        // copy-and-rename procedure ("Making Other Kinds Of Table Schema
        // Changes"). Every existing row keeps its `spec_id`; `loop_id` starts
        // NULL for all of them, so nothing already saved changes meaning.
        let has_loop_nodes_loop_id: bool = conn
            .query_row(
                "SELECT COUNT(*) FROM pragma_table_info('loop_nodes') WHERE name = 'loop_id'",
                [],
                |row| Ok(row.get::<_, i32>(0)? > 0),
            )
            .unwrap_or(false);
        if !has_loop_nodes_loop_id {
            conn.execute_batch(
                "PRAGMA foreign_keys=OFF;
                 BEGIN TRANSACTION;

                 CREATE TABLE loop_nodes_new (
                     id TEXT PRIMARY KEY,
                     spec_id TEXT REFERENCES loop_specs(id) ON DELETE CASCADE,
                     loop_id TEXT REFERENCES loops(id) ON DELETE CASCADE,
                     name TEXT NOT NULL,
                     kind TEXT NOT NULL,
                     config TEXT NOT NULL,
                     position INTEGER NOT NULL,
                     created_at INTEGER NOT NULL,
                     CHECK ((spec_id IS NULL) <> (loop_id IS NULL))
                 );
                 INSERT INTO loop_nodes_new (id, spec_id, loop_id, name, kind, config, position, created_at)
                     SELECT id, spec_id, NULL, name, kind, config, position, created_at FROM loop_nodes;
                 DROP TABLE loop_nodes;
                 ALTER TABLE loop_nodes_new RENAME TO loop_nodes;

                 CREATE UNIQUE INDEX IF NOT EXISTS idx_loop_nodes_position
                     ON loop_nodes(spec_id, position);
                 CREATE UNIQUE INDEX IF NOT EXISTS idx_loop_nodes_loop_position
                     ON loop_nodes(loop_id, position);

                 CREATE TABLE loop_edges_new (
                     id TEXT PRIMARY KEY,
                     spec_id TEXT REFERENCES loop_specs(id) ON DELETE CASCADE,
                     loop_id TEXT REFERENCES loops(id) ON DELETE CASCADE,
                     from_node TEXT NOT NULL REFERENCES loop_nodes(id) ON DELETE CASCADE,
                     to_node TEXT NOT NULL REFERENCES loop_nodes(id) ON DELETE CASCADE,
                     condition TEXT NOT NULL,
                     CHECK ((spec_id IS NULL) <> (loop_id IS NULL))
                 );
                 INSERT INTO loop_edges_new (id, spec_id, loop_id, from_node, to_node, condition)
                     SELECT id, spec_id, NULL, from_node, to_node, condition FROM loop_edges;
                 DROP TABLE loop_edges;
                 ALTER TABLE loop_edges_new RENAME TO loop_edges;

                 CREATE INDEX IF NOT EXISTS idx_loop_edges_spec_from
                     ON loop_edges(spec_id, from_node);
                 CREATE INDEX IF NOT EXISTS idx_loop_edges_loop_from
                     ON loop_edges(loop_id, from_node);

                 COMMIT;
                 PRAGMA foreign_keys=ON;",
            )
            .map_err(|e| anyhow::anyhow!("Migration failed: {e}"))?;
        }

        // These reference `loop_id`, so they can only be created once the
        // column is guaranteed to exist — either from the fresh CREATE TABLE
        // above (new databases) or the rebuild just above (migrated ones).
        conn.execute_batch(
            "CREATE UNIQUE INDEX IF NOT EXISTS idx_loop_nodes_loop_position
                 ON loop_nodes(loop_id, position);
             CREATE INDEX IF NOT EXISTS idx_loop_edges_loop_from
                 ON loop_edges(loop_id, from_node);",
        )
        .map_err(|e| anyhow::anyhow!("Migration failed: {e}"))?;

        // `active_run_queue_id` persists which queue (if any) a loop's current/
        // last run drew from, so an interrupted run (quota failure, daemon
        // crash) can be resumed against the same queue by every resume path
        // (scheduled autorun, `loop_reset`) instead of falling back to the
        // loop's own bound specs. Older databases predate the column.
        let has_active_run_queue_id: bool = conn
            .query_row(
                "SELECT COUNT(*) FROM pragma_table_info('loops') WHERE name = 'active_run_queue_id'",
                [],
                |row| Ok(row.get::<_, i32>(0)? > 0),
            )
            .unwrap_or(false);
        if !has_active_run_queue_id {
            conn.execute("ALTER TABLE loops ADD COLUMN active_run_queue_id TEXT", [])
                .map_err(|e| anyhow::anyhow!("Migration failed: {e}"))?;
        }

        // `on_completed` (N2): the loop's optional post-completion hook
        // config (agent-node-style JSON: platform/model/prompt/
        // timeout_minutes), fired once when a run reaches `Completed`. `NULL`
        // on older databases and on any loop that never configured one —
        // exactly today's (pre-N2) behavior.
        let has_on_completed: bool = conn
            .query_row(
                "SELECT COUNT(*) FROM pragma_table_info('loops') WHERE name = 'on_completed'",
                [],
                |row| Ok(row.get::<_, i32>(0)? > 0),
            )
            .unwrap_or(false);
        if !has_on_completed {
            conn.execute("ALTER TABLE loops ADD COLUMN on_completed TEXT", [])
                .map_err(|e| anyhow::anyhow!("Migration failed: {e}"))?;
        }

        // `pid`/`boot_id` (B12): the OS process-group leader spawned for a
        // node run, and the boot it was spawned under. Lets the engine
        // `killpg` an active run's process on every abnormal end (timeout,
        // pause, reset, budget exhaustion, run failure, daemon shutdown),
        // and lets startup reconciliation attempt a best-effort kill of
        // survivors from the same boot. Older databases predate both
        // columns; NULL on existing rows (nothing was tracked for them, so
        // there's nothing to kill).
        for column in ["pid", "boot_id"] {
            let has_column: bool = conn
                .query_row(
                    "SELECT COUNT(*) FROM pragma_table_info('loop_runs') WHERE name = ?1",
                    [column],
                    |row| Ok(row.get::<_, i32>(0)? > 0),
                )
                .unwrap_or(false);
            if !has_column {
                let sql = if column == "pid" {
                    "ALTER TABLE loop_runs ADD COLUMN pid INTEGER".to_string()
                } else {
                    "ALTER TABLE loop_runs ADD COLUMN boot_id TEXT".to_string()
                };
                conn.execute(&sql, [])
                    .map_err(|e| anyhow::anyhow!("Migration failed: {e}"))?;
            }
        }

        // `session_id` (RS1): the harness session id that served a node run,
        // captured per-platform (set-at-spawn for platforms that accept a
        // caller-chosen id, list-after-run for those that can enumerate their
        // sessions). Older databases predate the column; NULL on existing rows
        // (no session identity was ever captured for them). Additive — the
        // foundation for resume mode (RS2).
        let has_session_id: bool = conn
            .query_row(
                "SELECT COUNT(*) FROM pragma_table_info('loop_runs') WHERE name = 'session_id'",
                [],
                |row| Ok(row.get::<_, i32>(0)? > 0),
            )
            .unwrap_or(false);
        if !has_session_id {
            conn.execute("ALTER TABLE loop_runs ADD COLUMN session_id TEXT", [])
                .map_err(|e| anyhow::anyhow!("Migration failed: {e}"))?;
        }

        // `completed_via`/`completed_via_reason`/`completed_via_at` (B25):
        // administrative spec status transitions, recorded with provenance
        // and reason. Older databases predate these columns; NULL on existing
        // rows (all existing completions are engine-driven).
        for column in ["completed_via", "completed_via_reason", "completed_via_at"] {
            let has_column: bool = conn
                .query_row(
                    "SELECT COUNT(*) FROM pragma_table_info('loop_specs') WHERE name = ?1",
                    [column],
                    |row| Ok(row.get::<_, i32>(0)? > 0),
                )
                .unwrap_or(false);
            if !has_column {
                let sql = match column {
                    "completed_via" => "ALTER TABLE loop_specs ADD COLUMN completed_via TEXT",
                    "completed_via_reason" => {
                        "ALTER TABLE loop_specs ADD COLUMN completed_via_reason TEXT"
                    }
                    "completed_via_at" => {
                        "ALTER TABLE loop_specs ADD COLUMN completed_via_at INTEGER"
                    }
                    _ => "",
                };
                if !sql.is_empty() {
                    conn.execute(sql, [])
                        .map_err(|e| anyhow::anyhow!("Migration failed: {e}"))?;
                }
            }
        }

        // `group_name` (RS3): the optional context group a queue member belongs
        // to. Grouped members share a warm harness session — the first agent
        // node of a grouped spec resumes the session captured by the previous
        // successfully-completed grouped sibling instead of cold-starting.
        // Older databases predate the column; NULL on existing rows (every
        // legacy member is ungrouped and never cross-resumes). Additive.
        let has_group_name: bool = conn
            .query_row(
                "SELECT COUNT(*) FROM pragma_table_info('queue_members') WHERE name = 'group_name'",
                [],
                |row| Ok(row.get::<_, i32>(0)? > 0),
            )
            .unwrap_or(false);
        if !has_group_name {
            conn.execute("ALTER TABLE queue_members ADD COLUMN group_name TEXT", [])
                .map_err(|e| anyhow::anyhow!("Migration failed: {e}"))?;
        }

        // `route` (router nodes): the label a `route`-conditioned edge names,
        // alongside `condition = 'route'` — see
        // `domain::loops::LoopEdgeCondition::{as_str, route_label, from_parts}`.
        // NULL for every pre-existing edge (none of them can be a router's
        // route edge, since `LoopNodeKind::Router` didn't exist yet either).
        let has_edge_route: bool = conn
            .query_row(
                "SELECT COUNT(*) FROM pragma_table_info('loop_edges') WHERE name = 'route'",
                [],
                |row| Ok(row.get::<_, i32>(0)? > 0),
            )
            .unwrap_or(false);
        if !has_edge_route {
            conn.execute("ALTER TABLE loop_edges ADD COLUMN route TEXT", [])
                .map_err(|e| anyhow::anyhow!("Migration failed: {e}"))?;
        }

        // Per-member prompt override: a member can now render its own prompt
        // instead of the ensemble's shared `prompt_template` — see
        // `EnsembleMember::prompt_override`. NULL for every pre-existing
        // member, which is exactly "use the shared template", so no
        // behavioural migration is needed alongside the column add.
        let has_member_prompt_override: bool = conn
            .query_row(
                "SELECT COUNT(*) FROM pragma_table_info('ensemble_members') WHERE name = 'prompt_override'",
                [],
                |row| Ok(row.get::<_, i32>(0)? > 0),
            )
            .unwrap_or(false);
        if !has_member_prompt_override {
            conn.execute(
                "ALTER TABLE ensemble_members ADD COLUMN prompt_override TEXT",
                [],
            )
            .map_err(|e| anyhow::anyhow!("Migration failed: {e}"))?;
        }

        // Archiving (F4): a loop can leave the sidebar's browsing list
        // without losing its row, specs, or run history — the reversible
        // alternative to `delete_loop`. A constant `DEFAULT 0` means every
        // pre-existing loop reads as not-archived with no data movement.
        let has_archived: bool = conn
            .query_row(
                "SELECT COUNT(*) FROM pragma_table_info('loops') WHERE name = 'archived'",
                [],
                |row| Ok(row.get::<_, i32>(0)? > 0),
            )
            .unwrap_or(false);
        if !has_archived {
            conn.execute(
                "ALTER TABLE loops ADD COLUMN archived INTEGER NOT NULL DEFAULT 0",
                [],
            )
            .map_err(|e| anyhow::anyhow!("Migration failed: {e}"))?;
        }

        // C1: distinguishes a loop `reconcile_orphaned_loops` paused after an
        // unclean daemon exit from one an operator paused on purpose, so a
        // pending `autorun_at` schedule can survive the former but still be
        // blocked by the latter — see
        // [`crate::domain::loops::Loop::is_autorun_due`]. `DEFAULT 0` means
        // every pre-existing `Paused` loop reads as operator-paused, which is
        // the safe assumption for a row this migration has no way to
        // distinguish retroactively.
        let has_paused_by_reconciliation: bool = conn
            .query_row(
                "SELECT COUNT(*) FROM pragma_table_info('loops') WHERE name = 'paused_by_reconciliation'",
                [],
                |row| Ok(row.get::<_, i32>(0)? > 0),
            )
            .unwrap_or(false);
        if !has_paused_by_reconciliation {
            conn.execute(
                "ALTER TABLE loops ADD COLUMN paused_by_reconciliation INTEGER NOT NULL DEFAULT 0",
                [],
            )
            .map_err(|e| anyhow::anyhow!("Migration failed: {e}"))?;
        }

        // CM2: optional pre-wired target for infrastructure failures. When
        // set, new agent/check/gate nodes auto-create a `Break` edge to
        // this node. `NULL` on every pre-existing row (no auto-wiring).
        let has_infra_node_id: bool = conn
            .query_row(
                "SELECT COUNT(*) FROM pragma_table_info('loops') WHERE name = 'infra_node_id'",
                [],
                |row| Ok(row.get::<_, i32>(0)? > 0),
            )
            .unwrap_or(false);
        if !has_infra_node_id {
            conn.execute("ALTER TABLE loops ADD COLUMN infra_node_id TEXT", [])
                .map_err(|e| anyhow::anyhow!("Migration failed: {e}"))?;
        }

        // `spec_committed_head` (C15): the workdir's git HEAD immediately
        // after a `commit_rights: true` node's own execution actually moved
        // it — as opposed to `spec_start_head`, which only proves *some*
        // commit landed since the spec began and is satisfied just as well
        // by a concurrent commit from outside this run sharing the same
        // worktree. `NULL` on every pre-existing row (no historical spec's
        // committing node was ever tracked this way) and on any row where
        // the graph never named a committer. Additive.
        let has_spec_committed_head: bool = conn
            .query_row(
                "SELECT COUNT(*) FROM pragma_table_info('loop_specs') WHERE name = 'spec_committed_head'",
                [],
                |row| Ok(row.get::<_, i32>(0)? > 0),
            )
            .unwrap_or(false);
        if !has_spec_committed_head {
            conn.execute(
                "ALTER TABLE loop_specs ADD COLUMN spec_committed_head TEXT",
                [],
            )
            .map_err(|e| anyhow::anyhow!("Migration failed: {e}"))?;
        }

        // `cross_run_attempts` (C19): how many separate loop executions this
        // spec has failed with a genuine (non-infrastructure) verdict.
        // Unlike the per-node iteration budget `run_spec` tracks in memory
        // for the duration of one execution, this is persisted so it
        // survives `loop_reset`, a relaunch, and a daemon restart — the
        // whole point being that an unsatisfiable spec doesn't get a fresh
        // budget every time an operator resets and relaunches after a quota
        // failure. `DEFAULT 0` means every pre-existing spec reads as never
        // having failed under this counter, which is correct: it didn't
        // exist to count anything before now. See
        // `Database::increment_loop_spec_cross_run_attempts` and
        // `LoopEngine::record_spec_attempt`.
        let has_cross_run_attempts: bool = conn
            .query_row(
                "SELECT COUNT(*) FROM pragma_table_info('loop_specs') WHERE name = 'cross_run_attempts'",
                [],
                |row| Ok(row.get::<_, i32>(0)? > 0),
            )
            .unwrap_or(false);
        if !has_cross_run_attempts {
            conn.execute(
                "ALTER TABLE loop_specs ADD COLUMN cross_run_attempts INTEGER NOT NULL DEFAULT 0",
                [],
            )
            .map_err(|e| anyhow::anyhow!("Migration failed: {e}"))?;
        }

        let has_ensemble_kind: bool = conn
            .query_row(
                "SELECT COUNT(*) FROM pragma_table_info('ensembles') WHERE name = 'kind'",
                [],
                |row| Ok(row.get::<_, i32>(0)? > 0),
            )
            .unwrap_or(false);
        if !has_ensemble_kind {
            conn.execute(
                "ALTER TABLE ensembles ADD COLUMN kind TEXT NOT NULL DEFAULT 'parallel'",
                [],
            )
            .map_err(|e| anyhow::anyhow!("Migration failed: {e}"))?;
        }

        let has_round_robin_index: bool = conn
            .query_row(
                "SELECT COUNT(*) FROM pragma_table_info('ensembles') WHERE name = 'round_robin_index'",
                [],
                |row| Ok(row.get::<_, i32>(0)? > 0),
            )
            .unwrap_or(false);
        if !has_round_robin_index {
            conn.execute(
                "ALTER TABLE ensembles ADD COLUMN round_robin_index INTEGER",
                [],
            )
            .map_err(|e| anyhow::anyhow!("Migration failed: {e}"))?;
        }

        Self::set_schema_version(&conn)?;

        Ok(())
    }
}

pub mod achievements;
pub mod agent;
pub mod blueprints;
pub mod clean;
pub mod ensembles;
pub mod gamification;
pub mod group;
pub mod health;
pub mod intelligence;
pub mod last_prompts;
pub mod loop_transfer;
pub mod loops;
pub mod project;
pub mod queues;
pub mod run;
pub mod scheduled_sends;
pub mod seeds;
pub mod session;
pub mod state;
pub mod sync;

#[cfg(test)]
pub use crate::application::ports::{AgentRepository, RunRepository, StateRepository};

#[cfg(test)]
mod tests;
