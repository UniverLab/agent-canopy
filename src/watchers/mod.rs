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
use crate::domain::loops::Loop;
use crate::domain::models::{Agent, Trigger, WatchEvent};
use crate::executor::Executor;
use crate::loop_engine::LoopEngine;

/// Manages all active file system watchers.
pub struct WatcherEngine {
    db: Arc<Database>,
    executor: Arc<Executor>,
    /// Loop engine used to launch watch-triggered loops.
    loop_engine: Arc<LoopEngine>,
    /// Active notify watchers keyed by agent ID.
    active: Arc<Mutex<HashMap<String, ActiveWatcher>>>,
    /// Active notify watchers for loops, keyed by loop ID.
    active_loops: Arc<Mutex<HashMap<String, RecommendedWatcher>>>,
}

struct ActiveWatcher {
    /// The notify watcher handle — dropping this stops the watcher.
    _watcher: RecommendedWatcher,
    #[allow(dead_code)]
    agent: Agent,
}

/// What a watcher launches when its debounced event fires.
#[derive(Clone)]
enum FireTarget {
    Agent {
        // Boxed: `Agent` is large relative to the loop variant, so keep the
        // enum small (clippy::large_enum_variant).
        agent: Box<Agent>,
        executor: Arc<Executor>,
    },
    Loop {
        loop_id: String,
        db: Arc<Database>,
        loop_engine: Arc<LoopEngine>,
    },
}

impl FireTarget {
    fn id(&self) -> &str {
        match self {
            FireTarget::Agent { agent, .. } => &agent.id,
            FireTarget::Loop { loop_id, .. } => loop_id,
        }
    }

    /// Run the target. For agents this executes the CLI with event context;
    /// for loops it launches the loop graph fire-and-forget (skipping loops
    /// that are already running/paused so an event does not double-launch).
    async fn fire(self, file_path: String, evt_str: String) {
        match self {
            FireTarget::Agent { agent, executor } => {
                tracing::info!(
                    "Watcher '{}' triggered: {} on {}",
                    agent.id,
                    evt_str,
                    file_path
                );
                if let Err(e) = executor
                    .execute_agent_with_context(agent.as_ref(), &file_path, &evt_str)
                    .await
                {
                    tracing::error!("Watcher '{}' execution failed: {}", agent.id, e);
                }
            }
            FireTarget::Loop {
                loop_id,
                db,
                loop_engine,
            } => match db.get_loop(&loop_id) {
                Ok(Some(lp)) if lp.is_fireable() => {
                    tracing::info!(
                        "Watch loop '{}' triggered: {} on {}",
                        loop_id,
                        evt_str,
                        file_path
                    );
                    loop_engine.start_background(loop_id);
                }
                Ok(Some(_)) => tracing::debug!(
                    "Watch loop '{}' event ignored (loop already running/paused)",
                    loop_id
                ),
                Ok(None) => tracing::warn!("Watch loop '{}' no longer exists", loop_id),
                Err(e) => tracing::error!("Watch loop '{}' lookup failed: {}", loop_id, e),
            },
        }
    }
}

/// Resolved watch target: the actual path to watch and an optional filename filter.
struct WatchTarget {
    path: PathBuf,
    /// When set, only events whose path filename matches this value are processed.
    file_filter: Option<String>,
}

