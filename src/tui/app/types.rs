use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use crate::application::notification_service::NotificationService;
use crate::db::project::{RagInfoSummary, RagQueueItem};
use crate::db::Database;
use crate::domain::models::{Agent, RunLog};
use crate::domain::project::Project;
use crate::domain::sync::{ActiveIntent, SyncMessage, WorkspaceStatus};
use crate::domain::workflow::{Workflow, WorkflowDetails, WorkflowNodeRun};
use crate::rag::vector_store::SearchResult;
use crate::tui::agent::InteractiveAgent;
use crate::tui::app::dialog::{LaunchpadDialog, NewAgentDialog, SimplePromptDialog};
use crate::tui::app::terminal_search::TerminalSearch;
/// Unified entry in the sidebar.
#[allow(clippy::large_enum_variant)]
pub enum AgentEntry {
    Agent(Agent),
    Interactive(usize), // index into App::interactive_agents
    Terminal(usize),    // index into App::terminal_agents
    Group(usize),       // index into App::split_groups
}

impl AgentEntry {
    pub fn id<'a>(&'a self, app: &'a App) -> &'a str {
        match self {
            Self::Agent(a) => &a.id,
            Self::Interactive(idx) => app.interactive_agents.get(*idx).map_or("?", |a| &a.name),
            Self::Terminal(idx) => app.terminal_agents.get(*idx).map_or("?", |a| &a.name),
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
    Agent,
    ContextTransfer,
    RagTransfer,
    PromptTemplateDialog,
    WorkflowEditorDialog,
    ProjectRelationDialog,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum ProjectsPanelFocus {
    Projects,
    Workflows,
    RagInfo,
}

#[derive(Clone)]
pub(crate) enum WorkflowEditorMode {
    AgentPrompt,
    NodeConfig,
}

#[derive(Clone)]
pub(crate) struct WorkflowEditorDialog {
    pub node_id: String,
    pub node_name: String,
    pub title: String,
    pub help: String,
    pub buffer: String,
    pub cursor: usize,
    pub mode: WorkflowEditorMode,
    pub parse_error: Option<String>,
}

impl WorkflowEditorDialog {
    pub fn new(
        node_id: String,
        node_name: String,
        title: String,
        help: String,
        buffer: String,
        mode: WorkflowEditorMode,
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

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum SidebarMode {
    Agents,
    Projects,
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
    pub(crate) sidebar_mode: SidebarMode,
    pub(crate) log_content: String,
    pub(crate) log_scroll: u16,
    pub(crate) running: bool,
    pub(crate) new_agent_dialog: Option<NewAgentDialog>,
    pub(crate) launchpad_dialog: Option<LaunchpadDialog>,
    pub(crate) pending_launch_dialog: Option<NewAgentDialog>,
    pub(crate) quit_confirm: bool,
    pub(crate) delete_project_confirm: bool,
    pub(crate) delete_workflow_confirm: bool,

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
    pub(crate) projects: Vec<Project>,
    pub(crate) selected_project: usize,
    pub(crate) projects_panel_focus: ProjectsPanelFocus,
    pub(crate) workflows: Vec<Workflow>,
    pub(crate) selected_workflow_id: Option<String>,
    pub(crate) workflow_details: Option<WorkflowDetails>,
    pub(crate) workflow_runs: Vec<WorkflowNodeRun>,
    pub(crate) workflow_selected_spec: usize,
    pub(crate) workflow_selected_node: usize,
    pub(crate) workflow_editor_dialog: Option<WorkflowEditorDialog>,
    pub(crate) global_rag_queue: Vec<RagQueueItem>,
    pub(crate) selected_rag_queue: usize,
    pub(crate) rag_info: RagInfoSummary,
    /// Per-file RAG status loaded from `rag_file_events` table.
    pub(crate) rag_file_status: Vec<crate::db::project::RagPerFileStatus>,
    pub(crate) sidebar_visible: bool,
    pub(crate) hidden_activity_workdirs: HashSet<String>,
    pub(crate) forced_activity_workdirs: HashSet<String>,
    pub(crate) term_width: u16,
    pub(crate) show_legend: bool,
    pub(crate) show_copied: bool,
    pub(crate) copied_at: std::time::Instant,
    pub(crate) last_scroll_at: std::time::Instant,
    pub(crate) last_panel_inner: (u16, u16),
    pub(crate) last_panel_y: u16,
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
    /// Whether the RagInfo panel has focus in Agents sidebar mode.
    pub(crate) agents_rag_focused: bool,

    // RAG Playground state
    pub(crate) playground_active: bool,
    pub(crate) playground_query: String,
    pub(crate) playground_results: Vec<SearchResult>,
    pub(crate) playground_selected: usize,
    pub(crate) playground_last_search: std::time::Instant,
    pub(crate) playground_search_pending: bool,
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
