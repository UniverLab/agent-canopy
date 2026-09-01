use anyhow::Result;
use std::sync::Arc;

use rmcp::ServiceExt;

use crate::application::notification_service::{DefaultNotificationService, NotificationService};
use crate::application::ports::StateRepository;
use crate::daemon::health_routine::HealthRoutine;
use crate::daemon::process::{
    acquire_daemon_lock, kill_port_occupant, remove_pid_file, write_pid_file,
};
use crate::daemon::TaskTriggerHandler;
use crate::db::Database;
use crate::domain::db_paths::database_path;
use crate::executor::Executor;
use crate::loop_engine::LoopEngine;
use crate::rag::ingestion::IngestionManager;
use crate::scheduler::cron_scheduler::CronScheduler;
use crate::sync_manager::SyncManager;
use crate::watchers::WatcherEngine;

pub(crate) async fn run_http_server(port_override: Option<u16>) -> Result<()> {
    crate::domain::notification::register_aumid();
    crate::domain::notification::clear_stale_notifications();
    init_tracing();

    let port = crate::resolve_port(port_override);
    let data_dir = crate::ensure_data_dir()?;
    let _daemon_lock = acquire_daemon_lock(&data_dir)?;
    let db = Arc::new(Database::new(&database_path(&data_dir))?);
    if let Err(e) = crate::domain::prompts::seed_builtin_prompt_presets(&data_dir) {
        tracing::warn!("Could not seed builtin prompt presets: {e}");
    }
    let notification_service: Arc<dyn NotificationService> = Arc::new(DefaultNotificationService);
    let executor = Arc::new(Executor::new(
        Arc::clone(&db),
        Arc::clone(&notification_service),
    ));
    let sync_manager = Arc::new(SyncManager::new(Arc::clone(&db)));
    let canopy_config = crate::domain::canopy_config::CanopyConfig::load(&data_dir);
    let dynamic_skills = Arc::new(crate::dynamic_skills::SkillStore::from_config(
        &data_dir,
        &canopy_config.skills,
    ));
    let loop_engine = Arc::new(
        LoopEngine::new(Arc::clone(&db), Arc::clone(&notification_service))
            .with_ensemble_concurrency_cap(canopy_config.ensemble_concurrency_cap)
            .with_spec_attempt_limit(canopy_config.spec_attempt_limit)
            .with_dynamic_skills(Arc::clone(&dynamic_skills)),
    );
    let watcher_engine = Arc::new(WatcherEngine::new(
        Arc::clone(&db),
        Arc::clone(&executor),
        Arc::clone(&loop_engine),
    ));

    let ingestion = Arc::new(IngestionManager::new(Arc::clone(&db), data_dir.clone()));
    let _ingestion_cancel = Arc::clone(&ingestion).start();

    tracing::info!(
        "canopy v{} starting on port {}",
        env!("CARGO_PKG_VERSION"),
        port
    );

    write_pid_file(&data_dir)?;

    db.set_state("port", &port.to_string())?;
    db.set_state("version", env!("CARGO_PKG_VERSION"))?;
    db.set_state("last_start", &chrono::Utc::now().to_rfc3339())?;

    match db.reconcile_orphaned_loops(&data_dir) {
        Ok(count) if count > 0 => {
            tracing::warn!(
                "Reconciled {} loop(s) left running by a previous daemon",
                count
            );
        }
        Ok(_) => {}
        Err(e) => tracing::error!("Failed to reconcile orphaned loops: {}", e),
    }

    match db.reconcile_stranded_queue_specs() {
        Ok(count) if count > 0 => {
            tracing::warn!(
                "Reconciled {} stranded queue spec(s) left running by a previous daemon",
                count
            );
        }
        Ok(_) => {}
        Err(e) => tracing::error!("Failed to reconcile stranded queue specs: {}", e),
    }

    if let Err(e) = watcher_engine.reload_from_db().await {
        tracing::error!("Failed to reload watchers: {}", e);
    }

    startup_personal_rag(Arc::clone(&ingestion), &data_dir).await;

    let cron_scheduler = Arc::new(CronScheduler::with_loops(
        Arc::clone(&db),
        Arc::clone(&executor),
        Arc::clone(&loop_engine),
    ));
    let scheduler_notify = cron_scheduler.notifier();
    let scheduler_cancel = Arc::clone(&cron_scheduler).start();

    let health_routine = Arc::new(HealthRoutine::new(Arc::clone(&db), data_dir.clone()));
    let health_routine_cancel = health_routine.start();

    let announcements_cancel = if canopy_config.announcements_enabled {
        let client = Arc::new(crate::daemon::announcements::AnnouncementsClient::new(
            Arc::clone(&db),
            Arc::clone(&notification_service),
        ));
        Some(client.start())
    } else {
        None
    };

    let handler_db = Arc::clone(&db);
    let handler_executor = Arc::clone(&executor);
    let handler_watcher_engine = Arc::clone(&watcher_engine);
    let handler_scheduler_notify = Arc::clone(&scheduler_notify);
    let handler_sync_manager = Arc::clone(&sync_manager);
    let handler_loop_engine = Arc::clone(&loop_engine);
    let handler_ingestion = Arc::clone(&ingestion);
    let handler_dynamic_skills = Arc::clone(&dynamic_skills);

    let ct = tokio_util::sync::CancellationToken::new();

    let service = rmcp::transport::streamable_http_server::StreamableHttpService::new(
        move || {
            Ok(TaskTriggerHandler::new(
                Arc::clone(&handler_db),
                Arc::clone(&handler_executor),
                Arc::clone(&handler_watcher_engine),
                Arc::clone(&handler_scheduler_notify),
                Arc::clone(&handler_loop_engine),
                Arc::clone(&notification_service),
                Arc::clone(&handler_sync_manager),
                Arc::clone(&handler_ingestion),
                Arc::clone(&handler_dynamic_skills),
                port,
            ))
        },
        {
            let mut mgr =
                rmcp::transport::streamable_http_server::session::local::LocalSessionManager::default();
            mgr.session_config.keep_alive = None;
            mgr.into()
        },
        {
            #[allow(clippy::field_reassign_with_default)]
            {
                let mut cfg =
                    rmcp::transport::streamable_http_server::StreamableHttpServerConfig::default();
                cfg.cancellation_token = ct.child_token();
                cfg
            }
        },
    );

    let router = axum::Router::new()
        .nest_service("/mcp", service)
        .layer(axum::middleware::from_fn(log_mcp_error_responses));
    let bind_addr = format!("127.0.0.1:{port}");

    kill_port_occupant(port);

    let tcp_listener = tokio::net::TcpListener::bind(&bind_addr).await?;

    tracing::info!(
        "Streamable HTTP MCP server listening on http://{}/mcp",
        bind_addr
    );

    axum::serve(tcp_listener, router)
        .with_graceful_shutdown(async move {
            shutdown_signal().await;
            tracing::info!("Shutdown signal received");
            ct.cancel();
        })
        .await?;

    scheduler_cancel.cancel();
    health_routine_cancel.cancel();
    if let Some(cancel) = announcements_cancel {
        cancel.cancel();
    }
    watcher_engine.stop_all().await;
    terminate_all_running_node_processes_at_shutdown(&db).await;
    remove_pid_file(&data_dir);
    crate::domain::notification::clear_notifications_on_exit();
    tracing::info!("Daemon stopped");

    Ok(())
}

