//! File watcher engine using the `notify` crate.
//!
//! Manages filesystem watchers that trigger CLI executions when
//! specified events occur. Watchers survive agent disconnection
//! and are reloaded from `SQLite` on daemon startup.

use anyhow::Result;
use notify::{
    Config, Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher as NotifyWatcher,
};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::Mutex;

use crate::application::ports::AgentRepository;
use crate::db::Database;
use crate::domain::models::{Agent, Trigger, WatchEvent};
use crate::executor::Executor;

/// Manages all active file system watchers.
pub struct WatcherEngine {
    db: Arc<Database>,
    executor: Arc<Executor>,
    /// Active notify watchers keyed by agent ID.
    active: Arc<Mutex<HashMap<String, ActiveWatcher>>>,
}

struct ActiveWatcher {
    /// The notify watcher handle — dropping this stops the watcher.
    _watcher: RecommendedWatcher,
    #[allow(dead_code)]
    agent: Agent,
}

/// Resolved watch target: the actual path to watch and an optional filename filter.
struct WatchTarget {
    path: PathBuf,
    /// When set, only events whose path filename matches this value are processed.
    file_filter: Option<String>,
}

impl WatcherEngine {
    pub fn new(db: Arc<Database>, executor: Arc<Executor>) -> Self {
        Self {
            db,
            executor,
            active: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Load and start all enabled watch agents from the database.
    pub async fn reload_from_db(&self) -> Result<()> {
        let agents = self.db.list_watch_agents()?;
        tracing::info!(
            "Reloading {} enabled watch agents from database",
            agents.len()
        );
        for agent in agents {
            if let Err(e) = self.start_watcher(&agent).await {
                tracing::error!("Failed to start watcher for agent '{}': {}", agent.id, e);
            }
        }
        Ok(())
    }

    /// Start watching for a specific agent configuration.
    pub async fn start_watcher(&self, agent: &Agent) -> Result<()> {
        let Trigger::Watch {
            path,
            events,
            debounce_seconds,
            recursive,
        } = agent
            .trigger
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("Agent '{}' has no Watch trigger", agent.id))?
        else {
            return Err(anyhow::anyhow!("Agent '{}' trigger is not Watch", agent.id));
        };

        let target = resolve_watch_target(path);
        let mode = watch_mode(*recursive, target.file_filter.is_some());

        let watcher = build_notify_watcher(
            agent,
            events.clone(),
            *debounce_seconds,
            target.file_filter.clone(),
            Arc::clone(&self.executor),
        )?;

        log_watcher_start(&agent.id, path, &target, events, *recursive);

        if !target.path.exists() {
            log_missing_path(&agent.id, path, target.file_filter.is_some());
        }

        let mut watcher = watcher;
        watcher.watch(&target.path, mode)?;

        self.active.lock().await.insert(
            agent.id.clone(),
            ActiveWatcher {
                _watcher: watcher,
                agent: agent.clone(),
            },
        );
        Ok(())
    }

    /// Stop a specific watcher by ID.
    pub async fn stop_watcher(&self, id: &str) -> Result<()> {
        if self.active.lock().await.remove(id).is_some() {
            tracing::info!("Stopped watcher '{}'", id);
        }
        Ok(())
    }

    /// Stop all active watchers.
    pub async fn stop_all(&self) {
        let mut active = self.active.lock().await;
        let count = active.len();
        active.clear();
        tracing::info!("Stopped {} watchers", count);
    }

    pub async fn active_count(&self) -> usize {
        self.active.lock().await.len()
    }

    pub async fn is_active(&self, id: &str) -> bool {
        self.active.lock().await.contains_key(id)
    }
}

// ── Free functions ────────────────────────────────────────────────────────

/// Determine the actual path to watch and an optional filename filter.
///
/// On macOS, FSEvents works at directory level. For single-file targets we
/// watch the parent directory and filter by filename.
fn resolve_watch_target(path: &str) -> WatchTarget {
    let buf = PathBuf::from(path);
    let is_file_target = buf.is_file()
        || (!buf.exists()
            && buf.extension().is_some()
            && buf.parent().map(|p| p.is_dir()).unwrap_or(false));

    if is_file_target {
        let parent = buf.parent().unwrap_or(&buf).to_path_buf();
        let file_filter = buf.file_name().map(|f| f.to_string_lossy().to_string());
        WatchTarget {
            path: parent,
            file_filter,
        }
    } else {
        WatchTarget {
            path: buf,
            file_filter: None,
        }
    }
}

fn watch_mode(recursive: bool, is_file_filter: bool) -> RecursiveMode {
    if is_file_filter || !recursive {
        RecursiveMode::NonRecursive
    } else {
        RecursiveMode::Recursive
    }
}

/// Map a notify `EventKind` to a `WatchEvent`, returning `None` for irrelevant kinds.
fn map_event_kind(kind: &EventKind, agent_id: &str) -> Option<WatchEvent> {
    match kind {
        EventKind::Create(_) => Some(WatchEvent::Create),
        EventKind::Modify(notify::event::ModifyKind::Name(_)) => Some(WatchEvent::Move),
        EventKind::Modify(_) => Some(WatchEvent::Modify),
        EventKind::Remove(_) => Some(WatchEvent::Delete),
        _ => {
            tracing::debug!("Watcher '{}' ignoring event kind: {:?}", agent_id, kind);
            None
        }
    }
}

/// Returns `true` if the event matches the configured watch events.
fn event_matches(evt: WatchEvent, configured: &[WatchEvent]) -> bool {
    configured.contains(&evt)
        || (evt == WatchEvent::Modify && configured.contains(&WatchEvent::Create))
}

/// Build the notify watcher with the event-handling closure.
fn build_notify_watcher(
    agent: &Agent,
    events: Vec<WatchEvent>,
    debounce_secs: u64,
    file_filter: Option<String>,
    executor: Arc<Executor>,
) -> Result<RecommendedWatcher> {
    let id = agent.id.clone();
    let agent = agent.clone();
    let last_trigger: Arc<Mutex<Option<Instant>>> = Arc::new(Mutex::new(None));
    let rt = tokio::runtime::Handle::current();

    let watcher = RecommendedWatcher::new(
        move |res: Result<Event, notify::Error>| {
            handle_notify_event(
                res,
                &id,
                &events,
                file_filter.as_deref(),
                debounce_secs,
                &last_trigger,
                &agent,
                &executor,
                &rt,
            );
        },
        Config::default(),
    )?;
    Ok(watcher)
}

/// Synchronous event handler called by the notify thread.
#[allow(clippy::too_many_arguments)]
fn handle_notify_event(
    res: Result<Event, notify::Error>,
    id: &str,
    events: &[WatchEvent],
    file_filter: Option<&str>,
    debounce_secs: u64,
    last_trigger: &Arc<Mutex<Option<Instant>>>,
    agent: &Agent,
    executor: &Arc<Executor>,
    rt: &tokio::runtime::Handle,
) {
    let event = match res {
        Ok(e) => e,
        Err(e) => {
            tracing::error!("Watcher '{}' error: {}", id, e);
            return;
        }
    };

    if let Some(filter) = file_filter {
        let matches = event.paths.iter().any(|p| {
            p.file_name()
                .map(|f| f.to_string_lossy() == filter)
                .unwrap_or(false)
        });
        if !matches {
            return;
        }
    }

    let Some(evt) = map_event_kind(&event.kind, id) else {
        return;
    };
    if !event_matches(evt, events) {
        return;
    }

    let last_trigger = Arc::clone(last_trigger);
    let executor = Arc::clone(executor);
    let agent = agent.clone();
    let file_path = event
        .paths
        .first()
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_default();
    let evt_str = evt.to_string();

    rt.spawn(async move {
        {
            let mut lt = last_trigger.lock().await;
            if lt
                .map(|t| t.elapsed() < Duration::from_secs(debounce_secs))
                .unwrap_or(false)
            {
                return;
            }
            *lt = Some(Instant::now());
        }
        tracing::info!(
            "Watcher '{}' triggered: {} on {}",
            agent.id,
            evt_str,
            file_path
        );
        if let Err(e) = executor
            .execute_agent_with_context(&agent, &file_path, &evt_str)
            .await
        {
            tracing::error!("Watcher '{}' execution failed: {}", agent.id, e);
        }
    });
}

fn log_watcher_start(
    id: &str,
    original_path: &str,
    target: &WatchTarget,
    events: &[WatchEvent],
    recursive: bool,
) {
    if target.file_filter.is_some() {
        tracing::info!(
            "Started watcher '{}' on file '{}' (via parent dir '{}', events: {:?})",
            id,
            original_path,
            target.path.display(),
            events
        );
    } else {
        tracing::info!(
            "Started watcher '{}' on '{}' (events: {:?}, recursive: {})",
            id,
            original_path,
            events,
            recursive
        );
    }
}

fn log_missing_path(id: &str, path: &str, is_file_filter: bool) {
    if is_file_filter {
        tracing::info!(
            "Watcher '{}': file '{}' does not exist yet, watching parent dir for creation",
            id,
            path
        );
    } else {
        tracing::warn!(
            "Watcher '{}': path '{}' does not exist, watcher will activate when it's created",
            id,
            path
        );
    }
}
