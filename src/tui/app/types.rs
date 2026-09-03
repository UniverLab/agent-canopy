use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use chrono::{DateTime, Utc};

use super::loop_live_state::LoopLiveState;
use crate::application::notification_service::NotificationService;
use crate::db::project::{RagInfoSummary, RagQueueItem};
use crate::db::Database;
use crate::domain::loops::{Loop, LoopDetails, LoopNodeRun, LoopSpec};
use crate::domain::models::{Agent, CorruptAgent, RunLog};
use crate::domain::project::Project;
use crate::domain::sync::{ActiveIntent, SyncMessage, WorkspaceStatus};
use crate::rag::vector_store::SearchResult;
use crate::tui::agent::InteractiveAgent;
use crate::tui::app::dialog::{
    LaunchpadDialog, LoopFormDialog, NewAgentDialog, SimplePromptDialog,
};
use crate::tui::app::terminal_search::TerminalSearch;
/// Unified entry in the sidebar.
#[allow(clippy::large_enum_variant)]
pub enum AgentEntry {
    Agent(Agent),
    /// An agent row that failed to decode (e.g. malformed `trigger_config`
    /// written directly to SQLite by an external tool). Rendered as a
    /// degraded card instead of crashing the whole sidebar.
    Corrupt(CorruptAgent),
    Interactive(usize), // index into App::interactive_agents
    Terminal(usize),    // index into App::terminal_agents
    Orphaned(usize),    // index into App::orphaned_sessions
    Group(usize),       // index into App::split_groups
}

impl AgentEntry {
    pub fn id<'a>(&'a self, app: &'a App) -> &'a str {
        match self {
            Self::Agent(a) => &a.id,
            Self::Corrupt(c) => &c.id,
            Self::Interactive(idx) => app
                .interactive_agents
                .get(*idx)
                .map_or("?", |a| a.seed_name.as_deref().unwrap_or(&a.name)),
            Self::Terminal(idx) => app.terminal_agents.get(*idx).map_or("?", |a| &a.name),
            Self::Orphaned(idx) => app.orphaned_sessions.get(*idx).map_or("?", |s| &s.name),
            Self::Group(idx) => app.split_groups.get(*idx).map_or("?", |g| &g.id),
        }
    }
}

/// Which panel has focus.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Focus {
    Home,
    Preview,
    NewAgentDialog,
    LaunchpadDialog,
    KnowledgeDialog,
    Agent,
    ContextTransfer,
    RagTransfer,
    PromptTemplateDialog,
    LoopEditorDialog,
    LoopFormDialog,
    ProjectRelationDialog,
}

/// Mouse text selection over the focused agent's PTY pane. Coordinates are
/// pane-relative `(row, col)` cells matching the rendered `ScreenSnapshot`.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) struct TerminalSelection {
    /// Selected agent this selection belongs to: (is_terminal, index).
    pub agent: (bool, usize),
    pub start: (u16, u16),
    pub end: (u16, u16),
    pub dragging: bool,
}

impl TerminalSelection {
    /// Selection endpoints in linear (reading) order: start ≤ end.
    pub fn normalized(&self) -> ((u16, u16), (u16, u16)) {
        if self.end < self.start {
            (self.end, self.start)
        } else {
            (self.start, self.end)
        }
    }
}

/// The sidebar's three thematic tabs, shown one at a time below the pinned
/// RAG summary (top) and above the sysinfo dashboard (bottom).
/// `App::sidebar_layer` tracks which one is currently active.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SidebarLayer {
    /// Interactive agents + terminals — the things with a PTY right now.
    Live,
    /// Background agents + loops — live/recent runs, global across projects.
    Automation,
    /// The projects list.
    Knowledge,
}

/// Which of Automation's two sub-lists (background agents, loops) arrow-key
/// navigation is currently cycling through.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum AutomationKind {
    Agent,
    Loop,
}

/// The right panel's faces (CT1): one panel, three views. Activity is the
/// resting face when nothing else applies; Knowledge shows activity moved
/// under it plus the project-relations graph; Loop shows a read-only view
/// of the running loop.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PanelFace {
    #[default]
    Activity,
    Knowledge,
    Loop,
}

impl PanelFace {
    pub const ALL: [PanelFace; 3] = [PanelFace::Activity, PanelFace::Knowledge, PanelFace::Loop];

    pub fn label(self) -> &'static str {
        match self {
            PanelFace::Activity => "activity",
            PanelFace::Knowledge => "knowledge",
            PanelFace::Loop => "loop",
        }
    }

    /// Parse the persisted config string. Unknown values map to `None` so a
    /// config written by a newer binary never breaks this one — the panel
    /// just stays in automatic mode.
    pub fn from_str(value: &str) -> Option<PanelFace> {
        match value {
            "activity" => Some(PanelFace::Activity),
            "knowledge" => Some(PanelFace::Knowledge),
            "loop" => Some(PanelFace::Loop),
            _ => None,
        }
    }
}

/// Tabs shown inside a project once it's entered (`Focus::Agent` while
/// `SidebarLayer::Knowledge` is active) — everything project-scoped lives
/// here instead of as top-level sidebar siblings.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ProjectTab {
    Overview,
    Backlog,
    Knowledge,
    History,
}

/// Per-layer selection remembered across a keyboard tab *step*
/// (`Shift+←/→`), so stepping away and back lands you where you left off —
/// unlike a direct jump (mouse click, F2), which deliberately always lands
/// on the tab's edge item. `Live` and `Automation`'s agent sub-list share
/// `App::selected` as their index space, so a step away from one clobbers
/// the other's value; this cache is what lets a step back restore it.
#[derive(Clone, Default)]
pub(crate) struct SidebarStepMemory {
    pub(crate) live_selected: Option<usize>,
    pub(crate) automation_kind: Option<AutomationKind>,
    pub(crate) automation_selected: Option<usize>,
    pub(crate) automation_loop_id: Option<String>,
    pub(crate) knowledge_selected: Option<usize>,
}

impl ProjectTab {
    pub const ALL: [ProjectTab; 4] = [
        ProjectTab::Overview,
        ProjectTab::Backlog,
        ProjectTab::Knowledge,
        ProjectTab::History,
    ];

    pub fn label(self) -> &'static str {
        match self {
            ProjectTab::Overview => "Overview",
            ProjectTab::Backlog => "Backlog",
            ProjectTab::Knowledge => "Knowledge",
            ProjectTab::History => "History",
        }
    }

    pub fn hotkey(self) -> char {
        match self {
            ProjectTab::Overview => 'o',
            ProjectTab::Backlog => 'b',
            ProjectTab::Knowledge => 'k',
            ProjectTab::History => 'h',
        }
    }
}