/// rmcp's `/mcp` handler returns 404 "Session not found" (and other non-2xx
/// statuses) without ever calling `tracing::` — see the two 404 branches in
/// `rmcp::transport::streamable_http_server::tower`. That silence is exactly
/// what let a bridge's stale post-restart session id fail forever without a
/// trace in daemon.log. This logs the status and whether a session id was
/// attached (never the header value, and never the auth token) for any
/// `/mcp` response with status >= 400.
async fn log_mcp_error_responses(
    request: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    let session_id_present = request.headers().contains_key("mcp-session-id");
    let response = next.run(request).await;
    let status = response.status();
    if status.as_u16() >= 400 {
        tracing::warn!(
            status = %status,
            session_id_present,
            "/mcp request failed"
        );
    }
    response
}

/// Terminate every loop node run's process on the machine (B12), so a
/// graceful daemon shutdown never leaves a `mimo run`/check process behind
/// the way an abandoned timeout used to. Correct only because it is called
/// exactly once, at daemon shutdown, from the sole process that ever holds
/// the daemon lock (`acquire_daemon_lock` in [`run_http_server`]) — at that
/// point every `Running` node run in the database genuinely is this
/// process's own, so scanning all of them needs no ownership filter. Any
/// caller without that guarantee (a bridge, an embedded stdio server, a CLI
/// subcommand) must never call this; see the module-level invariant that
/// only the daemon lifecycle process may act on graphs globally.
/// `SIGTERM`s every process up front, waits out a single shared grace
/// period, then `SIGKILL`s survivors — awaited inline (unlike the detached
/// `terminate_process_group_async` used elsewhere) because the daemon
/// process is about to exit, so a detached grace-kill task would never get
/// to fire, and a *sequential* terminate-and-wait per process would
/// multiply the shutdown delay by the number of processes instead of
/// bounding it by one grace period total.
async fn terminate_all_running_node_processes_at_shutdown(db: &Database) {
    let Ok(runs) = db.list_all_running_loop_runs() else {
        return;
    };
    if runs.is_empty() {
        return;
    }

    #[cfg(unix)]
    for run in &runs {
        if let Some(pid) = run.pid {
            let _ = crate::daemon::process::send_signal_to_group(pid as i32, libc::SIGTERM);
        }
    }
    #[cfg(unix)]
    {
        tokio::time::sleep(crate::daemon::process::KILL_GRACE).await;
        for run in &runs {
            if let Some(pid) = run.pid {
                let _ = crate::daemon::process::send_signal_to_group(pid as i32, libc::SIGKILL);
            }
        }
    }

    for run in &runs {
        let _ = db.update_loop_run_result(
            &run.id,
            crate::domain::loops::LoopRunStatus::Fail,
            Some(&serde_json::json!({ "terminated": true, "reason": "daemon shutdown" })),
            Some(chrono::Utc::now()),
        );
    }
}

pub(crate) async fn run_stdio_server() -> Result<()> {
    init_tracing();
    tracing::info!("Starting in stdio MCP transport mode");

    let data_dir = crate::ensure_data_dir()?;
    let db = Arc::new(Database::new_safe(&database_path(&data_dir), &data_dir)?);
    let startup = stdio_server_startup(Arc::clone(&db), &data_dir).await;

    let handler = TaskTriggerHandler::new(
        Arc::clone(&db),
        Arc::clone(&startup.executor),
        Arc::clone(&startup.watcher_engine),
        startup.scheduler_notify,
        Arc::clone(&startup.loop_engine),
        Arc::clone(&startup.notification_service),
        Arc::clone(&startup.sync_manager),
        Arc::clone(&startup.ingestion),
        Arc::clone(&startup.dynamic_skills),
        0,
    );

    let transport = rmcp::transport::stdio();
    let server = handler.serve(transport).await?;
    tracing::info!("MCP stdio server started");

    server.waiting().await?;

    startup.cron_scheduler.stop();
    startup.watcher_engine.stop_all().await;
    crate::domain::notification::clear_notifications_on_exit();
    tracing::info!("Stdio server stopped");

    Ok(())
}

/// Everything `run_stdio_server` needs beyond `db` itself: the handler's
/// dependencies plus the background-task guards that must outlive the serve
/// loop. Split out from `run_stdio_server` so a test can drive this
/// DB-touching startup sequence directly, without a real stdio transport, and
/// assert it never mutates loop state — see the doc comment on
/// `Database::reconcile_orphaned_loops` for the incident this guards
/// against: this function deliberately does not call `reconcile_orphaned_loops`
/// or `reconcile_stranded_queue_specs`. A stdio server is not the daemon and
/// must never perform daemon-lifecycle graph recovery.
struct StdioServerStartup {
    executor: Arc<Executor>,
    watcher_engine: Arc<WatcherEngine>,
    notification_service: Arc<dyn NotificationService>,
    sync_manager: Arc<SyncManager>,
    loop_engine: Arc<LoopEngine>,
    ingestion: Arc<IngestionManager>,
    dynamic_skills: Arc<crate::dynamic_skills::SkillStore>,
    cron_scheduler: Arc<CronScheduler>,
    scheduler_notify: Arc<tokio::sync::Notify>,
    _ingestion_cancel: tokio_util::sync::CancellationToken,
    _scheduler_cancel: tokio_util::sync::CancellationToken,
    _health_routine_cancel: tokio_util::sync::CancellationToken,
}

