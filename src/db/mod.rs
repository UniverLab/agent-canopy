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
                spec_pool TEXT,
                active_run_pool_id TEXT,
                on_completed TEXT
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
                completed_via_at INTEGER
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

            CREATE TABLE IF NOT EXISTS pools (
                id TEXT PRIMARY KEY,
                name TEXT NOT NULL,
                created_at INTEGER NOT NULL
            );

            CREATE TABLE IF NOT EXISTS pool_members (
                pool_id TEXT NOT NULL REFERENCES pools(id) ON DELETE CASCADE,
                spec_id TEXT NOT NULL REFERENCES loop_specs(id) ON DELETE CASCADE,
                position INTEGER NOT NULL,
                group_name TEXT,
                PRIMARY KEY (pool_id, spec_id)
            );

            CREATE UNIQUE INDEX IF NOT EXISTS idx_pool_members_position
                ON pool_members(pool_id, position);

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

        // `spec_pool` (7f2efdf) was an unused template model, retired in favor
        // of the `pools`/`pool_members` tables below. Kept only so pre-R4
        // databases that already have the column don't need a destructive
        // migration; current code never reads or writes it.
        let has_spec_pool: bool = conn
            .query_row(
                "SELECT COUNT(*) FROM pragma_table_info('loops') WHERE name = 'spec_pool'",
                [],
                |row| Ok(row.get::<_, i32>(0)? > 0),
            )
            .unwrap_or(false);
        if !has_spec_pool {
            conn.execute("ALTER TABLE loops ADD COLUMN spec_pool TEXT", [])
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

        // `active_run_pool_id` persists which pool (if any) a loop's current/
        // last run drew from, so an interrupted run (quota failure, daemon
        // crash) can be resumed against the same pool by every resume path
        // (scheduled autorun, `loop_reset`) instead of falling back to the
        // loop's own bound specs. Older databases predate the column.
        let has_active_run_pool_id: bool = conn
            .query_row(
                "SELECT COUNT(*) FROM pragma_table_info('loops') WHERE name = 'active_run_pool_id'",
                [],
                |row| Ok(row.get::<_, i32>(0)? > 0),
            )
            .unwrap_or(false);
        if !has_active_run_pool_id {
            conn.execute("ALTER TABLE loops ADD COLUMN active_run_pool_id TEXT", [])
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
                "SELECT COUNT(*) FROM pragma_table_info('pool_members') WHERE name = 'group_name'",
                [],
                |row| Ok(row.get::<_, i32>(0)? > 0),
            )
            .unwrap_or(false);
        if !has_group_name {
            conn.execute("ALTER TABLE pool_members ADD COLUMN group_name TEXT", [])
                .map_err(|e| anyhow::anyhow!("Migration failed: {e}"))?;
        }

        Ok(())
    }
}

pub mod achievements;
pub mod agent;
pub mod blueprints;
pub mod ensembles;
pub mod gamification;
pub mod group;
pub mod intelligence;
pub mod last_prompts;
pub mod loops;
pub mod pools;
pub mod project;
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