/// Cheap, cached summary shown on a project's Preview card (highlighted, not
/// entered) — recomputed on the normal `App::refresh` cadence, never on a
/// per-keystroke highlight move (functional requirement 3).
#[derive(Clone, Default)]
pub(crate) struct ProjectPreviewSummary {
    pub pending_backlog: usize,
    pub knowledge_entries: usize,
    pub last_activity: Option<i64>,
    pub loop_running: bool,
}

/// Per-loop rendering data for the sidebar's `Loops` section: when the loop
/// last did something, and whether it is stuck on a reported blocker (a
/// `Paused` loop whose latest run recorded a `blocker`, see
/// `loop_report_blocker`). Computed once per refresh cycle
/// (`App::refresh_loops`) rather than queried or formatted per frame — the
/// sidebar just prints `last_run_label` as-is.
///
/// Deliberately not a per-loop spec count: that reads `0/0` for a
/// queue-driven loop, whose specs live on the queue rather than on the loop
/// itself (the loop focus view's `state.done_count`/`total_count` is the
/// correct place for that number, and is unaffected by this struct).
#[derive(Clone)]
pub(crate) struct LoopSidebarMeta {
    /// Last recorded activity for this loop: the latest `loop_runs.started_at`
    /// across every node run belonging to it, or — if it has never run — the
    /// loop's own `created_at`. A single monotonic "last activity" key that
    /// is defined for every loop regardless of status — the sort key behind
    /// `App::sidebar_loops`' most-recent-first ordering.
    pub last_activity: DateTime<Utc>,
    /// Precomputed display text for `last_activity`: a compact relative
    /// time (`2m`, `1h`, `3d`), `"running"` while the loop is actively
    /// executing, or `"never"` if it has never run.
    pub last_run_label: String,
    pub blocked: bool,
    /// `"resumes 5m"`-style label when the loop has a pending
    /// `loop_schedule_autorun`, `None` otherwise.
    pub autorun_label: Option<String>,
}

impl Default for LoopSidebarMeta {
    /// Only used as a placeholder before the first `refresh_loops` populates
    /// the real map — every loop gets a real entry on every refresh, so this
    /// value is never actually shown.
    fn default() -> Self {
        Self {
            last_activity: DateTime::<Utc>::from_timestamp(0, 0).expect("epoch is representable"),
            last_run_label: String::new(),
            blocked: false,
            autorun_label: None,
        }
    }
}

/// Border-focus sub-section within the `Live` layer (interactive/terminal
/// agents render as three stacked sub-panels sharing one collapsible layer).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[expect(dead_code)]
pub enum AgentSectionFocus {
    Interactive,
    Terminal,
    Groups,
    Brain,
}

/// Which sub-region of the live loop view owns plain arrow-key navigation:
/// the node graph (`loop_graph_move_highlight`, the long-standing default)
/// or the spec marker strip at the top (`loop_spec_strip_move_selection`).
/// Toggled with Tab/BackTab while a loop is the active sidebar selection —
/// see `on_loop` in `crate::tui::event::home_preview`.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub(crate) enum LoopLiveFocus {
    #[default]
    Graph,
    SpecStrip,
}

#[derive(Clone)]
pub(crate) enum LoopEditorMode {
    AgentPrompt,
    NodeConfig,
    /// A [`crate::domain::loops::LoopNodeKind::Router`] node's structured
    /// routes/fallback/wiring editor — replaces the raw JSON buffer used by
    /// [`LoopEditorMode::NodeConfig`] with the `router_*` fields below, since
    /// wiring an edge per route isn't expressible as node config alone.
    RouterRoutes,
    /// A node's outgoing `pass`/`fail`/`always` edges — lets an ordinary
    /// edge be retargeted or deleted, through the same validated path
    /// (`daemon::handler::retarget_loop_edge`/`delete_loop_edge_checked`)
    /// the `loop_update_edge`/`loop_delete_edge` MCP tools use, rather than
    /// only a router's route edges (see [`LoopEditorMode::RouterRoutes`]).
    Edges,
}

/// Which sub-field of the currently-focused route row
/// [`LoopEditorDialog::router_field`] points at.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum RouterField {
    Label,
    Description,
    Target,
}

/// One route being edited in the router routes dialog: the declared
/// label/description (persisted into the node's `config`) plus the target
/// node its `route` edge should point at (persisted as a separate
/// [`crate::domain::loops::LoopEdge`] on save — `None` means not wired yet).
#[derive(Clone, Default)]
pub(crate) struct RouterRouteDraft {
    pub label: String,
    pub description: String,
    pub target_node_id: Option<String>,
}

#[derive(Clone)]
pub(crate) struct LoopEditorDialog {
    pub node_id: String,
    pub node_name: String,
    pub title: String,
    pub help: String,
    pub buffer: String,
    pub cursor: usize,
    pub mode: LoopEditorMode,
    pub parse_error: Option<String>,
    /// `RouterRoutes` mode only, below — unused (empty) otherwise.
    pub router_routes: Vec<RouterRouteDraft>,
    pub router_fallback: String,
    pub router_route_index: usize,
    pub router_field: RouterField,
    /// Candidate `(node_id, node_name)` targets a route can wire to: every
    /// other node in the router's graph.
    pub router_targets: Vec<(String, String)>,
    /// `Edges` mode only, below — unused (empty) otherwise. This node's
    /// outgoing `pass`/`fail`/`always` edges (route edges are managed via
    /// `RouterRoutes` instead).
    pub edge_rows: Vec<crate::domain::loops::LoopEdge>,
    pub edge_row_index: usize,
    /// Candidate `(node_id, node_name)` retarget destinations: every other
    /// node in the edge's graph — same set as `router_targets`.
    pub edge_targets: Vec<(String, String)>,
}

impl LoopEditorDialog {
    pub fn new(
        node_id: String,
        node_name: String,
        title: String,
        help: String,
        buffer: String,
        mode: LoopEditorMode,
    ) -> Self {
        let cursor = buffer.chars().count();
        Self {
            node_id,
            node_name,
            title,
            help,
            buffer,
            cursor,
            mode,
            parse_error: None,
            router_routes: Vec::new(),
            router_fallback: String::new(),
            router_route_index: 0,
            router_field: RouterField::Label,
            router_targets: Vec::new(),
            edge_rows: Vec::new(),
            edge_row_index: 0,
            edge_targets: Vec::new(),
        }
    }