async fn stdio_server_startup(db: Arc<Database>, data_dir: &std::path::Path) -> StdioServerStartup {
    if let Err(e) = crate::domain::prompts::seed_builtin_prompt_presets(data_dir) {
        tracing::warn!("Could not seed builtin prompt presets: {e}");
    }
    let notification_service: Arc<dyn NotificationService> = Arc::new(DefaultNotificationService);
    let executor = Arc::new(Executor::new(
        Arc::clone(&db),
        Arc::clone(&notification_service),
    ));
    let sync_manager = Arc::new(SyncManager::new(Arc::clone(&db)));
    let canopy_config = crate::domain::canopy_config::CanopyConfig::load(data_dir);
    let dynamic_skills = Arc::new(crate::dynamic_skills::SkillStore::from_config(
        data_dir,
        &canopy_config.skills,
    ));
    let loop_engine = Arc::new(
        LoopEngine::new(Arc::clone(&db), Arc::clone(&notification_service))
            .with_ensemble_concurrency_cap(canopy_config.ensemble_concurrency_cap)
            .with_spec_attempt_limit(canopy_config.spec_attempt_limit)
            .with_dynamic_skills(Arc::clone(&dynamic_skills)),
    );
    let watcher_engine = Arc::new(WatcherEngine::new(
        Arc::clone(&db),
        Arc::clone(&executor),
        Arc::clone(&loop_engine),
    ));

    let ingestion = Arc::new(IngestionManager::new(
        Arc::clone(&db),
        data_dir.to_path_buf(),
    ));
    let ingestion_cancel = Arc::clone(&ingestion).start();

    // No `reconcile_orphaned_loops` / `reconcile_stranded_queue_specs` call
    // here: this is a stdio MCP server, not the daemon, and it must never
    // perform daemon-lifecycle graph recovery — see the doc comment on
    // `Database::reconcile_orphaned_loops` for the incident this guards
    // against.
    // No announcements client here: this is a stdio MCP server, not the
    // daemon, and the announcements WebSocket is a daemon-only background task.
    startup_personal_rag(Arc::clone(&ingestion), data_dir).await;

    if let Err(e) = watcher_engine.reload_from_db().await {
        tracing::error!("Failed to reload watchers: {}", e);
    }

    let cron_scheduler = Arc::new(CronScheduler::with_loops(
        Arc::clone(&db),
        Arc::clone(&executor),
        Arc::clone(&loop_engine),
    ));
    let scheduler_notify = cron_scheduler.notifier();
    let scheduler_cancel = Arc::clone(&cron_scheduler).start();

    let health_routine = Arc::new(HealthRoutine::new(Arc::clone(&db), data_dir.to_path_buf()));
    let health_routine_cancel = health_routine.start();

    StdioServerStartup {
        executor,
        watcher_engine,
        notification_service,
        sync_manager,
        loop_engine,
        ingestion,
        dynamic_skills,
        cron_scheduler,
        scheduler_notify,
        _ingestion_cancel: ingestion_cancel,
        _scheduler_cancel: scheduler_cancel,
        _health_routine_cancel: health_routine_cancel,
    }
}

/// Scan the personal RAG root for existing files, enqueue them, and start the watcher.
/// Version of the chunking algorithm; bump to force a full re-index.
/// v2: merge lexically similar neighbors (size-capped) instead of dissimilar ones.
const RAG_CHUNKING_VERSION: &str = "v2";

async fn startup_personal_rag(ingestion: Arc<IngestionManager>, data_dir: &std::path::Path) {
    let config = crate::domain::canopy_config::CanopyConfig::load(data_dir);
    let personal_roots: Vec<std::path::PathBuf> = config
        .rag_personal_dirs
        .iter()
        .map(std::path::PathBuf::from)
        .collect();

    if personal_roots.is_empty() {
        tracing::info!("startup_personal_rag: no directories configured, skipping");
        // Still initialize snapshot even if no RAG dirs configured
        ingestion.refresh_snapshot().await;
        return;
    }

    for root in &personal_roots {
        if let Err(e) = std::fs::create_dir_all(root) {
            tracing::warn!("Could not create personal RAG dir {:?}: {e}", root);
        }
    }

    // ── Model-change detection ──────────────────────────────────────────────
    // If the configured embeddings model differs from the last run, wipe the
    // vector store and clear the queue so everything is re-indexed with the
    // new model.  This covers both dimension changes (already handled inside
    // `open_at`) *and* same-dimension model swaps where old vectors would give
    // wrong results.
    let current_model = config.embeddings_model.trim().to_string();
    if !current_model.is_empty() {
        let last_model = ingestion
            .db()
            .get_state("rag_last_model")
            .ok()
            .flatten()
            .unwrap_or_default();

        if !last_model.is_empty() && last_model != current_model {
            tracing::warn!(
                "startup_personal_rag: embeddings model changed '{}' → '{}' \
                 — wiping vector store and clearing queue for full re-index",
                last_model,
                current_model
            );
            if let Err(e) =
                crate::rag::ingestion::wipe_lancedb(ingestion.db(), "model change").await
            {
                tracing::error!("startup_personal_rag: LanceDB wipe failed: {e:#}");
            }
            ingestion.clear_queue().await;
        }

        let _ = ingestion.db().set_state("rag_last_model", &current_model);
    }

    // ── Chunking-algorithm change detection ─────────────────────────────────
    // Bump RAG_CHUNKING_VERSION whenever chunk boundaries change (e.g. the
    // v2 switch from merge-dissimilar to merge-similar semantics). Existing
    // chunks were produced by the old algorithm, so wipe and re-index.
    let last_chunking = ingestion
        .db()
        .get_state("rag_chunking_version")
        .ok()
        .flatten()
        .unwrap_or_default();
    if last_chunking != RAG_CHUNKING_VERSION {
        tracing::warn!(
            "startup_personal_rag: chunking algorithm changed '{}' → '{}' \
             — wiping vector store and clearing queue for full re-index",
            if last_chunking.is_empty() {
                "v1"
            } else {
                &last_chunking
            },
            RAG_CHUNKING_VERSION
        );
        if let Err(e) =
            crate::rag::ingestion::wipe_lancedb(ingestion.db(), "chunking version change").await
        {
            tracing::error!("startup_personal_rag: LanceDB wipe failed: {e:#}");
        }
        ingestion.clear_queue().await;
        let _ = ingestion
            .db()
            .set_state("rag_chunking_version", RAG_CHUNKING_VERSION);
    }

    // ── Ledger vs vector-store reconciliation ───────────────────────────────
    // A hard crash (e.g. a WSL kill) can destroy the LanceDB directory while
    // the SQLite ledger (`rag_file_events`) survives on disk. Pure store
    // deletion doesn't go through the corrupt-store recovery path (which
    // already clears the ledger), so without this check the daemon would
    // trust the stale ledger, queue nothing, and the RAG index would stay
    // silently empty forever.
    ingestion.reconcile_ledger_with_store().await;

    // Reload any items already in the DB queue from a previous session.
    let recovered = ingestion
        .db()
        .requeue_processing_rag_items(chrono::Utc::now().timestamp())
        .unwrap_or(0);
    if recovered > 0 {
        tracing::warn!(
            "startup_personal_rag: recovered {recovered} stale processing queue item(s) after previous crash"
        );
    }

    // Files that have permanently failed indexing should not be silently
    // re-queued by the automatic startup rescan/reload; an explicit user
    // Modify/Create event via the filesystem watcher still gives them a
    // fresh chance.
    let failed = ingestion
        .db()
        .permanently_failed_rag_files()
        .unwrap_or_default();

    if let Ok(pending) = ingestion.db_pending_queue() {
        let mut reloaded = 0usize;
        for path in &pending {
            if failed.contains(path) {
                continue;
            }
            ingestion.enqueue(path).await;
            reloaded += 1;
        }
        if reloaded > 0 {
            tracing::info!("startup_personal_rag: reloaded {reloaded} pending queue items");
        }
    }

    // Scan existing files across all personal roots.
    // Skip files that are already indexed and haven't been modified since.
    let already_indexed = ingestion
        .db()
        .indexed_files_timestamps()
        .unwrap_or_default();
    let patterns = crate::rag::ragignore::load_patterns(data_dir);
    let mut queued = 0usize;
    for root in &personal_roots {
        let walker = walkdir::WalkDir::new(root)
            .follow_links(false)
            .into_iter()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_type().is_file());

        for entry in walker {
            let path = entry.path();
            if crate::rag::ragignore::is_ignored(path, root, &patterns) {
                continue;
            }
            let path_str = path.to_string_lossy().to_string();
            if crate::rag::chunker::detect_lang(&path_str).is_none() {
                continue;
            }
            // Skip if indexed and file hasn't changed since.
            if let Some(&indexed_at) = already_indexed.get(&path_str) {
                let mtime = entry
                    .metadata()
                    .ok()
                    .and_then(|m| m.modified().ok())
                    .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                    .map(|d| d.as_secs() as i64)
                    .unwrap_or(i64::MAX);
                if mtime <= indexed_at {
                    continue;
                }
            }
            if failed.contains(&path_str) {
                continue;
            }
            ingestion.enqueue(&path_str).await;
            queued += 1;
        }
    }
    if queued > 0 {
        tracing::info!("startup_personal_rag: queued {queued} existing file(s) for indexing");
    }

    // Reconcile orphan chunks from files deleted while the daemon was offline.
    ingestion.reconcile_orphan_chunks().await;
    // Keep a persisted snapshot so TUI reads counters without querying LanceDB.
    ingestion.refresh_snapshot().await;

    // Start the filesystem watcher for live updates.
    Arc::clone(&ingestion).start_personal_watcher(&personal_roots);
}

