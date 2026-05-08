use anyhow::Result;
use std::sync::Arc;

use rmcp::ServiceExt;

use crate::application::notification_service::{DefaultNotificationService, NotificationService};
use crate::application::ports::StateRepository;
use crate::daemon::process::{kill_port_occupant, remove_pid_file, write_pid_file};
use crate::daemon::TaskTriggerHandler;
use crate::db::Database;
use crate::executor::Executor;
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
    let db = Arc::new(Database::new(&data_dir.join("background_agents.db"))?);
    let notification_service: Arc<dyn NotificationService> = Arc::new(DefaultNotificationService);
    let executor = Arc::new(Executor::new(
        Arc::clone(&db),
        Arc::clone(&notification_service),
    ));
    let watcher_engine = Arc::new(WatcherEngine::new(Arc::clone(&db), Arc::clone(&executor)));
    let sync_manager = Arc::new(SyncManager::new(Arc::clone(&db)));

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

    if let Err(e) = watcher_engine.reload_from_db().await {
        tracing::error!("Failed to reload watchers: {}", e);
    }

    startup_personal_rag(Arc::clone(&ingestion), &data_dir).await;

    let cron_scheduler = Arc::new(CronScheduler::new(Arc::clone(&db), Arc::clone(&executor)));
    let scheduler_notify = cron_scheduler.notifier();
    let scheduler_cancel = Arc::clone(&cron_scheduler).start();

    let handler_db = Arc::clone(&db);
    let handler_executor = Arc::clone(&executor);
    let handler_watcher_engine = Arc::clone(&watcher_engine);
    let handler_scheduler_notify = Arc::clone(&scheduler_notify);
    let handler_sync_manager = Arc::clone(&sync_manager);

    let ct = tokio_util::sync::CancellationToken::new();

    let service = rmcp::transport::streamable_http_server::StreamableHttpService::new(
        move || {
            Ok(TaskTriggerHandler::new(
                Arc::clone(&handler_db),
                Arc::clone(&handler_executor),
                Arc::clone(&handler_watcher_engine),
                Arc::clone(&handler_scheduler_notify),
                Arc::clone(&notification_service),
                Arc::clone(&handler_sync_manager),
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

    let router = axum::Router::new().nest_service("/mcp", service);
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
    watcher_engine.stop_all().await;
    remove_pid_file(&data_dir);
    crate::domain::notification::clear_notifications_on_exit();
    tracing::info!("Daemon stopped");

    Ok(())
}

pub(crate) async fn run_stdio_server() -> Result<()> {
    init_tracing();
    tracing::info!("Starting in stdio MCP transport mode");

    let data_dir = crate::ensure_data_dir()?;
    let db = Arc::new(Database::new(&data_dir.join("background_agents.db"))?);
    let notification_service: Arc<dyn NotificationService> = Arc::new(DefaultNotificationService);
    let executor = Arc::new(Executor::new(
        Arc::clone(&db),
        Arc::clone(&notification_service),
    ));
    let watcher_engine = Arc::new(WatcherEngine::new(Arc::clone(&db), Arc::clone(&executor)));
    let sync_manager = Arc::new(SyncManager::new(Arc::clone(&db)));

    let ingestion = Arc::new(IngestionManager::new(Arc::clone(&db), data_dir.clone()));
    let _ingestion_cancel = Arc::clone(&ingestion).start();

    startup_personal_rag(Arc::clone(&ingestion), &data_dir).await;

    if let Err(e) = watcher_engine.reload_from_db().await {
        tracing::error!("Failed to reload watchers: {}", e);
    }

    let cron_scheduler = Arc::new(CronScheduler::new(Arc::clone(&db), Arc::clone(&executor)));
    let scheduler_notify = cron_scheduler.notifier();
    let _scheduler_cancel = Arc::clone(&cron_scheduler).start();

    let handler = TaskTriggerHandler::new(
        Arc::clone(&db),
        Arc::clone(&executor),
        Arc::clone(&watcher_engine),
        scheduler_notify,
        Arc::clone(&notification_service),
        sync_manager,
        0,
    );

    let transport = rmcp::transport::stdio();
    let server = handler.serve(transport).await?;
    tracing::info!("MCP stdio server started");

    server.waiting().await?;

    cron_scheduler.stop();
    watcher_engine.stop_all().await;
    crate::domain::notification::clear_notifications_on_exit();
    tracing::info!("Stdio server stopped");

    Ok(())
}

/// Scan the personal RAG root for existing files, enqueue them, and start the watcher.
async fn startup_personal_rag(ingestion: Arc<IngestionManager>, data_dir: &std::path::Path) {
    let config = crate::domain::canopy_config::CanopyConfig::load(data_dir);
    let personal_roots: Vec<std::path::PathBuf> = config
        .rag_personal_dirs
        .iter()
        .map(std::path::PathBuf::from)
        .collect();

    if personal_roots.is_empty() {
        tracing::info!("startup_personal_rag: no directories configured, skipping");
        return;
    }

    for root in &personal_roots {
        if let Err(e) = std::fs::create_dir_all(root) {
            tracing::warn!("Could not create personal RAG dir {:?}: {e}", root);
        }
    }

    // Reload any items already in the DB queue from a previous session.
    if let Ok(pending) = ingestion.db_pending_queue() {
        for path in &pending {
            ingestion.enqueue(path).await;
        }
        if !pending.is_empty() {
            tracing::info!(
                "startup_personal_rag: reloaded {} pending queue items",
                pending.len()
            );
        }
    }

    // Scan existing files across all personal roots.
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
            if crate::rag::chunker::detect_lang(&path_str).is_some() {
                ingestion.enqueue(&path_str).await;
                queued += 1;
            }
        }
    }
    if queued > 0 {
        tracing::info!("startup_personal_rag: queued {queued} existing file(s) for indexing");
    }

    // Reconcile orphan chunks from files deleted while the daemon was offline.
    ingestion.reconcile_orphan_chunks().await;

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
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive(tracing_subscriber::filter::LevelFilter::INFO.into()),
        )
        .with_target(false)
        .init();
}