    pub fn new_edges(
        node_id: String,
        node_name: String,
        title: String,
        help: String,
        edges: Vec<crate::domain::loops::LoopEdge>,
        targets: Vec<(String, String)>,
    ) -> Self {
        Self {
            node_id,
            node_name,
            title,
            help,
            buffer: String::new(),
            cursor: 0,
            mode: LoopEditorMode::Edges,
            parse_error: None,
            router_routes: Vec::new(),
            router_fallback: String::new(),
            router_route_index: 0,
            router_field: RouterField::Label,
            router_targets: Vec::new(),
            edge_rows: edges,
            edge_row_index: 0,
            edge_targets: targets,
        }
    }

    /// Move the `Edges` mode row focus among this node's outgoing edges,
    /// wrapping. A no-op with zero or one row.
    pub fn edge_move_row(&mut self, forward: bool) {
        if self.edge_rows.is_empty() {
            return;
        }
        self.edge_row_index =
            crate::tui::selection::move_index(self.edge_row_index, self.edge_rows.len(), forward);
    }

    /// The edge currently focused in `Edges` mode's row list.
    pub fn focused_edge(&self) -> Option<&crate::domain::loops::LoopEdge> {
        self.edge_rows.get(self.edge_row_index)
    }

    /// The next/previous candidate destination for the focused edge,
    /// cycling through `edge_targets` (every other node in the graph).
    /// Unlike [`Self::cycle_router_target`], an ordinary edge always names a
    /// concrete `to_node`, so there is no `(none)` state to cycle through.
    pub fn next_edge_target_candidate(&self, forward: bool) -> Option<String> {
        let edge = self.focused_edge()?;
        if self.edge_targets.is_empty() {
            return None;
        }
        let current = self
            .edge_targets
            .iter()
            .position(|(id, _)| id == &edge.to_node);
        let len = self.edge_targets.len();
        let next_index = match current {
            Some(index) => crate::tui::selection::move_index(index, len, forward),
            None => 0,
        };
        Some(self.edge_targets[next_index].0.clone())
    }

    #[allow(clippy::too_many_arguments)]
    pub fn new_router_routes(
        node_id: String,
        node_name: String,
        title: String,
        help: String,
        routes: Vec<RouterRouteDraft>,
        fallback: String,
        targets: Vec<(String, String)>,
    ) -> Self {
        Self {
            node_id,
            node_name,
            title,
            help,
            buffer: String::new(),
            cursor: 0,
            mode: LoopEditorMode::RouterRoutes,
            parse_error: None,
            router_routes: routes,
            router_fallback: fallback,
            router_route_index: 0,
            router_field: RouterField::Label,
            router_targets: targets,
            edge_rows: Vec::new(),
            edge_row_index: 0,
            edge_targets: Vec::new(),
        }
    }

    /// The label/description text behind the focused route's focused field,
    /// if the focus is on a text field (not the target picker).
    pub fn router_focused_text_mut(&mut self) -> Option<&mut String> {
        let field = self.router_field;
        let route = self.router_routes.get_mut(self.router_route_index)?;
        match field {
            RouterField::Label => Some(&mut route.label),
            RouterField::Description => Some(&mut route.description),
            RouterField::Target => None,
        }
    }

    /// Append to the focused route's focused text field. No-op on the
    /// target field (see [`Self::cycle_router_target`] instead) — fields are
    /// short single-line labels, so editing is append/pop-at-end only,
    /// unlike the free-cursor `buffer` used by the other editor modes.
    pub fn router_push_char(&mut self, value: char) {
        if let Some(text) = self.router_focused_text_mut() {
            text.push(value);
        }
    }

    /// Drop the last character of the focused route's focused text field.
    pub fn router_pop_char(&mut self) {
        if let Some(text) = self.router_focused_text_mut() {
            text.pop();
        }
    }

    /// Unwire the focused route's target (Backspace while it's focused).
    pub fn router_clear_target(&mut self) {
        if let Some(route) = self.router_routes.get_mut(self.router_route_index) {
            route.target_node_id = None;
        }
    }

    /// Cycle the focused route's target through `(none) -> targets... ->
    /// (none)`, in `router_targets` order.
    pub fn cycle_router_target(&mut self, forward: bool) {
        let Some(route) = self.router_routes.get_mut(self.router_route_index) else {
            return;
        };
        if self.router_targets.is_empty() {
            route.target_node_id = None;
            return;
        }
        let current = route
            .target_node_id
            .as_deref()
            .and_then(|id| self.router_targets.iter().position(|(tid, _)| tid == id));
        // Index space is `[None, targets[0], targets[1], ...]`.
        let len = self.router_targets.len() + 1;
        let current_index = current.map(|i| i + 1).unwrap_or(0);
        let next_index = crate::tui::selection::move_index(current_index, len, forward);
        route.target_node_id = if next_index == 0 {
            None
        } else {
            Some(self.router_targets[next_index - 1].0.clone())
        };
    }

    /// Move route/field focus to the next sub-field, wrapping to the next
    /// route's first field at the end.
    pub fn router_next_field(&mut self) {
        self.router_field = match self.router_field {
            RouterField::Label => RouterField::Description,
            RouterField::Description => RouterField::Target,
            RouterField::Target => {
                if !self.router_routes.is_empty() {
                    self.router_route_index = crate::tui::selection::move_index(
                        self.router_route_index,
                        self.router_routes.len(),
                        true,
                    );
                }
                RouterField::Label
            }
        };
    }

    pub fn router_prev_field(&mut self) {
        self.router_field = match self.router_field {
            RouterField::Target => RouterField::Description,
            RouterField::Description => RouterField::Label,
            RouterField::Label => {
                if !self.router_routes.is_empty() {
                    self.router_route_index = crate::tui::selection::move_index(
                        self.router_route_index,
                        self.router_routes.len(),
                        false,
                    );
                }
                RouterField::Target
            }
        };
    }

    /// Move the route selection itself (Up/Down), keeping the focused
    /// sub-field.
    pub fn router_move_route(&mut self, forward: bool) {
        if self.router_routes.is_empty() {
            return;
        }
        self.router_route_index = crate::tui::selection::move_index(
            self.router_route_index,
            self.router_routes.len(),
            forward,
        );
    }

    /// Append a fresh, unwired route and focus it (Ctrl+N).
    pub fn router_add_route(&mut self) {
        self.router_routes.push(RouterRouteDraft::default());
        self.router_route_index = self.router_routes.len() - 1;
        self.router_field = RouterField::Label;
    }

