use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

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

/// The sidebar's three thematic layers, stacked between the pinned RAG
/// summary (top) and sysinfo dashboard (bottom). Each is independently
/// collapsible; `App::sidebar_layer` tracks which one currently has
/// keyboard/mouse focus.
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

/// Per-loop rendering data for the sidebar's `Loops` section — spec progress
/// and whether the loop is stuck on a reported blocker (a `Paused` loop whose
/// latest run recorded a `blocker`, see `loop_report_blocker`). Computed once
/// per refresh cycle (`App::refresh_loops`) rather than queried per frame.
#[derive(Clone, Copy, Default)]
pub(crate) struct LoopSidebarMeta {
    pub done: usize,
    pub total: usize,
    pub blocked: bool,
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

#[derive(Clone)]
pub(crate) enum LoopEditorMode {
    AgentPrompt,
    NodeConfig,
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
    /// Which sidebar layer currently has keyboard/mouse focus.
    pub(crate) sidebar_layer: SidebarLayer,
    /// Persisted (see `db::state`) collapsed state for each layer.
    pub(crate) live_collapsed: bool,
    pub(crate) automation_collapsed: bool,
    pub(crate) knowledge_collapsed: bool,
    /// Which of Automation's two sub-lists is active for navigation.
    pub(crate) automation_kind: AutomationKind,
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
    pub(crate) delete_loop_confirm: bool,

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
    /// Mouse hit-test rows for the three layer headers, populated during
    /// draw: clicking toggles that layer's collapsed state.
    pub(crate) layer_header_click_map: Vec<(SidebarLayer, u16, u16)>,
    pub(crate) loops: Vec<Loop>,
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

    // RAG pause state (synced from daemon_state table)
    pub(crate) rag_paused: bool,
    /// Whether the embedding model is currently loaded in the daemon's
    /// memory (synced from daemon_state table — see `rag::status`).
    pub(crate) rag_model_loaded: bool,
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
    /// Tracks whether the system block has been sent per workdir.
    pub(crate) workdir_system_state: HashMap<PathBuf, WorkdirSystemState>,

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

/// Tracks system block delivery per workdir for idempotency.
#[derive(Clone, Default)]
pub(crate) struct WorkdirSystemState {
    pub sent: bool,
    pub sent_as_solo: bool,
}