async fn shutdown_signal() {
    let ctrl_c = tokio::signal::ctrl_c();

    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        let mut sigterm =
            signal(SignalKind::terminate()).expect("failed to install SIGTERM handler");

        tokio::select! {
            _ = ctrl_c => {},
            _ = sigterm.recv() => {},
        }
    }

    #[cfg(not(unix))]
    {
        ctrl_c.await.ok();
    }
}

fn init_tracing() {
    // Logs must never touch stdout: in stdio MCP mode it is reserved
    // exclusively for JSON-RPC messages.
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive(tracing_subscriber::filter::LevelFilter::INFO.into()),
        )
        .with_target(false)
        .with_writer(std::io::stderr)
        .init();
}

/// DIAGNOSTIC (temporary): reproduces the reported hang over the *real*
/// transport a node actually uses — a Streamable HTTP POST that goes through
/// `rmcp`'s `LocalSessionManager`/SSE machinery, not a direct in-process call
/// into `TaskTriggerHandler` (which the `diag_concurrent_db_write_during_dispatch`
/// test in `loop_engine.rs` already proved returns in milliseconds even mid-
/// dispatch). Isolates whether the hang lives in the HTTP/session layer.
#[cfg(test)]
mod hang_repro {
    use super::*;
    use crate::application::notification_service::{
        DefaultNotificationService, NotificationService,
    };
    use crate::daemon::TaskTriggerHandler;
    use crate::domain::loops::{
        Loop, LoopNode, LoopNodeKind, LoopSpec, LoopSpecStatus, LoopStatus,
    };
    use crate::executor::Executor;
    use crate::loop_engine::LoopEngine;
    use crate::rag::ingestion::IngestionManager;
    use crate::sync_manager::SyncManager;
    use crate::watchers::WatcherEngine;
    use std::os::unix::fs::PermissionsExt;

    /// Serializes tests in this module that touch `CANOPY_HOME_OVERRIDE`
    /// (process-wide env var) against each other. Mirrors the `HomeGuard`
    /// pattern in `loop_engine.rs`'s test module (a distinct static — this is
    /// a diagnostic test run in isolation, not meant to coexist with the
    /// wider suite's parallelism).
    static HOME_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn setup_sleeping_cli_home() -> tempfile::TempDir {
        let fake_home = tempfile::tempdir().unwrap();
        let canopy_dir = fake_home.path().join(".canopy");
        std::fs::create_dir_all(&canopy_dir).unwrap();
        let script = fake_home.path().join("sleep-cli.sh");
        std::fs::write(
            &script,
            "#!/bin/sh\nsleep \"${SLEEP_SECONDS:-4}\"\necho done\nexit 0\n",
        )
        .unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();

        let config = crate::domain::canopy_config::CanopyConfig {
            configured_at: Some(chrono::Utc::now().to_rfc3339()),
            clis: vec![crate::domain::cli_config::CliConfig {
                name: "sleep-cli".to_string(),
                binary: script.to_string_lossy().to_string(),
                headless_mode: "-c".to_string(),
                model_flag: None,
                supports_working_dir: false,
                working_dir_flag: None,
                env_vars: std::collections::HashMap::new(),
                interactive_args: None,
                fallback_interactive_args: None,
                resume_args: None,
                session_list_cmd: None,
                session_resume_cmd: None,
                accent_color: None,
                yolo_flag: None,
                prompt_via_stdin: false,
                ..Default::default()
            }],
            ..Default::default()
        };
        config.save(&canopy_dir).unwrap();
        fake_home
    }