    /// Drop the focused route (Ctrl+D). If it was the fallback, the fallback
    /// is cleared — [`crate::domain::loops::validate_router_routes`] will
    /// catch an empty/dangling fallback on save.
    pub fn router_remove_route(&mut self) {
        if self.router_routes.is_empty() {
            return;
        }
        let removed = self.router_routes.remove(self.router_route_index);
        if self.router_fallback == removed.label {
            self.router_fallback.clear();
        }
        if self.router_route_index >= self.router_routes.len() {
            self.router_route_index = self.router_routes.len().saturating_sub(1);
        }
    }

    /// Set the focused route as the fallback (Ctrl+F).
    pub fn router_set_fallback(&mut self) {
        if let Some(route) = self.router_routes.get(self.router_route_index) {
            self.router_fallback = route.label.clone();
        }
    }

    pub fn char_len(&self) -> usize {
        self.buffer.chars().count()
    }

    pub fn insert_char(&mut self, value: char) {
        self.insert_str(&value.to_string());
    }

    pub fn insert_str(&mut self, value: &str) {
        let byte_index = char_to_byte_index(&self.buffer, self.cursor);
        self.buffer.insert_str(byte_index, value);
        self.cursor += value.chars().count();
    }

    pub fn backspace(&mut self) {
        if self.cursor == 0 {
            return;
        }
        let start = char_to_byte_index(&self.buffer, self.cursor - 1);
        let end = char_to_byte_index(&self.buffer, self.cursor);
        self.buffer.replace_range(start..end, "");
        self.cursor -= 1;
    }

    pub fn move_left(&mut self) {
        self.cursor = self.cursor.saturating_sub(1);
    }

    pub fn move_right(&mut self) {
        self.cursor = (self.cursor + 1).min(self.char_len());
    }

    pub fn move_home(&mut self) {
        self.cursor = 0;
    }

    pub fn move_end(&mut self) {
        self.cursor = self.char_len();
    }
}

fn char_to_byte_index(text: &str, char_index: usize) -> usize {
    text.char_indices()
        .map(|(index, _)| index)
        .nth(char_index)
        .unwrap_or(text.len())
}

#[derive(Clone, Copy)]
pub(crate) enum ContextTransferSource {
    Interactive(usize),
    Terminal(usize),
}

#[derive(Clone)]
pub(crate) struct SyncPanelState {
    pub workdir: String,
    pub participant_count: usize,
    pub vibe: WorkspaceStatus,
    pub active_intents: Vec<ActiveIntent>,
    pub recent_messages: Vec<SyncMessage>,
}

#[derive(Clone)]
pub(crate) struct RagTransferModal {
    pub picker_selected: usize,
    pub query: String,
    pub context_payload: String,
}

// ── App struct ──────────────────────────────────────────────────

/// Main application state.
pub struct App {
    pub(crate) db: Arc<Database>,
    pub(crate) data_dir: PathBuf,

    // Data cache (refreshed every tick)
    pub(crate) agents: Vec<AgentEntry>,
    pub(crate) active_runs: HashMap<String, RunLog>,
    pub(crate) recent_runs: Vec<RunLog>,
    pub(crate) interactive_agents: Vec<InteractiveAgent>,
    /// Raw terminal sessions (no AI CLI).
    pub(crate) terminal_agents: Vec<InteractiveAgent>,
    /// Sessions orphaned during auto-resume (can be revived or dismissed).
    pub(crate) orphaned_sessions: Vec<crate::db::session::InteractiveSession>,
    /// Gate: pending scheduled sends are held (not delivered) until the
    /// startup restore runs — auto-resume reassigns each schedule onto its
    /// resumed session id and drops schedules whose session is gone. Without
    /// this, the first refresh (before sessions resume) would see zero live
    /// sessions and prematurely treat every due schedule as dead.
    pub(crate) scheduled_sends_restored: bool,

    // Split group state
    pub(crate) split_groups: Vec<crate::domain::models::SplitGroup>,
    /// ID of the split group currently being viewed (if any).
    pub(crate) active_split_id: Option<String>,
    /// True = right/bottom panel is focused in split view.
    pub(crate) split_right_focused: bool,
    /// Whether the split picker overlay is open.
    pub(crate) split_picker_open: bool,
    pub(crate) split_picker_idx: usize,
    pub(crate) split_picker_orientation: crate::domain::models::SplitOrientation,
    /// (name, type_label) for each available session in the picker.
    pub(crate) split_picker_sessions: Vec<(String, String)>,

    // Daemon info
    pub(crate) daemon_running: bool,
    pub(crate) daemon_pid: Option<u32>,
    pub(crate) daemon_version: String,

    // UI state
    pub(crate) selected: usize,
    pub(crate) focus: Focus,
    /// Which sidebar tab (Live / Automation / Knowledge) is active.
    pub(crate) sidebar_layer: SidebarLayer,
    /// Which of Automation's two sub-lists is active for navigation.
    pub(crate) automation_kind: AutomationKind,
    /// Remembered per-layer selection for `App::step_sidebar_tab`, so a step
    /// away and back doesn't reset the tab you return to.
    pub(crate) sidebar_step_memory: SidebarStepMemory,
    /// `Some(tab)` while a project is entered (Focus tab bar showing);
    /// `None` while only highlighted (Preview summary card showing).
    pub(crate) project_focus: Option<ProjectTab>,
    pub(crate) selected_project_history: usize,
    /// Persisted per-project History tab data, keyed by project hash and
    /// refreshed lazily on first show of the tab (functional requirement 4).
    pub(crate) project_history_cache: HashMap<String, Vec<crate::db::project::ProjectHistoryEntry>>,
    /// Cheap per-project Preview summary, keyed by project hash and
    /// recomputed on the normal refresh cadence — never per keystroke.
    pub(crate) project_preview_cache: HashMap<String, ProjectPreviewSummary>,
    pub(crate) log_content: String,
    pub(crate) log_scroll: u16,
    pub(crate) running: bool,
    pub(crate) new_agent_dialog: Option<NewAgentDialog>,
    pub(crate) launchpad_dialog: Option<LaunchpadDialog>,
    pub(crate) knowledge_dialog: Option<crate::tui::app::dialog::KnowledgeDialog>,
    pub(crate) pending_launch_dialog: Option<NewAgentDialog>,
    pub(crate) quit_confirm: bool,
    pub(crate) delete_project_confirm: bool,
    pub(crate) archive_loop_confirm: bool,
    /// Permanent-delete confirmation, reachable only from the archived
    /// view on an already-archived loop — see [`App::permanent_delete_selected_archived_loop`].
    pub(crate) permanent_delete_loop_confirm: bool,
    /// Confirmation gate for `loop_reset` (mirrors `archive_loop_confirm`) —
    /// reset clears progress on every non-completed spec, so it asks first,
    /// with the same wording the CLI's own prompt uses (see
    /// `daemon::loop_cli::confirm_reset`) so the two surfaces never teach
    /// different levels of caution.
    pub(crate) loop_reset_confirm: bool,