impl WatcherEngine {
    pub fn new(db: Arc<Database>, executor: Arc<Executor>, loop_engine: Arc<LoopEngine>) -> Self {
        Self {
            db,
            executor,
            loop_engine,
            active: Arc::new(Mutex::new(HashMap::new())),
            active_loops: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Load and start all enabled watch agents and watch-triggered loops from
    /// the database.
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

        let loops = self.db.list_watch_loops()?;
        tracing::info!(
            "Reloading {} watch-triggered loops from database",
            loops.len()
        );
        for lp in loops {
            if let Err(e) = self.start_loop_watcher(&lp).await {
                tracing::error!("Failed to start watcher for loop '{}': {}", lp.id, e);
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

        let mut watcher = build_notify_watcher(
            FireTarget::Agent {
                agent: Box::new(agent.clone()),
                executor: Arc::clone(&self.executor),
            },
            events.clone(),
            *debounce_seconds,
            target.file_filter.clone(),
        )?;

        log_watcher_start(&agent.id, path, &target, events, *recursive);

        if !target.path.exists() {
            log_missing_path(&agent.id, path, target.file_filter.is_some());
        }

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

    /// Start watching for a watch-triggered loop. On a matching event the loop
    /// engine launches the loop graph (unless it is already running).
    pub async fn start_loop_watcher(&self, lp: &Loop) -> Result<()> {
        let Some(Trigger::Watch {
            path,
            events,
            debounce_seconds,
            recursive,
        }) = lp.trigger.as_ref()
        else {
            return Err(anyhow::anyhow!("Loop '{}' has no Watch trigger", lp.id));
        };

        let target = resolve_watch_target(path);
        let mode = watch_mode(*recursive, target.file_filter.is_some());

        let mut watcher = build_notify_watcher(
            FireTarget::Loop {
                loop_id: lp.id.clone(),
                db: Arc::clone(&self.db),
                loop_engine: Arc::clone(&self.loop_engine),
            },
            events.clone(),
            *debounce_seconds,
            target.file_filter.clone(),
        )?;

        log_watcher_start(&lp.id, path, &target, events, *recursive);

        if !target.path.exists() {
            log_missing_path(&lp.id, path, target.file_filter.is_some());
        }

        watcher.watch(&target.path, mode)?;

        self.active_loops
            .lock()
            .await
            .insert(lp.id.clone(), watcher);
        Ok(())
    }

    /// Stop a specific watcher by ID.
    pub async fn stop_watcher(&self, id: &str) -> Result<()> {
        if self.active.lock().await.remove(id).is_some() {
            tracing::info!("Stopped watcher '{}'", id);
        }
        Ok(())
    }

    /// Stop a loop watcher by loop ID.
    pub async fn stop_loop_watcher(&self, id: &str) -> Result<()> {
        if self.active_loops.lock().await.remove(id).is_some() {
            tracing::info!("Stopped loop watcher '{}'", id);
        }
        Ok(())
    }

    /// Stop all active watchers (agents and loops).
    pub async fn stop_all(&self) {
        let mut active = self.active.lock().await;
        let count = active.len();
        active.clear();
        let mut active_loops = self.active_loops.lock().await;
        let loop_count = active_loops.len();
        active_loops.clear();
        tracing::info!(
            "Stopped {} agent watchers and {} loop watchers",
            count,
            loop_count
        );
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
    target: FireTarget,
    events: Vec<WatchEvent>,
    debounce_secs: u64,
    file_filter: Option<String>,
) -> Result<RecommendedWatcher> {
    let last_trigger: Arc<Mutex<Option<Instant>>> = Arc::new(Mutex::new(None));
    let rt = tokio::runtime::Handle::current();

    let watcher = RecommendedWatcher::new(
        move |res: Result<Event, notify::Error>| {
            handle_notify_event(
                res,
                &events,
                file_filter.as_deref(),
                debounce_secs,
                &last_trigger,
                &target,
                &rt,
            );
        },
        Config::default(),
    )?;
    Ok(watcher)
}

/// Synchronous event handler called by the notify thread.
fn handle_notify_event(
    res: Result<Event, notify::Error>,
    events: &[WatchEvent],
    file_filter: Option<&str>,
    debounce_secs: u64,
    last_trigger: &Arc<Mutex<Option<Instant>>>,
    target: &FireTarget,
    rt: &tokio::runtime::Handle,
) {
    let id = target.id();
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
    let target = target.clone();
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
        target.fire(file_path, evt_str).await;
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::models::WatchEvent;

    #[test]
    fn resolve_watch_target_directory() {
        let target = resolve_watch_target("/tmp");
        assert_eq!(target.path, PathBuf::from("/tmp"));
        assert!(target.file_filter.is_none());
    }

    #[test]
    fn resolve_watch_target_nonexistent_file_with_parent() {
        let target = resolve_watch_target("/tmp/nonexistent_file.txt");
        assert_eq!(target.path, PathBuf::from("/tmp"));
        assert_eq!(target.file_filter.as_deref(), Some("nonexistent_file.txt"));
    }

    #[test]
    fn resolve_watch_target_nonexistent_path_no_extension() {
        let target = resolve_watch_target("/tmp/some_dir");
        assert_eq!(target.path, PathBuf::from("/tmp/some_dir"));
        assert!(target.file_filter.is_none());
    }

    #[test]
    fn resolve_watch_target_root_path() {
        let target = resolve_watch_target("/");
        assert_eq!(target.path, PathBuf::from("/"));
        assert!(target.file_filter.is_none());
    }

    #[test]
    fn watch_mode_recursive_no_filter() {
        assert_eq!(watch_mode(true, false), RecursiveMode::Recursive);
    }

    #[test]
    fn watch_mode_non_recursive_no_filter() {
        assert_eq!(watch_mode(false, false), RecursiveMode::NonRecursive);
    }

    #[test]
    fn watch_mode_recursive_with_filter_forces_non_recursive() {
        assert_eq!(watch_mode(true, true), RecursiveMode::NonRecursive);
    }

    #[test]
    fn watch_mode_non_recursive_with_filter() {
        assert_eq!(watch_mode(false, true), RecursiveMode::NonRecursive);
    }

    #[test]
    fn event_matches_exact_match() {
        assert!(event_matches(WatchEvent::Create, &[WatchEvent::Create]));
    }

    #[test]
    fn event_matches_no_match() {
        assert!(!event_matches(WatchEvent::Create, &[WatchEvent::Delete]));
    }

    #[test]
    fn event_matches_modify_matches_create_config() {
        // When "create" is configured, "modify" events also match (notify
        // reports modifies as creates on some platforms).
        assert!(event_matches(WatchEvent::Modify, &[WatchEvent::Create]));
    }

    #[test]
    fn event_matches_modify_does_not_match_only_delete() {
        assert!(!event_matches(WatchEvent::Modify, &[WatchEvent::Delete]));
    }

    #[test]
    fn event_matches_in_list() {
        let configured = vec![WatchEvent::Create, WatchEvent::Delete];
        assert!(event_matches(WatchEvent::Create, &configured));
        assert!(event_matches(WatchEvent::Delete, &configured));
        // Modify matches because Create is in the list (line 337)
        assert!(event_matches(WatchEvent::Modify, &configured));
    }

    #[test]
    fn event_matches_move_exact() {
        assert!(event_matches(WatchEvent::Move, &[WatchEvent::Move]));
    }

    #[test]
    fn event_matches_move_not_in_create_list() {
        assert!(!event_matches(WatchEvent::Move, &[WatchEvent::Create]));
    }

    #[test]
    fn map_event_kind_returns_create_for_create() {
        let kind = notify::EventKind::Create(notify::event::CreateKind::File);
        assert_eq!(map_event_kind(&kind, "test"), Some(WatchEvent::Create));
    }

    #[test]
    fn map_event_kind_returns_modify_for_data_change() {
        let kind = notify::EventKind::Modify(notify::event::ModifyKind::Data(
            notify::event::DataChange::Any,
        ));
        assert_eq!(map_event_kind(&kind, "test"), Some(WatchEvent::Modify));
    }

    #[test]
    fn map_event_kind_returns_move_for_rename() {
        let kind = notify::EventKind::Modify(notify::event::ModifyKind::Name(
            notify::event::RenameMode::Any,
        ));
        assert_eq!(map_event_kind(&kind, "test"), Some(WatchEvent::Move));
    }

    #[test]
    fn map_event_kind_returns_delete_for_remove() {
        let kind = notify::EventKind::Remove(notify::event::RemoveKind::File);
        assert_eq!(map_event_kind(&kind, "test"), Some(WatchEvent::Delete));
    }

    #[test]
    fn map_event_kind_returns_none_for_access() {
        let kind = notify::EventKind::Access(notify::event::AccessKind::Read);
        assert_eq!(map_event_kind(&kind, "test"), None);
    }

    #[test]
    fn map_event_kind_returns_none_for_other() {
        let kind = notify::EventKind::Any;
        assert_eq!(map_event_kind(&kind, "test"), None);
    }
}