    /// Spawn a bare-bones Streamable HTTP MCP server (same construction as
    /// `run_http_server`, minus pid-file/daemon-lock/graceful-shutdown
    /// machinery) bound to an OS-chosen port. Returns the bound port and the
    /// pieces the test needs to drive a concurrent dispatch directly.
    async fn spawn_test_server(db: Arc<Database>) -> (u16, Arc<LoopEngine>) {
        let notif: Arc<dyn NotificationService> = Arc::new(DefaultNotificationService);
        let executor = Arc::new(Executor::new(Arc::clone(&db), Arc::clone(&notif)));
        let sync_manager = Arc::new(SyncManager::new(Arc::clone(&db)));
        let loop_engine = Arc::new(LoopEngine::new(Arc::clone(&db), Arc::clone(&notif)));
        let watcher_engine = Arc::new(WatcherEngine::new(
            Arc::clone(&db),
            Arc::clone(&executor),
            Arc::clone(&loop_engine),
        ));
        let tmp = tempfile::tempdir().unwrap();
        let ingestion = Arc::new(IngestionManager::new(
            Arc::clone(&db),
            tmp.path().to_path_buf(),
        ));
        let dynamic_skills = Arc::new(crate::dynamic_skills::SkillStore::new(
            tmp.path().join("skills"),
            Vec::new(),
            15,
        ));
        let cron_scheduler = Arc::new(crate::scheduler::cron_scheduler::CronScheduler::with_loops(
            Arc::clone(&db),
            Arc::clone(&executor),
            Arc::clone(&loop_engine),
        ));
        let scheduler_notify = cron_scheduler.notifier();
        let _scheduler_cancel = Arc::clone(&cron_scheduler).start();

        let handler_db = Arc::clone(&db);
        let handler_executor = Arc::clone(&executor);
        let handler_watcher_engine = Arc::clone(&watcher_engine);
        let handler_scheduler_notify = Arc::clone(&scheduler_notify);
        let handler_sync_manager = Arc::clone(&sync_manager);
        let handler_loop_engine = Arc::clone(&loop_engine);
        let handler_notif = Arc::clone(&notif);
        let handler_ingestion = Arc::clone(&ingestion);
        let handler_dynamic_skills = Arc::clone(&dynamic_skills);

        let service = rmcp::transport::streamable_http_server::StreamableHttpService::new(
            move || {
                Ok(TaskTriggerHandler::new(
                    Arc::clone(&handler_db),
                    Arc::clone(&handler_executor),
                    Arc::clone(&handler_watcher_engine),
                    Arc::clone(&handler_scheduler_notify),
                    Arc::clone(&handler_loop_engine),
                    Arc::clone(&handler_notif),
                    Arc::clone(&handler_sync_manager),
                    Arc::clone(&handler_ingestion),
                    Arc::clone(&handler_dynamic_skills),
                    0,
                ))
            },
            rmcp::transport::streamable_http_server::session::local::LocalSessionManager::default()
                .into(),
            Default::default(),
        );
        let router = axum::Router::new().nest_service("/mcp", service);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            let _ = axum::serve(listener, router).await;
        });
        // give the listener a beat to start accepting
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        (port, loop_engine)
    }

    struct McpClient {
        client: reqwest::Client,
        endpoint: String,
        session_id: Option<String>,
    }

    impl McpClient {
        fn new(port: u16) -> Self {
            Self {
                client: reqwest::Client::new(),
                endpoint: format!("http://127.0.0.1:{port}/mcp"),
                session_id: None,
            }
        }

        /// POST one JSON-RPC message, return the concatenated `data:` payload(s)
        /// of the SSE response body (mirrors `bridge.rs`'s `forward_request`).
        async fn post(&mut self, body: serde_json::Value) -> anyhow::Result<String> {
            let mut req = self
                .client
                .post(&self.endpoint)
                .header(reqwest::header::CONTENT_TYPE, "application/json")
                .header(
                    reqwest::header::ACCEPT,
                    "application/json, text/event-stream",
                )
                .body(body.to_string());
            if let Some(sid) = &self.session_id {
                req = req.header("mcp-session-id", sid);
            }
            let resp = req.send().await?;
            if let Some(sid) = resp.headers().get("mcp-session-id") {
                self.session_id = Some(sid.to_str()?.to_string());
            }
            let text = resp.text().await?;
            let mut out = String::new();
            for line in text.lines() {
                if let Some(data) = line.strip_prefix("data:") {
                    out.push_str(data.trim());
                }
            }
            Ok(out)
        }

        async fn initialize(&mut self) -> anyhow::Result<()> {
            self.post(serde_json::json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "initialize",
                "params": {
                    "protocolVersion": "2025-06-18",
                    "capabilities": {},
                    "clientInfo": {"name": "repro", "version": "0.1"}
                }
            }))
            .await?;
            // notifications/initialized — no id, server responds 202 Accepted.
            self.post(serde_json::json!({
                "jsonrpc": "2.0",
                "method": "notifications/initialized"
            }))
            .await?;
            Ok(())
        }

        async fn call_tool(
            &mut self,
            name: &str,
            args: serde_json::Value,
        ) -> anyhow::Result<String> {
            self.post(serde_json::json!({
                "jsonrpc": "2.0",
                "id": 2,
                "method": "tools/call",
                "params": {"name": name, "arguments": args}
            }))
            .await
        }
    }

    fn insert_loop_and_sleeping_node(db: &Database, workdir: &str) -> (String, String) {
        let loop_id = "wf-hang-repro".to_string();
        let spec_id = "spec-hang-repro".to_string();
        db.insert_loop(&Loop {
            archived: false,
            paused_by_reconciliation: false,
            id: loop_id.clone(),
            name: "Repro Loop".to_string(),
            description: None,
            workdir: workdir.to_string(),
            status: LoopStatus::Draft,
            trigger: None,
            created_at: chrono::Utc::now(),
            started_at: None,
            completed_at: None,
            autorun_at: None,
            auto_continue_at: None,
            auto_continue_action: None,
            active_run_queue_id: None,
            on_completed: None,
        })
        .unwrap();
        db.insert_loop_spec(&LoopSpec {
            id: spec_id.clone(),
            loop_id: Some(loop_id.clone()),
            name: "Spec".to_string(),
            description: Some("desc".to_string()),
            position: 1,
            parallelizable: false,
            status: LoopSpecStatus::Pending,
            started_at: None,
            completed_at: None,
            spec_start_head: None,
            spec_committed_head: None,
            workdir: None,
            completed_via: None,
            completed_via_reason: None,
            completed_via_at: None,
        })
        .unwrap();
        db.insert_loop_node(&LoopNode {
            id: "node-sleep".to_string(),
            spec_id: Some(spec_id.clone()),
            loop_id: None,
            name: "sleep".to_string(),
            kind: LoopNodeKind::Agent,
            config: serde_json::json!({
                "platform": "sleep-cli",
                "timeout_minutes": 1,
            }),
            position: 1,
            created_at: chrono::Utc::now(),
        })
        .unwrap();
        (loop_id, spec_id)
    }

    /// Regression (a): while a node is mid-dispatch on loop L (agent process
    /// running, loop status == `Running`), a *second*, independent MCP
    /// session — standing in for that node's own bridge connection calling
    /// back into the daemon — calls `loop_schedule_autorun` for the SAME
    /// loop L over real Streamable HTTP, exactly as a resilience node
    /// scheduling its own loop's resume does in production. The response
    /// must arrive promptly (well under any node timeout) and the schedule
    /// must be durably persisted.
    ///
    /// Also covers requirement 3 (fast-fail): `loop_run` and `loop_reset`
    /// against the SAME loop, mid-dispatch, must reject immediately with an
    /// explanatory error rather than blocking — proven here under a real
    /// concurrent dispatch, not just against a synthetic `LoopStatus::Running`
    /// value.
    #[tokio::test]
    // The HOME_LOCK guard is held for the entire test lifetime so concurrent
    // runs of CANOPY_HOME_OVERRIDE-touching tests can't race on the env var.
    // Holding a `std::sync::MutexGuard` across `.await` points is intentional
    // here (the lock is process-wide and is *meant* to be serializing) — and
    // clippy's await-holding-lock lint can't tell the difference between
    // "unintentional deadlock risk" and "deliberate cross-async test
    // serialization", so opt out at the test level.
    #[allow(clippy::await_holding_lock)]
    async fn node_initiated_autorun_call_during_own_dispatch_over_http() {
        let _home_lock = HOME_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let fake_home = setup_sleeping_cli_home();
        let prev_home = std::env::var("CANOPY_HOME_OVERRIDE").ok();
        unsafe {
            std::env::set_var("CANOPY_HOME_OVERRIDE", fake_home.path());
        }

        let dir = tempfile::tempdir().unwrap();
        let db = Arc::new(Database::new(&dir.path().join("test.db")).unwrap());
        let (loop_id, _spec_id) = insert_loop_and_sleeping_node(&db, &dir.path().to_string_lossy());

        let (port, loop_engine) = spawn_test_server(Arc::clone(&db)).await;

        let dispatch_loop_id = loop_id.clone();
        let dispatch =
            tokio::spawn(async move { loop_engine.run_loop(dispatch_loop_id, None, None).await });

        // Wait until the loop is actually Running (node process spawned).
        for _ in 0..50 {
            if db.get_loop(&loop_id).unwrap().unwrap().status == LoopStatus::Running {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        assert_eq!(
            db.get_loop(&loop_id).unwrap().unwrap().status,
            LoopStatus::Running,
            "dispatch never reached Running before the repro call"
        );

        let mut client = McpClient::new(port);
        client.initialize().await.unwrap();

        let at_dt = chrono::Utc::now() + chrono::Duration::hours(1);
        let at = at_dt.to_rfc3339();
        let start = std::time::Instant::now();
        let result = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            client.call_tool(
                "loop_schedule_autorun",
                serde_json::json!({"loop_id": loop_id, "at": at}),
            ),
        )
        .await;
        let elapsed = start.elapsed();

        let schedule_response = result
            .expect("node-initiated loop_schedule_autorun must respond promptly mid-dispatch")
            .expect("loop_schedule_autorun call must succeed");
        assert!(
            schedule_response.contains("scheduled to autorun"),
            "{schedule_response}"
        );
        assert!(
            elapsed < std::time::Duration::from_secs(5),
            "response took {elapsed:?}, expected well under the node timeout"
        );

        let persisted = db.get_loop(&loop_id).unwrap().unwrap().autorun_at;
        assert_eq!(
            persisted.map(|v| v.timestamp()),
            Some(at_dt.timestamp()),
            "schedule must be durably persisted at the requested instant"
        );

        // Requirement 3: loop_run / loop_reset against the SAME loop, still
        // mid-dispatch, must fail fast (not hang, not queue).
        let run_result = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            client.call_tool("loop_run", serde_json::json!({"loop_id": loop_id})),
        )
        .await
        .expect("loop_run must respond promptly against a running loop")
        .unwrap();
        assert!(
            run_result.contains("already running"),
            "expected fast-fail error, got: {run_result}"
        );

        let reset_result = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            client.call_tool("loop_reset", serde_json::json!({"loop_id": loop_id})),
        )
        .await
        .expect("loop_reset must respond promptly against a running loop")
        .unwrap();
        // The ground truth is the `loop_runs` table, not loop status (see
        // `Database::reset_loop`'s `InFlight` guard) — the refusal now names
        // the node and run still executing rather than just pointing at
        // `loop_pause`.
        assert!(
            reset_result.contains("sleep") && reset_result.contains("still executing"),
            "expected fast-fail error, got: {reset_result}"
        );

        dispatch.abort();
        match prev_home {
            Some(v) => unsafe { std::env::set_var("CANOPY_HOME_OVERRIDE", v) },
            None => unsafe { std::env::remove_var("CANOPY_HOME_OVERRIDE") },
        }
        drop(fake_home);
    }

    /// Regression (b): a quota-shaped implementer failure — modeled here as
    /// the raw CLI message the resilience node would observe — ends with
    /// `autorun_at` set to the *correct* instant, computed deterministically
    /// by the engine rather than by model arithmetic. Exercises the full
    /// `quota_reset_message` path through the real `#[tool]` handler, over
    /// the real HTTP transport.
    #[tokio::test]
    async fn quota_shaped_failure_schedules_correct_autorun_instant() {
        let dir = tempfile::tempdir().unwrap();
        let db = Arc::new(Database::new(&dir.path().join("test.db")).unwrap());
        let (loop_id, _spec_id) = insert_loop_and_sleeping_node(&db, &dir.path().to_string_lossy());
        db.update_loop_status(&loop_id, LoopStatus::Failed, None, Some(chrono::Utc::now()))
            .unwrap();

        let (port, _loop_engine) = spawn_test_server(Arc::clone(&db)).await;
        let mut client = McpClient::new(port);
        client.initialize().await.unwrap();

        let before = chrono::Utc::now();
        let result = client
            .call_tool(
                "loop_schedule_autorun",
                serde_json::json!({
                    "loop_id": loop_id,
                    "quota_reset_message":
                        "You've hit your session limit · resets 1pm (America/Bogota)"
                }),
            )
            .await
            .unwrap();
        let after = chrono::Utc::now();
        assert!(result.contains("scheduled to autorun"), "{result}");

        let persisted = db
            .get_loop(&loop_id)
            .unwrap()
            .unwrap()
            .autorun_at
            .expect("autorun_at must be set");
        // The handler computed `at` from its own `Utc::now()` sometime
        // between `before` and `after`; recomputing against either bound
        // must agree with what got persisted, proving it's the deterministic
        // parser's output (not a model-guessed value).
        let expected_lo = crate::domain::quota_reset::parse_quota_reset_instant(
            "resets 1pm (America/Bogota)",
            before,
        )
        .unwrap();
        let expected_hi = crate::domain::quota_reset::parse_quota_reset_instant(
            "resets 1pm (America/Bogota)",
            after,
        )
        .unwrap();
        assert_eq!(expected_lo.timestamp(), expected_hi.timestamp());
        assert_eq!(persisted.timestamp(), expected_lo.timestamp());
    }

    /// Mutually exclusive params: passing both `at` and `quota_reset_message`
    /// must be rejected up front, not silently prefer one.
    #[tokio::test]
    async fn at_and_quota_reset_message_are_mutually_exclusive() {
        let dir = tempfile::tempdir().unwrap();
        let db = Arc::new(Database::new(&dir.path().join("test.db")).unwrap());
        let (loop_id, _spec_id) = insert_loop_and_sleeping_node(&db, &dir.path().to_string_lossy());

        let (port, _loop_engine) = spawn_test_server(Arc::clone(&db)).await;
        let mut client = McpClient::new(port);
        client.initialize().await.unwrap();

        let result = client
            .call_tool(
                "loop_schedule_autorun",
                serde_json::json!({
                    "loop_id": loop_id,
                    "at": chrono::Utc::now().to_rfc3339(),
                    "quota_reset_message": "resets 1pm (America/Bogota)",
                }),
            )
            .await
            .unwrap();
        assert!(result.contains("not both"), "{result}");
        assert!(
            db.get_loop(&loop_id).unwrap().unwrap().autorun_at.is_none(),
            "rejected call must not have scheduled anything"
        );
    }

    /// Requirement 4: a retried `loop_schedule_autorun` (simulating a lost
    /// ack whose write already committed) must not double-schedule — the
    /// second call with the same loop id + target instant leaves exactly one
    /// pending schedule, at that instant, not a queued second one.
    #[tokio::test]
    async fn retried_schedule_call_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let db = Arc::new(Database::new(&dir.path().join("test.db")).unwrap());
        let (loop_id, _spec_id) = insert_loop_and_sleeping_node(&db, &dir.path().to_string_lossy());

        let (port, _loop_engine) = spawn_test_server(Arc::clone(&db)).await;
        let mut client = McpClient::new(port);
        client.initialize().await.unwrap();

        let at = chrono::Utc::now() + chrono::Duration::hours(1);
        for _ in 0..2 {
            let result = client
                .call_tool(
                    "loop_schedule_autorun",
                    serde_json::json!({"loop_id": loop_id, "at": at.to_rfc3339()}),
                )
                .await
                .unwrap();
            assert!(result.contains("scheduled to autorun"), "{result}");
        }

        let lp = db.get_loop(&loop_id).unwrap().unwrap();
        assert_eq!(lp.autorun_at.map(|v| v.timestamp()), Some(at.timestamp()));
    }
}