    // Brian's Brain automaton (sidebar decoration)
    pub(crate) sidebar_brain: Option<crate::tui::brians_brain::BriansBrain>,
    // Brian's Brain for home banner background
    pub(crate) home_brain: Option<crate::tui::brians_brain::BriansBrain>,

    // System monitoring (updated asynchronously to avoid UI freezes)
    pub(crate) system_info: crate::system::SystemInfo,
    pub(crate) system_info_target: crate::system::SystemInfo,
    pub(crate) system_info_rx: std::sync::mpsc::Receiver<crate::system::SystemInfo>,
    /// Controls system monitor activity: true = poll, false = pause polling.
    pub(crate) system_monitor_active: Arc<AtomicBool>,
    pub(crate) last_system_update: std::time::Instant,
    pub(crate) last_system_frame_at: std::time::Instant,
    pub(crate) process_start_time: std::time::Instant,

    // Layout state
    pub(crate) sidebar_click_map: Vec<(usize, u16, u16)>,
    /// Agent index under the mouse cursor in the sidebar (hover highlight).
    pub(crate) hovered_row: Option<usize>,
    /// Manual mouse-wheel scroll adjustment applied on top of the
    /// selection-follow scroll in the agent sidebar sections.
    pub(crate) sidebar_scroll_offset: usize,
    /// Total visible agent rows across the rendered sidebar sections on the
    /// last frame; used to clamp mouse-wheel scrolling.
    pub(crate) sidebar_visible_capacity: usize,
    pub(crate) projects: Vec<Project>,
    pub(crate) selected_project: usize,
    pub(crate) agent_section_focus: AgentSectionFocus,
    /// Mouse hit-test rows for the Automation layer's loop cards, populated
    /// during draw: `(loop id, row_start, row_end)`.
    pub(crate) automation_loop_click_map: Vec<(String, u16, u16)>,
    /// Mouse hit-test rows for the Knowledge layer's project list,
    /// populated during draw: `(project index, row_start, row_end)`.
    pub(crate) project_click_map: Vec<(usize, u16, u16)>,
    /// Mouse hit-test columns for a Focus tab bar, populated during draw:
    /// `(tab, col_start, col_end)`.
    pub(crate) project_tab_click_map: Vec<(ProjectTab, u16, u16)>,
    /// Mouse hit-test rows for the active tab's list, populated during draw.
    pub(crate) project_tab_row_click_map: Vec<(usize, u16, u16)>,
    /// Mouse hit-test cells for the sidebar's tab strip, populated during
    /// draw: `(tab, row, col_start, col_end)` — clicking switches the active
    /// tab.
    pub(crate) sidebar_tab_click_map: Vec<(SidebarLayer, u16, u16, u16)>,
    pub(crate) loops: Vec<Loop>,
    /// Archived loops (excluded from `loops`), populated only while
    /// `loop_view_archived` is true — see [`App::refresh_loops`].
    pub(crate) archived_loops: Vec<Loop>,
    /// Count of archived loops, kept up to date on every refresh so it's
    /// visible from the main view at all times regardless of
    /// `loop_view_archived`.
    pub(crate) archived_loop_count: usize,
    /// Whether the Loops sidebar section is currently showing the archive
    /// (`true`) instead of the main list (`false`) — a toggle on the
    /// existing Loops section rather than a separate sidebar layer, so
    /// archived loops stay in the same mental place as active ones.
    pub(crate) loop_view_archived: bool,
    pub(crate) selected_loop_id: Option<String>,
    pub(crate) loop_details: Option<LoopDetails>,
    pub(crate) loop_runs: Vec<LoopNodeRun>,
    pub(crate) loop_selected_spec: usize,
    pub(crate) loop_selected_node: usize,
    pub(crate) loop_editor_dialog: Option<LoopEditorDialog>,
    pub(crate) loop_form_dialog: Option<LoopFormDialog>,
    /// Per-loop spec progress ("done/total") and blocked status for the
    /// sidebar's `Loops` section, keyed by loop id. Refreshed alongside
    /// `loops` in `App::refresh_loops`.
    pub(crate) loop_sidebar_meta: HashMap<String, LoopSidebarMeta>,
    /// Live snapshot of the currently-selected loop's runtime state,
    /// refreshed every tick. `None` when no loop is selected.
    pub(crate) loop_live_state: Option<LoopLiveState>,
    /// Whether the live loop view's graph highlight auto-follows the
    /// engine's current node (`true`, the default) or sits on a node the
    /// user manually navigated to (`false`, see `loop_graph_selected_node`).
    /// Reset to `true` whenever the selected loop changes.
    pub(crate) loop_graph_follow: bool,
    /// The node id manually highlighted in the live loop view's graph.
    /// Only meaningful while `loop_graph_follow` is `false`.
    pub(crate) loop_graph_selected_node: Option<String>,
    /// Which sub-region of the live loop view plain arrow keys drive.
    /// Reset to `Graph` whenever the selected loop changes.
    pub(crate) loop_live_focus: LoopLiveFocus,
    /// The spec id manually selected in the live loop view's marker strip
    /// (independent of `current_spec_id` / the graph's own follow state —
    /// selecting a spec here never touches `loop_graph_follow`). `None`
    /// means the strip shows the running/next-pending spec by default.
    pub(crate) loop_spec_strip_selected: Option<String>,
    /// First visible index into `LoopLiveState::spec_queue` for the marker
    /// strip, when there are more specs than fit in the panel's width.
    pub(crate) loop_spec_strip_scroll: usize,
    /// How many marker chips fit in the panel's width on the last render —
    /// used to keep keyboard navigation's scroll offset in sync with what's
    /// actually drawn. Populated in `draw_loop_live_view`.
    pub(crate) loop_spec_strip_capacity: usize,
    /// Mouse hit-test cells for the marker strip, populated during draw:
    /// `(spec id, row, col_start, col_end)` — mirrors `sidebar_tab_click_map`.
    pub(crate) loop_spec_strip_click_map: Vec<(String, u16, u16, u16)>,
    /// Vertical scroll offset (in text lines) for the live loop view's main
    /// content area. Clamped to `[0, total_lines - panel_height]` on every
    /// render. Reset to 0 whenever the selected loop changes.
    pub(crate) loop_live_view_scroll: u16,
    /// Total number of content lines rendered on the last frame — used to
    /// clamp `loop_live_view_scroll`. Populated by `render_loop_live_view`.
    pub(crate) loop_live_view_total_lines: u16,
    /// Open autorun-scheduling input for the loop currently focused in the
    /// live view — `None` when not open. See
    /// [`crate::tui::app::dialog::LoopAutorunDialog`].
    pub(crate) loop_autorun_dialog: Option<crate::tui::app::dialog::LoopAutorunDialog>,
    /// CT3 live-tail viewer for a running check node. Diagnostic-only: all
    /// reads, so dismissing it can never affect execution. `Some` while
    /// open (including after the node finishes, showing the final banner).
    pub(crate) node_tail_dialog: Option<crate::tui::app::dialog::NodeTailDialog>,
    /// True while a loop-control dispatch (`loop_run`/`loop_pause`/
    /// `loop_continue`/`loop_reset`/`loop_schedule_autorun`) is in flight on
    /// `loop_action_rx` — guards against a second dispatch racing the first.
    pub(crate) loop_action_pending: bool,
    /// Receiver for the background thread running the current loop-control
    /// dispatch (see `App::dispatch_loop_action`), polled non-blockingly by
    /// `App::poll_loop_action` every tick so the UI thread never waits on the
    /// daemon's HTTP round-trip.
    pub(crate) loop_action_rx:
        Option<std::sync::mpsc::Receiver<crate::tui::app::dialog::LoopActionOutcome>>,
    /// The daemon's verbatim response to the last loop-control action, shown
    /// until dismissed or superseded by the next dispatch — success or
    /// error, per the loop controls' "always shown, never swallowed" rule.
    pub(crate) loop_action_message: Option<crate::tui::app::dialog::LoopActionMessage>,
    /// When the current `loop_action_message` was set — drives its
    /// auto-dismiss (mirrors `copied_at`/`dismiss_copied`).
    pub(crate) loop_action_message_at: std::time::Instant,
    /// Standalone/backlog specs (no loop yet), filtered to the selected
    /// project's workdir tag when a project is selected. Refreshed alongside
    /// `projects` in `App::refresh_projects`.
    pub(crate) backlog_specs: Vec<LoopSpec>,
    pub(crate) selected_backlog: usize,
    pub(crate) global_rag_queue: Vec<RagQueueItem>,
    pub(crate) selected_rag_queue: usize,
    pub(crate) rag_info: RagInfoSummary,
    /// Per-file RAG status loaded from `rag_file_events` table.
    pub(crate) rag_file_status: Vec<crate::db::project::RagPerFileStatus>,
    /// Knowledge nodes (facts/patterns) for the selected project.
    pub(crate) project_knowledge: Vec<crate::db::intelligence::IntelligenceNodeRecord>,
    pub(crate) selected_knowledge: usize,
    pub(crate) knowledge_filter: String,
    pub(crate) knowledge_filter_mode: bool,
    pub(crate) sidebar_visible: bool,
    pub(crate) hidden_activity_workdirs: HashSet<String>,
    pub(crate) forced_activity_workdirs: HashSet<String>,
    pub(crate) term_width: u16,
    pub(crate) show_legend: bool,
    pub(crate) legend_selected: usize,
    pub(crate) show_copied: bool,
    pub(crate) copied_at: std::time::Instant,
    pub(crate) last_scroll_at: std::time::Instant,
    pub(crate) last_panel_inner: (u16, u16),
    pub(crate) last_panel_x: u16,
    pub(crate) last_panel_y: u16,
    /// Active mouse text selection over the focused agent's PTY pane.
    pub(crate) terminal_selection: Option<TerminalSelection>,
    pub(crate) whimsg: crate::tui::whimsg::Whimsg,
    /// Hash of the last log chunk scanned for whimsg triggers — avoids re-firing
    /// on the same content every tick.
    pub(crate) whimsg_last_log_hash: u64,
    pub(crate) context_transfer_modal: Option<crate::tui::context_transfer::ContextTransferModal>,
    pub(crate) rag_transfer_modal: Option<RagTransferModal>,
    pub(crate) context_transfer_config: crate::tui::context_transfer::ContextTransferConfig,
    /// Prompt templates loaded from registry
    #[allow(dead_code)]
    pub(crate) prompt_templates: crate::tui::prompt_templates::PromptTemplates,
    /// Current simple prompt dialog state
    pub(crate) simple_prompt_dialog: Option<SimplePromptDialog>,
    /// Persisted prompt-builder sessions per agent/session (cleared on send).
    pub(crate) prompt_builder_sessions:
        HashMap<String, crate::tui::app::dialog::PromptBuilderSession>,
    /// Tab-bar origin `(x, y)` of the prompt builder from the last frame, used
    /// for mouse hit-testing the clickable Normal/Raw tabs.
    pub(crate) prompt_tab_origin: Option<(u16, u16)>,
    /// Raw tab content region `Rect` from the last frame, used for mouse
    /// wheel hit-testing the scrollable content area.
    pub(crate) prompt_raw_content_rect: Option<ratatui::layout::Rect>,
    /// Whether to send OS-level desktop notifications (agent done/failed).
    pub(crate) notifications_enabled: bool,
    /// Notification service for sending cross-platform notifications.
    pub(crate) notification_service: Arc<dyn NotificationService>,
    /// IDs of runs that were active on the previous refresh tick.
    pub(crate) prev_active_run_ids: std::collections::HashSet<String>,
    /// Tick counter for animation (increments every refresh)
    pub(crate) animation_tick: u32,
    /// Preferred unit for sysinfo temperature labels.
    pub(crate) temperature_unit: crate::domain::canopy_config::TemperatureUnit,
    /// Resolved TUI color theme (T6), read from config once at startup.
    /// No live switching yet — changing it requires a restart.
    pub(crate) theme: crate::tui::ui::theme::Theme,
    /// Terminal autocomplete suggestion picker (shown on Tab).
    pub(crate) suggestion_picker: Option<crate::tui::terminal_history::SuggestionPicker>,
    /// Per-session terminal histories (loaded on demand, cached in memory).
    pub(crate) terminal_histories: HashMap<String, crate::tui::terminal_history::SessionHistory>,
    /// Terminal scrollback search state (Ctrl+F).
    pub(crate) terminal_search: Option<TerminalSearch>,
    /// CLI launch usage counters (persisted to disk).
    pub(crate) cli_usage: crate::domain::usage_stats::CliUsage,

    // Activity panel scroll
    pub(crate) sync_scroll_offset: u16,
    /// Last rendered area of the activity panel (used for mouse hit-testing).
    pub(crate) last_sync_area: Option<ratatui::layout::Rect>,

    // CT1 multi-face right panel: the switching rule lives in
    // `crate::tui::app::panel_face` — these are its inputs and outputs.
    /// Currently visible face. Only ever changed by the panel-face tick or
    /// by an explicit pin/pick — never as a side effect of rendering — so
    /// switching faces can't flicker or force a full-screen redraw.
    pub(crate) panel_face: PanelFace,
    /// The face pinned from the picker (`None` = automatic mode).
    pub(crate) panel_pinned: Option<PanelFace>,
    /// Event-driven face with its 10-second dwell deadline. A newer event
    /// replaces it and restarts the dwell; events never queue.
    pub(crate) panel_dwell_face: Option<PanelFace>,
    pub(crate) panel_dwell_until: Option<std::time::Instant>,
    /// Human-readable reason for the current dwell (`"new knowledge"`,
    /// `"backlog changed"`). Shown in the title while the dwell holds.
    pub(crate) panel_dwell_reason: Option<String>,
    /// Reason for the last automatic switch (`"loop running"`, `"new
    /// knowledge"`, …). Rendered as a badge so a face never appears
    /// unexplained. `None` after a manual pin/pick.
    pub(crate) panel_last_reason: Option<String>,
    /// Whether the face picker overlay is open, and its cursor.
    pub(crate) panel_picker_open: bool,
    pub(crate) panel_picker_idx: usize,
    /// True while the mouse is pressed/dragging inside the panel or the
    /// panel was clicked into (keyboard focus claimed by the panel).
    /// While set, any pending automatic switch is dropped, not deferred.
    pub(crate) panel_focused: bool,
    /// Set by a mouse-wheel scroll over the panel; consumed (cleared) by
    /// the next panel-face tick, which drops the pending switch with it.
    pub(crate) panel_interacting: bool,
    /// Baselines for event detection: knowledge-entry and backlog counts
    /// at the last tick. A change fires a Knowledge event.
    pub(crate) panel_last_knowledge_count: usize,
    pub(crate) panel_last_backlog_count: usize,
    /// Loop-running state at the last tick (the STATE input).
    pub(crate) panel_last_loop_running: bool,
    /// False until the first tick has seeded the baselines above, so the
    /// initial data load never fires a spurious event.
    pub(crate) panel_baselines_init: bool,