#[cfg(test)]
mod mcp_error_logging_tests {
    use super::*;
    use axum::response::IntoResponse;
    use axum::routing::post;
    use std::sync::{Arc as StdArc, Mutex};
    use tracing_subscriber::layer::SubscriberExt;
    use tracing_subscriber::Layer;

    #[derive(Default, Clone)]
    struct CapturedEvents(StdArc<Mutex<Vec<String>>>);

    struct RecordingLayer(CapturedEvents);

    impl<S: tracing::Subscriber> Layer<S> for RecordingLayer {
        fn on_event(
            &self,
            event: &tracing::Event<'_>,
            _ctx: tracing_subscriber::layer::Context<'_, S>,
        ) {
            struct Visitor(String);
            impl tracing::field::Visit for Visitor {
                fn record_debug(
                    &mut self,
                    field: &tracing::field::Field,
                    value: &dyn std::fmt::Debug,
                ) {
                    self.0.push_str(&format!("{}={:?} ", field.name(), value));
                }
            }
            let mut visitor = Visitor(String::new());
            event.record(&mut visitor);
            self.0 .0.lock().unwrap().push(visitor.0);
        }
    }

    async fn always_401(_req: axum::extract::Request) -> axum::response::Response {
        (axum::http::StatusCode::UNAUTHORIZED, "nope").into_response()
    }