    // RAG pause state (synced from daemon_state table)
    pub(crate) rag_paused: bool,
    /// Whether the embedding model is currently loaded in the daemon's
    /// memory (synced from daemon_state table — see `rag::status`).
    pub(crate) rag_model_loaded: bool,
    /// Configured embeddings model id, snapshotted from config.toml at
    /// startup — lets the status widgets show "unavailable" when the
    /// configured provider is one this binary cannot serve (see
    /// `rag::status::compute_rag_status`), instead of only failing at
    /// query time.
    pub(crate) rag_embeddings_model: String,
    /// Current acquisition (download+prepare) state for the configured
    /// local embedding model, read from daemon_state per-model keys.
    /// `None` when the model is already fully available or was never
    /// tracked — the normal ready/sleeping logic applies.
    pub(crate) rag_acquisition_state: Option<crate::rag::status::AcquisitionState>,
    /// Whether the RagInfo panel has focus in Agents sidebar mode.
    pub(crate) agents_rag_focused: bool,

    // RAG Playground state
    pub(crate) playground_active: bool,
    pub(crate) playground_query: String,
    pub(crate) playground_results: Vec<SearchResult>,
    pub(crate) playground_selected: usize,
    pub(crate) playground_last_search: std::time::Instant,
    pub(crate) playground_search_pending: bool,
    /// In-flight background playground search (B23): receiving end of the
    /// worker thread running embed+search off the UI thread, tagged with the
    /// query it executed. `Some` while a search is executing — the TUI keeps
    /// rendering and polling instead of blocking on the model load.
    pub(crate) playground_search_rx:
        Option<std::sync::mpsc::Receiver<(String, anyhow::Result<Vec<SearchResult>>)>>,
    pub(crate) playground_last_executed_query: String,
    /// Whether the playground is showing a single chunk in detail mode.
    pub(crate) playground_detail_mode: bool,
    /// Scroll offset for the detail view content.
    pub(crate) playground_scroll: u16,
    /// Optional project hash to filter search results. None = Global.
    pub(crate) playground_project_hash: Option<String>,
    /// Tracks whether the session-start protocol block has been sent per
    /// session. Keyed by `App::current_prompt_session_key` (agent/session
    /// ID when one exists, workdir as a fallback) rather than by workdir
    /// alone, so two different agent sessions sharing a workdir don't share
    /// delivery state.
    pub(crate) session_protocol_state: HashMap<String, SessionProtocolState>,

    pub(crate) active_sandbox: Option<crate::domain::sandbox::Sandbox>,

    // Project relation graph
    pub(crate) project_relation_dialog: Option<ProjectRelationDialog>,
    pub(crate) project_graph_edges: Vec<ProjectGraphEdge>,
    pub(crate) project_graph_trees: Vec<Vec<String>>,

    // Nursery — temporary path for seed creation loop
    pub(crate) nursery_path: Option<std::path::PathBuf>,

    /// Whether the terminal supports and has enabled the Kitty keyboard
    /// enhancement protocol (Shift+Enter disambiguation).
    pub(crate) keyboard_enhancement_active: bool,

    // Atmosphere engine
    pub(crate) atmosphere: crate::tui::atmosphere::SceneManager,
    pub(crate) atmosphere_ctx: crate::tui::atmosphere::AtmosphereCtx,
    /// Previous mouse position for delta calculation.
    pub(crate) atmosphere_last_mouse: (u16, u16),
    /// When true, particles are not rendered (suppressed by held click).
    pub(crate) atmosphere_hidden: bool,

    // Gamification
    pub(crate) mission_manager: crate::tui::gamification::MissionManager,
    pub(crate) mission_pending_events: Vec<crate::tui::gamification::MissionEvent>,
    pub(crate) max_cpu_frequency_seen: Option<u64>,
    /// Monotonic anchor for accumulating real Canopy uptime (persisted in state).
    pub(crate) uptime_anchor: Option<std::time::Instant>,
}

#[derive(Clone)]
#[allow(dead_code)]
pub(crate) struct ProjectGraphEdge {
    pub from_name: String,
    pub to_name: String,
    pub from_hash: String,
    pub to_hash: String,
    pub relation: String,
}

#[derive(Clone)]
pub(crate) struct ProjectRelationDialog {
    pub from_hash: String,
    pub from_name: String,
    pub available: Vec<crate::db::intelligence::IntelligenceNodeRecord>,
    pub filtered: Vec<usize>,
    pub selected_idx: usize,
    pub relation_idx: usize,
    pub relation_types: Vec<String>,
    pub filter_buffer: String,
    pub error: Option<String>,
}

/// Tracks session-start protocol block delivery per session for idempotency.
/// The block is the static "[START HERE — required]" contract, not the
/// per-turn workspace/intents/chatter context, which is sent every turn
/// regardless of this state.
#[derive(Clone, Default)]
pub(crate) struct SessionProtocolState {
    pub protocol_sent: bool,
}

#[cfg(test)]
mod router_routes_dialog_tests {
    use super::{LoopEditorDialog, RouterField, RouterRouteDraft};

    fn dialog_with_routes(labels: &[&str]) -> LoopEditorDialog {
        let routes = labels
            .iter()
            .map(|label| RouterRouteDraft {
                label: label.to_string(),
                description: format!("{label} desc"),
                target_node_id: None,
            })
            .collect();
        let targets = vec![
            ("n1".to_string(), "Node One".to_string()),
            ("n2".to_string(), "Node Two".to_string()),
        ];
        LoopEditorDialog::new_router_routes(
            "router1".to_string(),
            "Classify".to_string(),
            "title".to_string(),
            "help".to_string(),
            routes,
            String::new(),
            targets,
        )
    }

    #[test]
    fn router_next_field_cycles_through_a_route_then_advances_to_the_next() {
        let mut dialog = dialog_with_routes(&["a", "b"]);
        assert_eq!(dialog.router_field, RouterField::Label);

        dialog.router_next_field();
        assert_eq!(dialog.router_field, RouterField::Description);
        assert_eq!(dialog.router_route_index, 0);

        dialog.router_next_field();
        assert_eq!(dialog.router_field, RouterField::Target);
        assert_eq!(dialog.router_route_index, 0);

        dialog.router_next_field();
        assert_eq!(dialog.router_field, RouterField::Label);
        assert_eq!(dialog.router_route_index, 1, "wraps to the next route");
    }

    #[test]
    fn router_prev_field_is_the_exact_inverse() {
        let mut dialog = dialog_with_routes(&["a", "b"]);
        dialog.router_route_index = 1;
        dialog.router_field = RouterField::Label;

        dialog.router_prev_field();
        assert_eq!(dialog.router_field, RouterField::Target);
        assert_eq!(
            dialog.router_route_index, 0,
            "wraps back to the previous route"
        );
    }

    #[test]
    fn router_push_and_pop_char_edit_the_focused_route_field() {
        let mut dialog = dialog_with_routes(&["", ""]);
        dialog.router_routes[0].description.clear();
        dialog.router_field = RouterField::Label;
        dialog.router_push_char('b');
        dialog.router_push_char('i');
        dialog.router_push_char('n');
        assert_eq!(dialog.router_routes[0].label, "bin");

        dialog.router_pop_char();
        assert_eq!(dialog.router_routes[0].label, "bi");

        dialog.router_next_field();
        dialog.router_push_char('x');
        assert_eq!(dialog.router_routes[0].description, "x");
        // The other route is untouched.
        assert_eq!(dialog.router_routes[1].label, "");
    }

    #[test]
    fn cycle_router_target_moves_through_none_and_every_candidate() {
        let mut dialog = dialog_with_routes(&["a"]);
        assert_eq!(dialog.router_routes[0].target_node_id, None);

        dialog.cycle_router_target(true);
        assert_eq!(
            dialog.router_routes[0].target_node_id.as_deref(),
            Some("n1")
        );

        dialog.cycle_router_target(true);
        assert_eq!(
            dialog.router_routes[0].target_node_id.as_deref(),
            Some("n2")
        );

        dialog.cycle_router_target(true);
        assert_eq!(
            dialog.router_routes[0].target_node_id, None,
            "wraps back to unwired after the last candidate"
        );

        dialog.cycle_router_target(false);
        assert_eq!(
            dialog.router_routes[0].target_node_id.as_deref(),
            Some("n2")
        );
    }

    #[test]
    fn router_clear_target_unwires_the_focused_route() {
        let mut dialog = dialog_with_routes(&["a"]);
        dialog.router_routes[0].target_node_id = Some("n1".to_string());
        dialog.router_clear_target();
        assert_eq!(dialog.router_routes[0].target_node_id, None);
    }

    #[test]
    fn router_add_route_appends_and_focuses_a_fresh_unwired_route() {
        let mut dialog = dialog_with_routes(&["a", "b"]);
        dialog.router_add_route();

        assert_eq!(dialog.router_routes.len(), 3);
        assert_eq!(dialog.router_route_index, 2);
        assert_eq!(dialog.router_field, RouterField::Label);
        assert_eq!(dialog.router_routes[2].label, "");
        assert_eq!(dialog.router_routes[2].target_node_id, None);
    }

    #[test]
    fn router_remove_route_drops_it_and_clears_a_dangling_fallback() {
        let mut dialog = dialog_with_routes(&["a", "b", "c"]);
        dialog.router_fallback = "b".to_string();
        dialog.router_route_index = 1;

        dialog.router_remove_route();

        assert_eq!(dialog.router_routes.len(), 2);
        assert!(dialog.router_routes.iter().all(|r| r.label != "b"));
        assert_eq!(
            dialog.router_fallback, "",
            "fallback naming the removed route is cleared, not left dangling"
        );
    }

    #[test]
    fn router_remove_route_keeps_an_unrelated_fallback() {
        let mut dialog = dialog_with_routes(&["a", "b", "c"]);
        dialog.router_fallback = "a".to_string();
        dialog.router_route_index = 1; // removes "b"

        dialog.router_remove_route();

        assert_eq!(dialog.router_fallback, "a");
    }

    #[test]
    fn router_set_fallback_names_the_focused_route() {
        let mut dialog = dialog_with_routes(&["a", "b"]);
        dialog.router_route_index = 1;
        dialog.router_set_fallback();
        assert_eq!(dialog.router_fallback, "b");
    }
}