    async fn always_200(_req: axum::extract::Request) -> axum::response::Response {
        (axum::http::StatusCode::OK, "ok").into_response()
    }

    /// Spawn a router with only `log_mcp_error_responses` wired in front of a
    /// stub handler, and a recording tracing layer as the thread's default
    /// subscriber. `#[tokio::test]` defaults to a current-thread runtime, so
    /// the spawned server task runs on the same thread as the guard and
    /// observes the same subscriber.
    async fn spawn_with_recording_layer(
        handler: axum::routing::MethodRouter,
    ) -> (u16, CapturedEvents, tracing::subscriber::DefaultGuard) {
        let captured = CapturedEvents::default();
        let subscriber = tracing_subscriber::registry().with(RecordingLayer(captured.clone()));
        let guard = tracing::subscriber::set_default(subscriber);

        let router = axum::Router::new()
            .route("/mcp", handler)
            .layer(axum::middleware::from_fn(log_mcp_error_responses));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            let _ = axum::serve(listener, router).await;
        });
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        (port, captured, guard)
    }

    #[tokio::test]
    async fn logs_status_and_session_presence_never_header_value() {
        let (port, captured, _guard) = spawn_with_recording_layer(post(always_401)).await;

        let client = reqwest::Client::new();
        client
            .post(format!("http://127.0.0.1:{port}/mcp"))
            .header("mcp-session-id", "super-secret-session-value")
            .send()
            .await
            .unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let events = captured.0.lock().unwrap();
        assert!(
            events
                .iter()
                .any(|e| e.contains("status") && e.contains("401")),
            "{events:?}"
        );
        assert!(
            events
                .iter()
                .any(|e| e.contains("session_id_present") && e.contains("true")),
            "{events:?}"
        );
        assert!(
            !events
                .iter()
                .any(|e| e.contains("super-secret-session-value")),
            "header value must never be logged: {events:?}"
        );
    }

    #[tokio::test]
    async fn logs_session_absent_when_no_header_sent() {
        let (port, captured, _guard) = spawn_with_recording_layer(post(always_401)).await;

        let client = reqwest::Client::new();
        client
            .post(format!("http://127.0.0.1:{port}/mcp"))
            .send()
            .await
            .unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let events = captured.0.lock().unwrap();
        assert!(
            events
                .iter()
                .any(|e| e.contains("session_id_present") && e.contains("false")),
            "{events:?}"
        );
    }

    #[tokio::test]
    async fn does_not_log_for_successful_responses() {
        let (port, captured, _guard) = spawn_with_recording_layer(post(always_200)).await;

        let client = reqwest::Client::new();
        client
            .post(format!("http://127.0.0.1:{port}/mcp"))
            .send()
            .await
            .unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let events = captured.0.lock().unwrap();
        let middleware_events: Vec<_> = events
            .iter()
            .filter(|e| e.contains("status") || e.contains("session_id_present"))
            .collect();
        assert!(
            middleware_events.is_empty(),
            "middleware must not log for 200 responses, got: {middleware_events:?}"
        );
    }
}

#[cfg(test)]
mod stdio_startup_reconciliation_tests {
    use super::*;
    use crate::domain::loops::{
        Loop, LoopNode, LoopNodeKind, LoopNodeRun, LoopRunStatus, LoopSpec, LoopSpecStatus,
        LoopStatus,
    };
    use tempfile::{tempdir, NamedTempFile};

    fn test_db() -> Database {
        let tmp = NamedTempFile::new().expect("create temp file");
        let path = tmp.path().to_path_buf();
        std::mem::forget(tmp);
        Database::new(&path).expect("create test db")
    }

    fn init_git_repo(path: &std::path::Path) {
        let run = |args: &[&str]| {
            let status = std::process::Command::new("git")
                .args(args)
                .current_dir(path)
                .env("GIT_AUTHOR_NAME", "Test")
                .env("GIT_AUTHOR_EMAIL", "test@example.com")
                .env("GIT_COMMITTER_NAME", "Test")
                .env("GIT_COMMITTER_EMAIL", "test@example.com")
                .status()
                .expect("git command failed to run");
            assert!(status.success(), "git {:?} failed", args);
        };
        run(&["init", "-q"]);
        run(&["config", "user.name", "Test"]);
        run(&["config", "user.email", "test@example.com"]);
        std::fs::write(path.join("README.md"), "test").unwrap();
        run(&["add", "."]);
        run(&["commit", "-q", "-m", "init"]);
    }

    fn git_head(path: &std::path::Path) -> String {
        let output = std::process::Command::new("git")
            .args(["rev-parse", "HEAD"])
            .current_dir(path)
            .output()
            .expect("git rev-parse failed to run");
        assert!(output.status.success(), "git rev-parse HEAD failed");
        String::from_utf8_lossy(&output.stdout).trim().to_string()
    }

    fn git_is_clean(path: &std::path::Path) -> bool {
        let output = std::process::Command::new("git")
            .args(["status", "--porcelain"])
            .current_dir(path)
            .output()
            .expect("git status failed to run");
        output.stdout.is_empty()
    }

    /// Seeds a `Running` loop with a dangling `running` node run over a dirty
    /// git worktree — exactly the shape `reconcile_orphaned_loops` (called
    /// from a real daemon boot) would pause, interrupt, and mark `Interrupted`
    /// without touching git. Returns the loop id, run id, and the workdir
    /// (kept alive for the caller via the returned `TempDir`). `suffix`
    /// distinguishes multiple graphs seeded into the same database (each
    /// gets its own tempdir workdir, so two calls never collide on ids or
    /// worktree).
    fn seed_running_loop_with_dirty_worktree(
        db: &Database,
        suffix: &str,
    ) -> (String, String, tempfile::TempDir) {
        let dir = tempdir().unwrap();
        init_git_repo(dir.path());
        let head = git_head(dir.path());
        std::fs::write(dir.path().join("truncated.rs"), "fn broken(").unwrap();

        let lp = Loop {
            archived: false,
            paused_by_reconciliation: false,
            id: format!("wf-stdio-startup-{suffix}"),
            name: format!("Stdio startup test loop {suffix}"),
            description: None,
            workdir: dir.path().to_string_lossy().to_string(),
            status: LoopStatus::Running,
            trigger: None,
            created_at: chrono::Utc::now(),
            started_at: None,
            completed_at: None,
            autorun_at: None,
            auto_continue_at: None,
            auto_continue_action: None,
            active_run_queue_id: None,
            on_completed: None,
        };
        let spec = LoopSpec {
            id: format!("spec-stdio-startup-{suffix}"),
            loop_id: Some(lp.id.clone()),
            name: "Spec".to_string(),
            description: None,
            position: 1,
            parallelizable: false,
            status: LoopSpecStatus::Running,
            started_at: None,
            completed_at: None,
            spec_start_head: Some(head),
            workdir: None,
            completed_via: None,
            completed_via_reason: None,
            completed_via_at: None,
            spec_committed_head: None,
        };
        let node = LoopNode {
            id: format!("node-stdio-startup-{suffix}"),
            spec_id: Some(spec.id.clone()),
            loop_id: None,
            name: "Node".to_string(),
            kind: LoopNodeKind::Check,
            config: serde_json::json!({"command": "true", "success_condition": "exit_code_0"}),
            position: 1,
            created_at: chrono::Utc::now(),
        };
        let run = LoopNodeRun {
            id: format!("run-stdio-startup-{suffix}"),
            loop_id: lp.id.clone(),
            spec_id: spec.id.clone(),
            node_id: node.id.clone(),
            status: LoopRunStatus::Running,
            input: None,
            output: None,
            started_at: chrono::Utc::now(),
            completed_at: None,
            iteration: 1,
            pid: None,
            boot_id: None,
            session_id: None,
        };

        db.insert_loop(&lp).unwrap();
        db.insert_loop_spec(&spec).unwrap();
        db.insert_loop_node(&node).unwrap();
        db.insert_loop_run(&run).unwrap();

        (lp.id, run.id, dir)
    }

    /// The regression test for the 2026-08-03 incident: `run_stdio_server`'s
    /// startup sequence (`stdio_server_startup` — the same DB-touching setup
    /// a `canopy bridge` embedded-stdio fallback runs) must never reconcile a
    /// `Running` graph, even though the daemon's own startup path
    /// (`run_http_server`) calls `reconcile_orphaned_loops` at the equivalent
    /// point in its own sequence.
    #[tokio::test]
    async fn stdio_startup_never_reconciles_running_graph() {
        let db = Arc::new(test_db());
        let (loop_id, run_id, dir) = seed_running_loop_with_dirty_worktree(&db, "solo");
        let data_dir = tempdir().unwrap();

        let _startup = stdio_server_startup(Arc::clone(&db), data_dir.path()).await;

        let lp_after = db.get_loop(&loop_id).unwrap().unwrap();
        assert_eq!(
            lp_after.status,
            LoopStatus::Running,
            "stdio startup must not pause a graph the daemon still owns"
        );
        let run_after = db.get_loop_run(&run_id).unwrap().unwrap();
        assert_eq!(
            run_after.status,
            LoopRunStatus::Running,
            "stdio startup must not mark the run interrupted"
        );
        assert!(
            !git_is_clean(dir.path()),
            "stdio startup must not touch the worktree's uncommitted changes"
        );
    }

    /// Concurrency invariant, not just the single-graph regression above:
    /// two graphs in two different workdirs are both `Running` when a third
    /// process (a `canopy bridge` embedded-stdio fallback) runs the stdio
    /// server's startup path. Neither graph's status, run, or worktree may
    /// be touched — graph B's presence must not change what happens to
    /// graph A, and vice versa.
    #[tokio::test]
    async fn stdio_startup_leaves_two_concurrent_graphs_in_different_workdirs_untouched() {
        let db = Arc::new(test_db());
        let (loop_a, run_a, dir_a) = seed_running_loop_with_dirty_worktree(&db, "a");
        let (loop_b, run_b, dir_b) = seed_running_loop_with_dirty_worktree(&db, "b");
        let data_dir = tempdir().unwrap();

        let _startup = stdio_server_startup(Arc::clone(&db), data_dir.path()).await;

        for (loop_id, run_id, dir) in [(&loop_a, &run_a, &dir_a), (&loop_b, &run_b, &dir_b)] {
            let lp_after = db.get_loop(loop_id).unwrap().unwrap();
            assert_eq!(
                lp_after.status,
                LoopStatus::Running,
                "stdio startup must not pause graph '{loop_id}' just because another graph is also live"
            );
            let run_after = db.get_loop_run(run_id).unwrap().unwrap();
            assert_eq!(
                run_after.status,
                LoopRunStatus::Running,
                "stdio startup must not mark graph '{loop_id}''s run interrupted"
            );
            assert!(
                !git_is_clean(dir.path()),
                "stdio startup must not touch graph '{loop_id}''s uncommitted changes"
            );
        }
    }
}
