use anyhow::Result;
use chrono::Timelike;
use ratatui::layout::Rect;
use ratatui::style::Color;
use std::collections::{HashMap, HashSet};

use super::at_picker::AtPicker;
use crate::db::Database;
use crate::domain::project::Project;
use crate::tui::app::types::Focus;
use std::path::Path;

/// Picker state for adding/removing sections
#[derive(Debug, Clone, PartialEq, Default)]
pub enum SectionPickerMode {
    #[default]
    None,
    AddSection {
        selected: usize,
    },
    RemoveSection {
        selected: usize,
    },
    AddCustom {
        input: String,
    },
    /// Skills picker for the Tools section — entries are `(label, raw_name, prefix)`
    SkillsPicker {
        selected: usize,
        /// `(display_label, raw_name, prefix)` — `prefix` is "skill" or "global"
        entries: Vec<(String, String, String)>,
        /// `None` → create a new tools section on confirm; `Some(id)` → replace content of that section
        replace_id: Option<String>,
    },
    ProjectPicker {
        selected: usize,
        entries: Vec<ProjectPickerEntry>,
    },
    /// Prompt-preset picker (P2): entries are read fresh from
    /// `~/.canopy/prompts/*.md` (P1) each time the picker opens — `(name,
    /// preview, full content)`. `filter` is the user's typed substring
    /// filter; `selected` indexes into the FILTERED list, mirroring
    /// `NewAgentDialog`'s CLI picker (`cli_picker_idx`).
    PresetPicker {
        selected: usize,
        entries: Vec<(String, String, String)>,
        filter: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectPickerEntry {
    pub hash: String,
    pub name: String,
    pub path: String,
}

/// The send control's selector value (U11): lateral arrows toggle between
/// sending immediately and scheduling a date-time.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SendChoice {
    #[default]
    Now,
    Date,
}

/// Which top-of-dialog tab is active. `Normal` is the section-based form;
/// `Raw` is a single free-text field that is sent as-is (or, when empty,
/// previews the composed Normal-form prompt). Pilot of clickable "windows".
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
pub enum PromptTab {
    #[default]
    Normal,
    Raw,
}

/// Section id backing the Raw tab's free-text buffer. It deliberately lives in
/// `sections`/`section_cursors`/`section_scrolls` (so all the existing editing
/// machinery applies) but is NEVER added to `enabled_sections`, so it stays
/// out of focus navigation, `is_empty`, and the composed-prompt build.
pub const RAW_SECTION_ID: &str = "__raw__";

/// Display label (with padding) of the Normal tab in the tab bar.
pub const TAB_NORMAL_LABEL: &str = " Normal ";
/// Display label (with padding) of the Raw tab in the tab bar.
pub const TAB_RAW_LABEL: &str = " Raw ";

/// Inline date-time picker state for the send control (U11). Opened with
/// Enter on `send: date`, preseeded with the current local time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SendAtEdit {
    pub value: chrono::NaiveDateTime,
    /// Focused field: 0=year, 1=month, 2=day, 3=hour, 4=minute.
    pub field: usize,
    /// In-progress numeric accumulator for the focused field while the user is
    /// typing digits (U11): the running value and how many digits have been
    /// entered since the field was (re)started. Reset on field change and on
    /// arrow adjust so a fresh digit always starts a new number.
    pub typed: u32,
    pub typed_len: u8,
}

/// Digits a field accepts before it is "full" and auto-advances: the year is
/// 4 digits wide, every other field is 2.
fn field_digit_width(field: usize) -> u8 {
    if field == 0 {
        4
    } else {
        2
    }
}

/// Month arithmetic for the send picker: ±N months with day clamping
/// (chrono's checked add/sub semantics — Jan 31 + 1 month = Feb 28/29).
fn add_months(value: chrono::NaiveDateTime, delta: i64) -> Option<chrono::NaiveDateTime> {
    if delta >= 0 {
        value.checked_add_months(chrono::Months::new(delta as u32))
    } else {
        value.checked_sub_months(chrono::Months::new(delta.unsigned_abs() as u32))
    }
}

/// Number of days in a given month (handles leap years).
fn days_in_month(year: i32, month: u32) -> u32 {
    let (next_year, next_month) = if month == 12 {
        (year + 1, 1)
    } else {
        (year, month + 1)
    };
    match (
        chrono::NaiveDate::from_ymd_opt(year, month, 1),
        chrono::NaiveDate::from_ymd_opt(next_year, next_month, 1),
    ) {
        (Some(first), Some(next)) => (next - first).num_days() as u32,
        _ => 28,
    }
}

/// Set one component (year/month/day/hour/minute) of `value` to `num`, clamping
/// it into that component's valid range and clamping the day to the resulting
/// month length. Returns `None` only if chrono still rejects the date, in which
/// case the caller keeps the previous value.
fn with_field(
    value: chrono::NaiveDateTime,
    field: usize,
    num: u32,
) -> Option<chrono::NaiveDateTime> {
    use chrono::{Datelike, NaiveDate, Timelike};
    let date = value.date();
    let time = value.time();
    match field {
        0 => {
            let year = num.clamp(1, 9999) as i32;
            let day = date.day().min(days_in_month(year, date.month()));
            NaiveDate::from_ymd_opt(year, date.month(), day).map(|d| d.and_time(time))
        }
        1 => {
            let month = num.clamp(1, 12);
            let day = date.day().min(days_in_month(date.year(), month));
            NaiveDate::from_ymd_opt(date.year(), month, day).map(|d| d.and_time(time))
        }
        2 => {
            let day = num.clamp(1, days_in_month(date.year(), date.month()));
            NaiveDate::from_ymd_opt(date.year(), date.month(), day).map(|d| d.and_time(time))
        }
        3 => {
            let hour = num.min(23);
            date.and_hms_opt(hour, time.minute(), time.second())
        }
        _ => {
            let minute = num.min(59);
            date.and_hms_opt(time.hour(), minute, time.second())
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RagScope<'a> {
    Global,
    Project(&'a str),
}

/// New simplified prompt template dialog with dynamic sections
/// Now supports multiple instances of the same section type
pub struct SimplePromptDialog {
    /// Map of unique section IDs to their content
    pub sections: HashMap<String, String>,
    /// Ordered list of section IDs currently enabled
    pub enabled_sections: Vec<String>,
    /// Which section field is currently focused
    pub focused_section: usize,
    /// Previous focus before opening the dialog
    pub prev_focus: Option<Focus>,
    /// State for the section picker modal
    pub picker_mode: SectionPickerMode,
    /// Counter for generating unique IDs per section type
    pub section_counters: HashMap<String, usize>,
    /// Per-section cursor positions (char index)
    pub section_cursors: HashMap<String, usize>,
    /// Per-section scroll offsets (visual line)
    pub section_scrolls: HashMap<String, usize>,
    /// Active `@`-file picker (inline dropdown), if open.
    pub at_picker: Option<AtPicker>,
    /// Collapsed paste content: placeholder text is stored in `sections`,
    /// the real pasted content lives here and is used for building the prompt.
    pub collapsed_pastes: HashMap<String, String>,
    /// Section IDs that are read-only (auto-filled, cannot be edited).
    pub locked_sections: HashSet<String>,
    /// Invisible system block rendered at the top of the final prompt so its
    /// protocol is read before the task. None = omit. Set once per workdir
    /// session (idempotent).
    pub system_content: Option<String>,
    /// Send-timing selector (U11): `now` sends immediately, `date`
    /// schedules the delivery at `send_at`.
    pub send_choice: SendChoice,
    /// Confirmed scheduled delivery, local wall-clock (U11). Only honored
    /// when `send_choice` is [`SendChoice::Date`].
    pub send_at: Option<chrono::NaiveDateTime>,
    /// Inline date-time picker state while the user is editing (U11).
    pub send_edit: Option<SendAtEdit>,
    /// Inline validation hint for the send control (e.g. past time picked).
    pub send_error: Option<String>,
    /// A last-prompt recall (Ctrl+L) awaiting the standard confirm pattern
    /// because the builder currently has non-empty content. `None` once
    /// confirmed/canceled. Not persisted across dialog openings.
    pub pending_recall: Option<crate::db::last_prompts::LastPrompt>,
    /// Active tab at the top of the dialog (Normal form vs Raw free-text).
    pub active_tab: PromptTab,
    /// Cached composed-prompt string shown as the read-only preview when the
    /// Raw tab is active and its buffer is empty. Recomputed on entering the
    /// Raw tab (the Normal form can't change while Raw is shown). Transient —
    /// never persisted.
    pub raw_preview: Option<String>,
    /// Vertical scroll offset (in preview lines) for the read-only raw
    /// preview. Reset whenever the preview is recomputed. Transient.
    pub raw_preview_scroll: usize,
    /// Wheel-driven scroll offset for the editable raw buffer (view-only,
    /// separate from the cursor). `None` means cursor-follow is in effect;
    /// `Some(n)` means the user scrolled with the wheel and the view is
    /// pinned at line `n` until the next cursor movement clears it.
    pub raw_edit_scroll: Option<usize>,
    /// Selected row in the pending-scheduled-sends list panel (B33). `Some(i)`
    /// means the list region has keyboard focus and row `i` is highlighted;
    /// `None` means the list (if shown) is idle and keys go to the normal
    /// fields. Transient — recomputed against the live DB list on each frame.
    pub scheduled_list_selected: Option<usize>,
    /// When set, a re-confirmed schedule UPDATES this existing scheduled-send
    /// row in place (delete + re-insert) instead of creating a duplicate — the
    /// select-to-edit flow (B33). Transient; cleared once the builder closes.
    pub editing_scheduled_id: Option<String>,
}

impl SimplePromptDialog {
    pub fn new() -> Self {
        let mut counters = HashMap::new();
        counters.insert("instruction".to_string(), 2usize);
        counters.insert("context".to_string(), 2usize);
        let mut cursors = HashMap::new();
        cursors.insert("instruction_1".to_string(), 0usize);
        let mut scrolls = HashMap::new();
        scrolls.insert("instruction_1".to_string(), 0usize);
        let mut sections = HashMap::new();
        sections.insert("instruction_1".to_string(), String::new());
        Self {
            sections,
            enabled_sections: vec!["instruction_1".to_string()],
            // Focus starts on the first section — the send control (virtual
            // index 0) sits at the bottom and is reached by wrapping (U11).
            focused_section: 1,
            prev_focus: None,
            picker_mode: SectionPickerMode::None,
            section_counters: counters,
            section_cursors: cursors,
            section_scrolls: scrolls,
            at_picker: None,
            collapsed_pastes: HashMap::new(),
            locked_sections: HashSet::new(),
            system_content: None,
            send_choice: SendChoice::Now,
            send_at: None,
            send_edit: None,
            send_error: None,
            pending_recall: None,
            active_tab: PromptTab::Normal,
            raw_preview: None,
            raw_preview_scroll: 0,
            raw_edit_scroll: None,
            scheduled_list_selected: None,
            editing_scheduled_id: None,
        }
    }

    /// True when every enabled section is blank — the state Ctrl+L's confirm
    /// pattern treats as "safe to overwrite without asking".
    pub fn is_empty(&self) -> bool {
        self.enabled_sections.iter().all(|section_id| {
            self.section_content_for_build(section_id)
                .map(str::trim)
                .unwrap_or("")
                .is_empty()
        })
    }

    /// Replace all builder content with a single instruction section holding
    /// `text` verbatim. Used to recall a prompt whose structured builder
    /// state wasn't captured — a scheduled send recovered after its target
    /// session died only has the flattened prompt string (see
    /// `Database::insert_failed_scheduled_send`), so a faithful
    /// section-by-section restore isn't possible.
    pub fn load_flat_text(&mut self, text: &str) {
        self.sections.clear();
        self.enabled_sections.clear();
        self.section_cursors.clear();
        self.section_scrolls.clear();
        self.collapsed_pastes.clear();
        self.locked_sections.clear();
        self.section_counters.clear();
        self.section_counters.insert("instruction".to_string(), 2);

        let cursor = text.chars().count();
        self.sections
            .insert("instruction_1".to_string(), text.to_string());
        self.enabled_sections.push("instruction_1".to_string());
        self.section_cursors
            .insert("instruction_1".to_string(), cursor);
        self.section_scrolls.insert("instruction_1".to_string(), 0);
        self.focused_section = 1; // 0 is the send_at virtual field
        self.picker_mode = SectionPickerMode::None;
        self.at_picker = None;
    }

    /// Get cursor position for a section
    pub fn cursor(&self, section: &str) -> usize {
        self.section_cursors.get(section).copied().unwrap_or(0)
    }

    /// Get scroll offset for a section
    pub fn scroll(&self, section: &str) -> usize {
        self.section_scrolls.get(section).copied().unwrap_or(0)
    }

    /// Drive the `@`-picker's debounced search. Call once per UI tick so a
    /// pending search runs after typing pauses.
    pub fn tick_at_picker(&mut self) {
        if let Some(picker) = self.at_picker.as_mut() {
            picker.tick_search();
        }
    }

    /// Returns true if the section is locked (read-only).
    pub fn is_locked(&self, section_id: &str) -> bool {
        self.locked_sections.contains(section_id)
    }

    /// Mark a section as locked (read-only).
    pub fn lock_section(&mut self, section_id: &str) {
        self.locked_sections.insert(section_id.to_string());
    }

    /// Generate unique ID for a section instance (always uses `name_N` format, N starting at 1).
    fn generate_section_id(&mut self, section_name: &str) -> String {
        let counter = self
            .section_counters
            .entry(section_name.to_string())
            .or_insert(1);
        let id = format!("{}_{}", section_name, counter);
        *counter += 1;
        id
    }

    fn section_type(section_id: &str) -> &str {
        Self::get_available_sections()
            .into_iter()
            .map(|(name, _)| name)
            .find(|name| section_id == *name || section_id.starts_with(&format!("{name}_")))
            .unwrap_or(section_id)
    }

    fn section_matches_prefix(section_id: &str, prefix: &str) -> bool {
        section_id == prefix || section_id.starts_with(&format!("{prefix}_"))
    }

    fn instruction_count(&self) -> usize {
        self.enabled_sections
            .iter()
            .filter(|section_id| Self::section_matches_prefix(section_id, "instruction"))
            .count()
    }

    fn insert_section(&mut self, section_name: &str, content: String) -> String {
        let unique_id = self.generate_section_id(section_name);
        let cursor_pos = content.chars().count();
        self.enabled_sections.push(unique_id.clone());
        self.sections.insert(unique_id.clone(), content);
        self.section_cursors.insert(unique_id.clone(), cursor_pos);
        self.section_scrolls.insert(unique_id.clone(), 0);
        self.collapsed_pastes.remove(&unique_id);
        // Focus the newly added section: enabled_sections index (len-1) maps to
        // focus index (len) because focus 0 is the virtual send control.
        self.focused_section = self.enabled_sections.len();
        unique_id
    }

    /// Add a section instance (can be same type multiple times)
    pub fn add_section(&mut self, section_name: &str) {
        self.insert_section(section_name, String::new());
    }

    /// Add a section with pre-existing content (used for context transfer and initial content).
    /// Returns the generated section ID.
    pub fn add_section_with_content(&mut self, section_name: &str, content: String) -> String {
        self.insert_section(section_name, content)
    }

    /// Remove a specific section instance.
    /// The last remaining instruction section cannot be removed.
    pub fn remove_section(&mut self, section_id: &str) {
        if Self::section_matches_prefix(section_id, "instruction") && self.instruction_count() <= 1
        {
            return;
        }

        self.enabled_sections.retain(|s| s != section_id);
        self.sections.remove(section_id);
        self.section_cursors.remove(section_id);
        self.section_scrolls.remove(section_id);
        self.collapsed_pastes.remove(section_id);
        if self.focused_section > 0 {
            self.focused_section = self.focused_section.saturating_sub(1);
        }
    }

    /// Get available section types (these can always be added again).
    /// RAG Search is only included when an embeddings model is configured.
    pub fn get_available_sections() -> Vec<(&'static str, &'static str)> {
        let rag_enabled = dirs::home_dir()
            .map(|h| {
                let config = crate::domain::canopy_config::CanopyConfig::load(&h.join(".canopy"));
                !config.embeddings_model.trim().is_empty()
            })
            .unwrap_or(false);

        let mut sections = vec![
            ("instruction", "Instruction"),
            ("goal", "Goal"),
            ("context", "Context"),
            ("project_context", "Project Context"),
            ("resources", "Resources"),
        ];
        if rag_enabled {
            sections.push(("rag_search", "RAG Search"));
        }
        sections.extend([("constraints", "Constraints"), ("tools", "Tools")]);
        sections.push(("preset", "Preset"));
        sections
    }

    /// Return true if this section ID represents the read-only "tools" section.
    pub fn is_tools_section(section_id: &str) -> bool {
        section_id == "tools" || section_id.starts_with("tools_")
    }

    /// Collect all available skills for the skills picker.
    /// Returns `Vec<(display_label, raw_name, prefix)>`.
    pub fn collect_skills_for_picker(workdir: &std::path::Path) -> Vec<(String, String, String)> {
        let mut entries: Vec<(String, String, String)> = Vec::new();
        let project = workdir.join(".agents").join("skills");
        add_skills_from_dir(&project, "skill", &mut entries);
        if let Some(global) = dirs::home_dir().map(|h| h.join(".agents").join("skills")) {
            if global != project {
                add_skills_from_dir(&global, "global", &mut entries);
            }
        }
        entries
    }

    /// Collect prompt presets for the Preset picker (P2): every `*.md` file
    /// under `dir` (typically `~/.canopy/prompts/`, see
    /// `domain::prompts::prompts_dir`), sorted by name. Returns `(name,
    /// preview, full content)` where `preview` is the file's first
    /// non-empty line. Read fresh at picker-open time — no caching — so an
    /// external edit shows up immediately (spec P2). A missing directory
    /// yields an empty list rather than an error; an unreadable file is
    /// skipped rather than aborting the whole listing.
    pub fn collect_presets_for_picker(dir: &std::path::Path) -> Vec<(String, String, String)> {
        let Ok(read_dir) = std::fs::read_dir(dir) else {
            return Vec::new();
        };
        let mut entries: Vec<(String, String, String)> = read_dir
            .filter_map(|entry| entry.ok())
            .filter(|entry| entry.path().extension().and_then(|e| e.to_str()) == Some("md"))
            .filter_map(|entry| {
                let path = entry.path();
                let name = path.file_stem()?.to_str()?.to_string();
                let content = std::fs::read_to_string(&path).ok()?;
                let preview = content
                    .lines()
                    .map(str::trim)
                    .find(|line| !line.is_empty())
                    .unwrap_or("")
                    .to_string();
                Some((name, preview, content))
            })
            .collect();
        entries.sort_by(|a, b| a.0.cmp(&b.0));
        entries
    }

    /// Indices into `entries` whose name matches `filter` as a
    /// case-insensitive substring — the same "fuzzy" filter convention
    /// `NewAgentDialog::filtered_cli_indices` uses for the CLI picker.
    pub fn filtered_preset_indices(
        entries: &[(String, String, String)],
        filter: &str,
    ) -> Vec<usize> {
        let query = filter.trim().to_lowercase();
        entries
            .iter()
            .enumerate()
            .filter(|(_, (name, _, _))| query.is_empty() || name.to_lowercase().contains(&query))
            .map(|(idx, _)| idx)
            .collect()
    }

    pub fn collect_projects_for_picker(db: &Database) -> Result<Vec<ProjectPickerEntry>> {
        let mut entries = db
            .list_projects()?
            .into_iter()
            .map(|project| ProjectPickerEntry {
                hash: project.hash,
                name: project.name,
                path: project.path,
            })
            .collect::<Vec<_>>();
        entries.sort_by(|left, right| left.name.cmp(&right.name).then(left.path.cmp(&right.path)));
        Ok(entries)
    }

    /// Set the content of a specific tools section to a single skill label.
    /// Used by the SkillsPicker to replace the skill in an existing tools section.
    pub fn set_tools_section_skill(&mut self, section_id: &str, label: &str) {
        self.sections
            .insert(section_id.to_string(), label.to_string());
    }

    /// Get section types available to add (can always add more instances)
    pub fn get_addable_sections(&self) -> Vec<(&'static str, &'static str)> {
        Self::get_available_sections()
    }

    fn section_display_name(section_id: &str) -> String {
        let section_name = Self::section_type(section_id);
        let label = Self::get_available_sections()
            .into_iter()
            .find(|(name, _)| *name == section_name)
            .map(|(_, label)| label)
            .unwrap_or(section_name);

        if section_id.contains('_') {
            return format!("{} {}", label, section_id.rsplit('_').next().unwrap_or(""));
        }

        label.to_string()
    }

    /// Get section instances available to remove (last instruction is protected)
    pub fn get_removable_sections(&self) -> Vec<(String, String)> {
        let instruction_count = self.instruction_count();
        self.enabled_sections
            .iter()
            .filter(|section_id| {
                !Self::section_matches_prefix(section_id, "instruction") || instruction_count > 1
            })
            .map(|section_id| (section_id.clone(), Self::section_display_name(section_id)))
            .collect()
    }

    /// Get the content for a section
    pub fn get_section_content(&self, section_name: &str) -> String {
        self.sections.get(section_name).cloned().unwrap_or_default()
    }

    /// Set the content for a section
    pub fn set_section_content(&mut self, section_name: &str, content: String) {
        self.sections.insert(section_name.to_string(), content);
    }

    /// Get the real content for a section, resolving any collapsed paste.
    pub fn section_content_for_build(&self, section_id: &str) -> Option<&str> {
        self.collapsed_pastes
            .get(section_id)
            .map(|s| s.as_str())
            .or_else(|| self.sections.get(section_id).map(|s| s.as_str()))
    }

    fn section_entries<'a>(&'a self, prefix: &str) -> Vec<&'a str> {
        self.enabled_sections
            .iter()
            .filter(|section_id| Self::section_matches_prefix(section_id, prefix))
            .filter_map(|section_id| self.section_content_for_build(section_id))
            .map(str::trim)
            .filter(|content| !content.is_empty())
            .collect()
    }

    fn section_lines(&self, prefix: &str) -> Vec<String> {
        self.section_entries(prefix)
            .into_iter()
            .flat_map(str::lines)
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .map(ToOwned::to_owned)
            .collect()
    }

    fn format_project_block(project: &Project) -> String {
        let mut lines = vec![
            format!("name: {}", project.name),
            format!("workdir_hash: {}", project.hash),
            format!("path: {}", project.path),
        ];
        if let Some(description) = project.description.as_deref() {
            lines.push(format!("description: {}", description));
        }
        if let Some(tags) = project.tags.as_deref() {
            lines.push(format!("tags: {}", tags));
        }
        if let Some(indexed_at) = project.indexed_at {
            lines.push(format!("indexed_at: {}", indexed_at));
        }
        lines.join("\n")
    }

    fn format_file_resource(path: &Path) -> String {
        format!("path: {}\nkind: file", path.display())
    }

    fn format_rag_chunk(query: &str, chunk: &crate::rag::vector_store::SearchResult) -> String {
        let dist = chunk
            .distance
            .map_or("—".to_string(), |d| format!("{d:.4}"));
        format!(
            "kind: rag_chunk\nquery: {query}\npath: {}\ndistance: {}\ncontent:\n{}",
            chunk.file_path, dist, chunk.content
        )
    }

    fn lookup_project_reference(db: &Database, entry: &str) -> Result<Option<Project>> {
        if let Some(project) = db.get_project(entry)? {
            return Ok(Some(project));
        }

        let path = Path::new(entry);
        if path.exists() {
            return db.get_project_by_path_or_ancestor(path);
        }

        Ok(None)
    }

    fn resolve_resource_entry(db: &Database, raw: &str) -> String {
        let trimmed = raw.trim();

        if trimmed.starts_with("http://") || trimmed.starts_with("https://") {
            return format!("path: {trimmed}\nkind: url");
        }

        let path = Path::new(trimmed);
        if path.exists() {
            if path.is_dir() {
                if let Ok(Some(project)) = db.get_project_by_path(path) {
                    return Self::format_project_block(&project);
                }
                return format!("path: {}\nkind: directory", path.display());
            }
            return Self::format_file_resource(path);
        }

        format!("content: {trimmed}\nkind: raw")
    }

    fn resolve_rag_scope<'a>(
        query: &'a str,
        default_project_hash: Option<&'a str>,
    ) -> (RagScope<'a>, &'a str) {
        if let Some(rest) = query.strip_prefix("global:") {
            return (RagScope::Global, rest.trim());
        }
        if let Some(rest) = query.strip_prefix("project:") {
            if let Some((project_hash, query)) = rest.split_once(':') {
                return (RagScope::Project(project_hash.trim()), query.trim());
            }
        }

        default_project_hash.map_or((RagScope::Global, query.trim()), |project_hash| {
            (RagScope::Project(project_hash), query.trim())
        })
    }

    fn default_project_hash(&self, db: &Database, current_workdir: &Path) -> Option<String> {
        db.get_project_by_path_or_ancestor(current_workdir)
            .ok()
            .flatten()
            .map(|project| project.hash)
    }

    fn resolve_project_contexts(&self, db: &Database) -> Vec<String> {
        let mut seen_hashes = HashSet::new();
        let mut projects = self
            .section_lines("project_context")
            .into_iter()
            .filter_map(|entry| Self::lookup_project_reference(db, &entry).ok().flatten())
            .filter(|project| seen_hashes.insert(project.hash.clone()))
            .collect::<Vec<_>>();

        for project in self.derived_project_contexts_from_resources(db) {
            if seen_hashes.insert(project.hash.clone()) {
                projects.push(project);
            }
        }

        projects
            .into_iter()
            .map(|project| Self::format_project_block(&project))
            .collect()
    }

    fn derived_project_contexts_from_resources(&self, db: &Database) -> Vec<Project> {
        self.section_lines("resources")
            .into_iter()
            .filter_map(|entry| {
                let path = Path::new(&entry);
                path.exists()
                    .then(|| db.get_project_by_path_or_ancestor(path).ok().flatten())
                    .flatten()
            })
            .collect()
    }

    fn resolve_resource_entries(&self, db: &Database) -> Vec<String> {
        self.section_lines("resources")
            .into_iter()
            .map(|entry| Self::resolve_resource_entry(db, &entry))
            .collect()
    }

    fn search_rag_resources<'a>(
        _db: &Database,
        query: &'a str,
        _default_project_hash: Option<&'a str>,
    ) -> Vec<String> {
        let (scope, resolved_query) = Self::resolve_rag_scope(query, _default_project_hash);
        let _ = scope;
        if resolved_query.is_empty() {
            return Vec::new();
        }

        let canopy_dir = match dirs::home_dir() {
            Some(h) => h.join(".canopy"),
            None => return Vec::new(),
        };
        let config = crate::domain::canopy_config::CanopyConfig::load(&canopy_dir);
        let model = config.embeddings_model.trim();
        if model.is_empty() {
            return Vec::new();
        }
        let Ok(dimensions) = crate::rag::embedding_client::model_dimensions(model) else {
            return Vec::new();
        };
        let Ok(rt) = tokio::runtime::Handle::try_current() else {
            return Vec::new();
        };

        rt.block_on(async {
            let Ok(store) = crate::rag::vector_store::VectorStore::new(dimensions).await else {
                return Vec::new();
            };
            let Ok(embedder) = crate::rag::embedding_client::client_from_config(&config) else {
                return Vec::new();
            };
            let Ok(query_vec) = embedder.embed(resolved_query) else {
                return Vec::new();
            };
            let Ok(results) = store.search_similar(&query_vec, 5).await else {
                return Vec::new();
            };
            results
                .iter()
                .map(|chunk| Self::format_rag_chunk(resolved_query, chunk))
                .collect()
        })
    }

    fn resolve_rag_resources(
        &self,
        db: &Database,
        default_project_hash: Option<&str>,
    ) -> Vec<String> {
        self.section_lines("rag_search")
            .into_iter()
            .flat_map(|query| Self::search_rag_resources(db, &query, default_project_hash))
            .collect()
    }

    fn append_prompt_section(
        &self,
        result: &mut String,
        prefix: &str,
        header: &str,
        outer_tag: &str,
        item_tag: &str,
    ) {
        build_xml_block(
            result,
            &self.enabled_sections,
            |section_id| Self::section_matches_prefix(section_id, prefix),
            |section_id| self.section_content_for_build(section_id),
            header,
            outer_tag,
            item_tag,
        );
    }

    fn append_instruction_section(&self, result: &mut String) {
        result.push_str("# [INSTRUCTIONS]: Execution Logic\n");
        result.push_str("<instruction_set>\n");
        for content in self.section_entries("instruction") {
            push_xml_item(result, "instruction", content);
        }
        result.push_str("</instruction_set>\n\n");
    }

    fn append_tools_section(&self, result: &mut String) {
        let tool_lines = self.collect_tool_lines();
        if tool_lines.is_empty() {
            return;
        }

        result.push_str("# [TOOLS]: Skills & Capabilities\n");
        result.push_str("<tools>\n");
        for tool_line in tool_lines {
            append_tool_skill(result, &tool_line);
        }
        result.push_str("</tools>\n\n");
    }

    fn collect_tool_lines(&self) -> Vec<String> {
        self.section_entries("tools")
            .into_iter()
            .flat_map(|content| {
                content
                    .lines()
                    .map(str::trim)
                    .filter(|line| !line.is_empty())
                    .map(str::to_string)
                    .collect::<Vec<_>>()
            })
            .collect()
    }

    pub fn build_prompt_with_resolved_resources(
        &self,
        db: &Database,
        current_workdir: &Path,
    ) -> Result<String> {
        let system = self.render_system_block();
        let mut body = self.build_body();
        let project_contexts = self.resolve_project_contexts(db);
        let default_project_hash = self.default_project_hash(db, current_workdir);
        let mut resources = self.resolve_resource_entries(db);
        resources.extend(self.resolve_rag_resources(db, default_project_hash.as_deref()));

        // PROJECT CONTEXT leads the body, right below the system block.
        if let Some(section) = format_indexed_xml_section(
            "# [PROJECT CONTEXT]: Registered Project Metadata\n",
            "project_context",
            "project",
            &project_contexts,
        ) {
            body = format!("{section}{body}");
        }

        // Resolved resources replace the raw resources section and close the body.
        if let Some(section) = format_indexed_xml_section(
            "# [RESOURCES]: Knowledge Base & Data\n",
            "resources",
            "resource",
            &resources,
        ) {
            body = strip_resources_section(&body);
            body.push_str(&section);
        }

        // System block stays first so its protocol is read before the task.
        Ok(format!("{system}{body}"))
    }

    /// The invisible system block, rendered so its operating protocol is the
    /// first thing the agent reads. Empty when there is no system content.
    fn render_system_block(&self) -> String {
        match &self.system_content {
            Some(system) => format!("<system>\n{system}\n</system>\n\n"),
            None => String::new(),
        }
    }

    /// Build the prompt body (every section except the system block).
    fn build_body(&self) -> String {
        let mut result = String::new();
        self.append_prompt_section(
            &mut result,
            "goal",
            "# [GOAL]: Desired Outcome\n",
            "goal",
            "goal_item",
        );
        self.append_prompt_section(
            &mut result,
            "context",
            "# [CONTEXT]: Project Background\n",
            "context",
            "context",
        );
        self.append_instruction_section(&mut result);
        self.append_prompt_section(
            &mut result,
            "resources",
            "# [RESOURCES]: Knowledge Base & Data\n",
            "resources",
            "resource",
        );
        self.append_prompt_section(
            &mut result,
            "constraints",
            "# [CONSTRAINTS]: Behavioral Boundaries\n",
            "constraints",
            "constraint",
        );
        self.append_tools_section(&mut result);
        result
    }

    pub fn migrate_legacy_sections(&mut self, current_project_path: Option<&str>) {
        let had_memory_context = self
            .enabled_sections
            .iter()
            .any(|section_id| Self::section_matches_prefix(section_id, "memory_context"));
        let obsolete_sections = self
            .enabled_sections
            .iter()
            .filter(|section_id| {
                Self::section_matches_prefix(section_id, "memory_context")
                    || Self::section_matches_prefix(section_id, "examples")
            })
            .cloned()
            .collect::<Vec<_>>();

        for section_id in obsolete_sections {
            self.remove_section(&section_id);
        }

        let has_project_context = self
            .enabled_sections
            .iter()
            .any(|section_id| Self::section_matches_prefix(section_id, "project_context"));
        if had_memory_context && !has_project_context {
            if let Some(project_path) = current_project_path {
                self.add_section_with_content("project_context", project_path.to_string());
            }
        }
        self.focused_section = self
            .focused_section
            .min(self.enabled_sections.len().saturating_sub(1));
    }

    fn set_content_and_cursor(
        &mut self,
        section_id: &str,
        content: String,
        cursor: usize,
        field_width: usize,
    ) {
        self.set_section_content(section_id, content);
        self.section_cursors.insert(section_id.to_string(), cursor);
        self.update_section_scroll(section_id, field_width);
    }

    fn split_content_at_cursor(&self, section_id: &str) -> (String, String, usize) {
        let content = self.get_section_content(section_id);
        let chars: Vec<char> = content.chars().collect();
        let cursor = self.cursor(section_id).min(chars.len());
        let before = chars[..cursor].iter().collect();
        let after = chars[cursor..].iter().collect();
        (before, after, cursor)
    }

    fn replace_char_range(
        &mut self,
        section_id: &str,
        start: usize,
        end: usize,
        replacement: &str,
        field_width: usize,
    ) {
        let content = self.get_section_content(section_id);
        let chars: Vec<char> = content.chars().collect();
        let start = start.min(chars.len());
        let end = end.min(chars.len());

        let mut new_content = String::new();
        new_content.extend(chars[..start].iter().copied());
        new_content.push_str(replacement);
        new_content.extend(chars[end..].iter().copied());
        self.set_content_and_cursor(
            section_id,
            new_content,
            start + replacement.chars().count(),
            field_width,
        );
    }

    fn resources_section_id(&self) -> Option<String> {
        self.enabled_sections
            .iter()
            .find(|section_id| Self::section_matches_prefix(section_id, "resources"))
            .cloned()
    }

    fn add_resource_reference(&mut self, full_path: &str) {
        let Some(section_id) = self.resources_section_id() else {
            self.add_section_with_content("resources", full_path.to_string());
            return;
        };

        let content = self.get_section_content(&section_id);
        let updated = if content.is_empty() {
            full_path.to_string()
        } else {
            format!("{content}\n{full_path}")
        };
        self.set_section_content(&section_id, updated);
    }

    /// Replace the `@`-trigger with `@rel_path` in the section text and add the full path
    /// to the resources section (creating one if needed).
    /// Skills are treated as normal file resources — no special content injection.
    pub fn insert_at_completion(
        &mut self,
        section_id: &str,
        rel_path: &str,
        full_path: &str,
        field_width: usize,
    ) {
        let Some(trigger_pos) = self.at_picker.as_ref().map(|picker| picker.trigger_pos) else {
            return;
        };

        // The `@` is at trigger_pos; cursor is currently at trigger_pos + 1
        // (we never insert query chars into the text, only into picker.query).
        // A leftover manual `@` can sit right before it (e.g. the user typed
        // `@`, dismissed the picker with Esc, then typed `@` again) — absorb
        // that one too so the result is `@path`, not `@@path`.
        let chars: Vec<char> = self.get_section_content(section_id).chars().collect();
        let start = if trigger_pos > 0 && chars.get(trigger_pos - 1) == Some(&'@') {
            trigger_pos - 1
        } else {
            trigger_pos
        };
        self.replace_char_range(
            section_id,
            start,
            trigger_pos + 1,
            &format!("@{rel_path}"),
            field_width,
        );
        self.add_resource_reference(full_path);
        // NOTE: focused_section is intentionally NOT restored here.
        // The caller (event handler) owns that responsibility and restores it
        // explicitly after this function returns.
    }

    fn next_file_reference(text: &str, current_pos: usize) -> Option<(usize, &str, usize)> {
        let at_pos = text[current_pos..].find('@')?;
        let absolute_pos = current_pos + at_pos;
        let remaining = &text[absolute_pos..];
        let ref_end = remaining
            .find(|c: char| c.is_whitespace() || c == ',' || c == '!' || c == '?' || c == '│')
            .unwrap_or(remaining.len());
        Some((absolute_pos, &remaining[..ref_end], absolute_pos + ref_end))
    }

    fn is_file_reference(file_ref: &str) -> bool {
        file_ref.len() > 1
            && file_ref[1..]
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-' || c == '.' || c == '/')
    }

    /// Colorize `@word` tokens in rendered section text with a custom accent color.
    pub fn get_file_reference_with_styling(
        &self,
        text: &str,
        accent: Color,
    ) -> Vec<(String, Option<Color>)> {
        let mut result = Vec::new();
        let mut current_pos = 0;

        while let Some((absolute_pos, file_ref, next_pos)) =
            Self::next_file_reference(text, current_pos)
        {
            if absolute_pos > current_pos {
                result.push((text[current_pos..absolute_pos].to_string(), None));
            }

            let color = Self::is_file_reference(file_ref).then_some(accent);
            result.push((file_ref.to_string(), color));
            current_pos = next_pos;
        }

        if current_pos < text.len() {
            result.push((text[current_pos..].to_string(), None));
        }
        result
    }

    /// Count visual (wrapped) lines for a text given a field width
    pub fn visual_line_count(text: &str, field_width: usize) -> usize {
        if field_width == 0 {
            return 1;
        }

        // Keep wrapping math aligned with the rendered paragraph, including tabs
        // and hard line breaks, so box height grows when visible text does.
        let mut lines = 1usize;
        let mut col = 0usize;
        for ch in text.chars() {
            match ch {
                '\n' => {
                    lines += 1;
                    col = 0;
                }
                '\t' => {
                    let tab = 4 - (col % 4);
                    if col + tab > field_width {
                        lines += 1;
                        col = tab;
                    } else {
                        col += tab;
                    }
                }
                _ => {
                    if col + 1 > field_width {
                        lines += 1;
                        col = 1;
                    } else {
                        col += 1;
                    }
                }
            }
        }

        lines.max(1)
    }

    /// Visual lines occupied by the first `char_idx` chars of text.
    fn visual_lines_to_cursor(text: &str, char_idx: usize, field_width: usize) -> usize {
        let prefix: String = text.chars().take(char_idx).collect();
        Self::visual_line_count(&prefix, field_width).max(1)
    }

    /// Max visible lines for a section type (instruction=5, others=3)
    pub fn max_visible_lines(section_id: &str) -> usize {
        if Self::section_matches_prefix(section_id, "instruction") {
            5
        } else {
            3
        }
    }

    /// Update scroll for a section so the cursor stays visible.
    pub fn update_section_scroll(&mut self, section_id: &str, field_width: usize) {
        let max_vis = Self::max_visible_lines(section_id);
        let text = self.section_content_for_build(section_id).unwrap_or("");
        let cur = self.cursor(section_id);
        let cursor_visual_line =
            Self::visual_lines_to_cursor(text, cur, field_width).saturating_sub(1);

        let scroll = self
            .section_scrolls
            .entry(section_id.to_string())
            .or_insert(0);
        if cursor_visual_line < *scroll {
            *scroll = cursor_visual_line;
        } else if cursor_visual_line >= *scroll + max_vis {
            *scroll = cursor_visual_line + 1 - max_vis;
        }
    }

    /// Move cursor left one char in the given section.
    pub fn move_cursor_left(&mut self, section_id: &str, field_width: usize) {
        let cur = self.cursor(section_id);
        if cur > 0 {
            self.section_cursors.insert(section_id.to_string(), cur - 1);
            self.update_section_scroll(section_id, field_width);
        }
    }

    /// Move cursor right one char in the given section.
    pub fn move_cursor_right(&mut self, section_id: &str, field_width: usize) {
        let len = self
            .sections
            .get(section_id)
            .map(|s| s.chars().count())
            .unwrap_or(0);
        let cur = self.cursor(section_id);
        if cur < len {
            self.section_cursors.insert(section_id.to_string(), cur + 1);
            self.update_section_scroll(section_id, field_width);
        }
    }

    /// (visual line, column) for every cursor position 0..=len, using the same
    /// wrapping math as `visual_line_count` so movement matches what is drawn.
    fn visual_positions(text: &str, field_width: usize) -> Vec<(usize, usize)> {
        let field_width = field_width.max(1);
        let mut positions = Vec::with_capacity(text.chars().count() + 1);
        let mut line = 0usize;
        let mut col = 0usize;
        positions.push((line, col));
        for ch in text.chars() {
            match ch {
                '\n' => {
                    line += 1;
                    col = 0;
                }
                '\t' => {
                    let tab = 4 - (col % 4);
                    if col + tab > field_width {
                        line += 1;
                        col = tab;
                    } else {
                        col += tab;
                    }
                }
                _ => {
                    if col + 1 > field_width {
                        line += 1;
                        col = 1;
                    } else {
                        col += 1;
                    }
                }
            }
            positions.push((line, col));
        }
        positions
    }

    /// Move the cursor one visual line vertically, keeping the column as close
    /// as possible. `delta` is -1 (up) or +1 (down).
    fn move_cursor_vertical(&mut self, section_id: &str, field_width: usize, delta: isize) {
        let text = self.get_section_content(section_id);
        let positions = Self::visual_positions(&text, field_width);
        let cur = self.cursor(section_id).min(positions.len() - 1);
        let (cur_line, cur_col) = positions[cur];

        let target = if delta < 0 {
            match cur_line.checked_sub(1) {
                Some(line) => line,
                None => {
                    // Already on the first visual line: jump to start.
                    self.section_cursors.insert(section_id.to_string(), 0);
                    self.update_section_scroll(section_id, field_width);
                    return;
                }
            }
        } else {
            let last_line = positions.last().map(|&(line, _)| line).unwrap_or(0);
            if cur_line >= last_line {
                // Already on the last visual line: jump to end.
                self.section_cursors
                    .insert(section_id.to_string(), positions.len() - 1);
                self.update_section_scroll(section_id, field_width);
                return;
            }
            cur_line + 1
        };

        // Best index on the target line: largest column that doesn't pass cur_col,
        // falling back to the line's last position.
        let mut best = None;
        for (idx, &(line, col)) in positions.iter().enumerate() {
            if line != target {
                continue;
            }
            if col <= cur_col || best.is_none() {
                best = Some(idx);
            }
        }
        if let Some(idx) = best {
            self.section_cursors.insert(section_id.to_string(), idx);
            self.update_section_scroll(section_id, field_width);
        }
    }

    /// Move cursor up one visual line in the given section.
    pub fn move_cursor_up(&mut self, section_id: &str, field_width: usize) {
        self.move_cursor_vertical(section_id, field_width, -1);
    }

    /// Move cursor down one visual line in the given section.
    pub fn move_cursor_down(&mut self, section_id: &str, field_width: usize) {
        self.move_cursor_vertical(section_id, field_width, 1);
    }

    /// Insert a character at cursor position in any section.
    /// Content is stored exactly as typed; soft wrapping happens at render time.
    pub fn insert_char_at_cursor(&mut self, section_id: &str, ch: char, field_width: usize) {
        let content = self.get_section_content(section_id);
        let cur = self.cursor(section_id).min(content.chars().count());

        let mut new_chars: Vec<char> = content.chars().collect();
        new_chars.insert(cur, ch);
        let new_content: String = new_chars.into_iter().collect();
        self.set_content_and_cursor(section_id, new_content, cur + 1, field_width);
    }

    /// Delete the character before cursor in any section.
    pub fn backspace_at_cursor(&mut self, section_id: &str, field_width: usize) {
        let content = self.get_section_content(section_id);
        let chars: Vec<char> = content.chars().collect();
        let cur = self.cursor(section_id);
        if cur > 0 && cur <= chars.len() {
            let mut new_chars = chars;
            new_chars.remove(cur - 1);
            let new_content: String = new_chars.into_iter().collect();
            self.set_content_and_cursor(section_id, new_content, cur - 1, field_width);
        }
    }

    /// Insert a newline at cursor position in any section.
    pub fn insert_newline_at_cursor(&mut self, section_id: &str, field_width: usize) {
        let (before, after, cursor) = self.split_content_at_cursor(section_id);
        self.set_content_and_cursor(
            section_id,
            format!("{before}\n{after}"),
            cursor + 1,
            field_width,
        );
    }

    /// Insert text at cursor position in any section.
    pub fn insert_text_at_cursor(&mut self, section_id: &str, text: &str, field_width: usize) {
        let (before, after, cursor) = self.split_content_at_cursor(section_id);
        self.set_content_and_cursor(
            section_id,
            format!("{before}{text}{after}"),
            cursor + text.chars().count(),
            field_width,
        );
    }

    /// Whether the `send_at` field is currently focused (virtual section at index 0).
    #[allow(dead_code)]
    pub fn is_send_at_focused(&self) -> bool {
        self.focused_section == 0 && !self.enabled_sections.is_empty()
    }

    /// Total focusable items: send_at (1) + enabled_sections.
    pub fn total_focusable(&self) -> usize {
        1 + self.enabled_sections.len()
    }

    /// Focus the first enabled section, ready to type. The send control lives
    /// at focus index 0 and is the LAST stop of the navigation cycle — never
    /// the first — so every open path (fresh, reopen, restored session) starts
    /// here (focus index 1 = first section).
    pub fn focus_first_section(&mut self) {
        self.focused_section = usize::from(!self.enabled_sections.is_empty());
    }

    /// Move focus to the next field in VISUAL order (U11): sections top to
    /// bottom, then the send control at the bottom, then wrap to the first
    /// section. The send control keeps focus index 0 internally.
    pub fn focus_next(&mut self) {
        let sections = self.enabled_sections.len();
        self.focused_section = match self.focused_section {
            0 if sections > 0 => 1,
            0 => 0,
            i if i >= sections => 0,
            i => i + 1,
        };
    }

    /// Move focus to the previous field in VISUAL order (see
    /// [`Self::focus_next`]): from the send control up to the last section,
    /// from the first section wrap down to the send control.
    pub fn focus_prev(&mut self) {
        let sections = self.enabled_sections.len();
        self.focused_section = match self.focused_section {
            0 => sections,
            1 => 0,
            i => i - 1,
        };
    }

    /// Map the focus index to an `enabled_sections` index.
    /// `None` when send_at (focus 0) is selected.
    pub fn focused_section_index(&self) -> Option<usize> {
        if self.focused_section == 0 {
            None
        } else {
            Some(self.focused_section - 1)
        }
    }

    /// Resolve the currently focused section name, if any (not send_at).
    pub fn focused_section_name(&self) -> Option<&str> {
        self.focused_section_index()
            .and_then(|idx| self.enabled_sections.get(idx))
            .map(String::as_str)
    }

    /// Toggle the send selector between `now` and `date` (lateral arrows,
    /// U11). Leaving `date` discards any picked time and open picker.
    pub fn send_toggle(&mut self) {
        self.send_error = None;
        self.send_choice = match self.send_choice {
            SendChoice::Now => SendChoice::Date,
            SendChoice::Date => {
                self.send_at = None;
                self.send_edit = None;
                SendChoice::Now
            }
        };
    }

    /// Open the inline date-time picker (Enter on `send: date`), preseeded
    /// with the already-picked time or the current local time (U11).
    pub fn send_begin_edit(&mut self) {
        self.send_error = None;
        let seed = self.send_at.unwrap_or_else(|| {
            let now = chrono::Local::now().naive_local();
            now.with_second(0)
                .and_then(|t| t.with_nanosecond(0))
                .unwrap_or(now)
        });
        self.send_edit = Some(SendAtEdit {
            value: seed,
            field: 0,
            typed: 0,
            typed_len: 0,
        });
    }

    /// Move the picker's focused field (0=year … 4=minute). Changing field
    /// clears any half-typed number so the next digit starts fresh.
    pub fn send_edit_move(&mut self, delta: isize) {
        if let Some(edit) = self.send_edit.as_mut() {
            let next = edit.field as isize + delta;
            edit.field = next.clamp(0, 4) as usize;
            edit.typed = 0;
            edit.typed_len = 0;
        }
    }

    /// Adjust the picker's focused field by `delta` with real calendar math
    /// (months/days carry correctly). Arrows and typing coexist: an arrow
    /// clears the digit accumulator so a following digit starts a new number.
    pub fn send_edit_adjust(&mut self, delta: i64) {
        let Some(edit) = self.send_edit.as_mut() else {
            return;
        };
        self.send_error = None;
        edit.typed = 0;
        edit.typed_len = 0;
        let value = edit.value;
        let adjusted = match edit.field {
            0 => add_months(value, delta * 12),
            1 => add_months(value, delta),
            2 => Some(value + chrono::Duration::days(delta)),
            3 => Some(value + chrono::Duration::hours(delta)),
            _ => Some(value + chrono::Duration::minutes(delta)),
        };
        if let Some(adjusted) = adjusted {
            edit.value = adjusted;
        }
    }

    /// Type a digit into the picker's focused field (U11). Digits accumulate
    /// within the field (year 4 wide, others 2); when the field fills it
    /// auto-advances to the next field, and a digit typed into an already-full
    /// field restarts that field. The resulting component is clamped into its
    /// valid range on the fly (e.g. month `13` → `12`), mirroring the clamping
    /// the arrow-based `send_edit_adjust` already performs.
    pub fn send_edit_type_digit(&mut self, digit: u32) {
        self.send_error = None;
        let Some(edit) = self.send_edit.as_mut() else {
            return;
        };
        let field = edit.field;
        let width = field_digit_width(field);
        // A digit landing on an already-full field starts the number over.
        if edit.typed_len >= width {
            edit.typed = 0;
            edit.typed_len = 0;
        }
        edit.typed = edit.typed * 10 + digit;
        edit.typed_len += 1;
        if let Some(updated) = with_field(edit.value, field, edit.typed) {
            edit.value = updated;
        }
        // Field full → auto-advance to the next field (clamped at minute).
        if edit.typed_len >= width {
            edit.field = (field + 1).min(4);
            edit.typed = 0;
            edit.typed_len = 0;
        }
    }

    /// Confirm the picker (Enter): a future time is stored and displayed
    /// inline; a past time is rejected with an inline hint (U11).
    pub fn send_edit_confirm(&mut self) -> bool {
        let Some(edit) = self.send_edit else {
            return false;
        };
        if edit.value <= chrono::Local::now().naive_local() {
            self.send_error = Some("picked time is in the past".to_string());
            return false;
        }
        self.send_at = Some(edit.value);
        self.send_choice = SendChoice::Date;
        self.send_edit = None;
        self.send_error = None;
        true
    }

    /// Cancel the picker (Esc): back to `now` unless a time was already
    /// confirmed earlier.
    pub fn send_edit_cancel(&mut self) {
        self.send_edit = None;
        self.send_error = None;
        if self.send_at.is_none() {
            self.send_choice = SendChoice::Now;
        }
    }

    /// Clear the schedule entirely (Backspace): send immediately.
    pub fn clear_send_at(&mut self) {
        self.send_choice = SendChoice::Now;
        self.send_at = None;
        self.send_edit = None;
        self.send_error = None;
    }

    /// The send control's inline value text (U11).
    pub fn send_display(&self) -> String {
        match (self.send_choice, self.send_at) {
            (SendChoice::Now, _) => "now".to_string(),
            (SendChoice::Date, Some(at)) => at.format("%Y-%m-%d %H:%M").to_string(),
            (SendChoice::Date, None) => "date".to_string(),
        }
    }

    // ── Tabs & Raw mode ─────────────────────────────────────────────────

    /// The Raw tab's free-text buffer (empty string when never touched).
    pub fn raw_text(&self) -> &str {
        self.sections
            .get(RAW_SECTION_ID)
            .map(String::as_str)
            .unwrap_or("")
    }

    /// True when the Raw buffer is blank (whitespace-only counts as empty).
    /// The empty state is what triggers the composed-prompt preview.
    pub fn raw_is_empty(&self) -> bool {
        self.raw_text().trim().is_empty()
    }

    /// Switch to a specific tab. Focus lands on the tab's first editable field
    /// (Raw → the raw buffer; Normal → the first section), never on the send
    /// control. No-op transient state (`raw_preview`) is cleared so the caller
    /// can recompute it when needed.
    pub fn set_tab(&mut self, tab: PromptTab) {
        if self.active_tab == tab {
            return;
        }
        self.active_tab = tab;
        self.raw_preview = None;
        self.raw_edit_scroll = None;
        match tab {
            PromptTab::Raw => {
                // Raw has two focus targets: the buffer (index 1) and the send
                // control (index 0). Land on the buffer, ready to type.
                self.focused_section = 1;
                let end = self.raw_text().chars().count();
                self.section_cursors
                    .entry(RAW_SECTION_ID.to_string())
                    .or_insert(end);
            }
            PromptTab::Normal => self.focus_first_section(),
        }
    }

    /// Toggle between the Normal and Raw tabs.
    pub fn toggle_tab(&mut self) {
        let next = match self.active_tab {
            PromptTab::Normal => PromptTab::Raw,
            PromptTab::Raw => PromptTab::Normal,
        };
        self.set_tab(next);
    }

    /// The exact string a "send" produces given the active tab:
    /// Raw + non-empty buffer → the raw text verbatim (no XML/system wrapping);
    /// otherwise the composed Normal-form prompt (identical to sending from the
    /// Normal tab, and to what the Raw-empty preview shows).
    pub fn resolve_outgoing_prompt(&self, db: &Database, current_workdir: &Path) -> Result<String> {
        if self.active_tab == PromptTab::Raw && !self.raw_is_empty() {
            return Ok(self.raw_text().to_string());
        }
        self.build_prompt_with_resolved_resources(db, current_workdir)
    }

    /// Compute the composed-prompt preview string (best-effort) and cache it in
    /// `raw_preview`. Called when entering the Raw tab; the Normal form can't
    /// change while Raw is shown, so the cache stays faithful.
    pub fn refresh_raw_preview(&mut self, db: &Database, current_workdir: &Path) {
        self.raw_preview = self
            .build_prompt_with_resolved_resources(db, current_workdir)
            .ok();
        self.raw_preview_scroll = 0;
    }

    /// Scroll the read-only raw preview by `delta` lines (negative = up),
    /// clamped to the preview's line count so it can always be scrolled back.
    pub fn scroll_raw_preview(&mut self, delta: isize) {
        let max = self
            .raw_preview
            .as_deref()
            .map(|preview| preview.lines().count().saturating_sub(1))
            .unwrap_or(0);
        let next = self.raw_preview_scroll.saturating_add_signed(delta);
        self.raw_preview_scroll = next.min(max);
    }

    /// Wheel-scroll the editable raw buffer by `delta` lines (negative = up).
    /// The scroll is clamped to `[0, max]` where `max = total_lines - avail_h`,
    /// and stored separately from the cursor so the cursor is never moved by
    /// a wheel event (view-only, satisfying FR 4).
    pub fn scroll_raw_edit(&mut self, delta: isize, total_lines: usize, avail_h: usize) {
        let max_scroll = total_lines.saturating_sub(avail_h);
        let current = self.raw_edit_scroll.unwrap_or(0);
        let next = current.saturating_add_signed(delta);
        self.raw_edit_scroll = Some(next.min(max_scroll));
    }

    /// Hit-boxes for the two tab labels, laid out left-to-right from `(x, y)`.
    /// Pure geometry so the render and the mouse handler agree, and so the
    /// click→tab mapping is unit-testable without any event plumbing.
    pub fn tab_hitboxes(x: u16, y: u16) -> [(PromptTab, Rect); 2] {
        let normal_w = TAB_NORMAL_LABEL.chars().count() as u16;
        let raw_w = TAB_RAW_LABEL.chars().count() as u16;
        [
            (PromptTab::Normal, Rect::new(x, y, normal_w, 1)),
            (PromptTab::Raw, Rect::new(x + normal_w, y, raw_w, 1)),
        ]
    }

    /// Map a click at `(col, row)` to the tab whose hit-box contains it, given
    /// the tab bar origin `(x, y)`. Pure — the unit test for mouse switching.
    pub fn tab_at(x: u16, y: u16, col: u16, row: u16) -> Option<PromptTab> {
        Self::tab_hitboxes(x, y)
            .into_iter()
            .find_map(|(tab, rect)| {
                (row == rect.y && col >= rect.x && col < rect.x + rect.width).then_some(tab)
            })
    }

    /// Pure geometry for the Raw tab's scrollable content region, mirroring
    /// `draw_raw_tab_content`'s `content_area`. The `inner` rect is the
    /// dialog's inner area (border excluded); `list_panel_height` is the
    /// scheduled-sends panel rows (0 when no pending sends). Used by the
    /// mouse-wheel hit test to decide whether a scroll event targets the
    /// Raw content.
    pub fn raw_content_rect(
        inner: ratatui::layout::Rect,
        list_panel_height: u16,
    ) -> ratatui::layout::Rect {
        let content_top = inner.y + 3;
        let content_bottom = inner.y + inner.height.saturating_sub(2 + list_panel_height);
        let avail_h = content_bottom.saturating_sub(content_top).max(1);
        ratatui::layout::Rect {
            x: inner.x + 1,
            y: content_top,
            width: inner.width.saturating_sub(2),
            height: avail_h,
        }
    }

    // ── Scheduled-sends list panel (B33) ────────────────────────────────

    /// Load a queued scheduled send into the Raw tab for in-place editing.
    /// A scheduled send stores a single already-resolved string, which the
    /// Normal field-stack cannot un-compose — so the Raw free-text field is its
    /// natural home. Switches to the Raw tab, loads the prompt (cursor at end),
    /// preseeds the send control with the entry's fire time, and records the id
    /// so re-confirming a schedule replaces that row instead of duplicating it.
    pub fn load_scheduled_for_edit(
        &mut self,
        id: &str,
        prompt: &str,
        fire_local: chrono::NaiveDateTime,
    ) {
        self.active_tab = PromptTab::Raw;
        self.raw_preview = None;
        self.raw_preview_scroll = 0;
        self.sections
            .insert(RAW_SECTION_ID.to_string(), prompt.to_string());
        let end = prompt.chars().count();
        self.section_cursors.insert(RAW_SECTION_ID.to_string(), end);
        // Raw's first focus target is the buffer (index 1), never the send control.
        self.focused_section = 1;
        self.editing_scheduled_id = Some(id.to_string());
        self.send_choice = SendChoice::Date;
        self.send_at = Some(fire_local);
        self.send_edit = None;
        self.send_error = None;
        // Editing takes over from browsing: release the list focus.
        self.scheduled_list_selected = None;
    }

    /// Pure geometry for the scheduled-sends list panel: given the total number
    /// of pending entries, the panel's max visible rows, and the current
    /// selection, return `(visible_rows, scroll_offset)` so the selected row
    /// stays in view. Kept pure (like `tab_hitboxes`) so render and tests agree.
    pub fn scheduled_list_view(
        total: usize,
        max_rows: usize,
        selected: Option<usize>,
    ) -> (usize, usize) {
        if total == 0 || max_rows == 0 {
            return (0, 0);
        }
        let visible = total.min(max_rows);
        let scroll = match selected {
            Some(sel) => {
                let sel = sel.min(total - 1);
                if sel < visible {
                    0
                } else {
                    (sel + 1 - visible).min(total - visible)
                }
            }
            None => 0,
        };
        (visible, scroll)
    }

    fn should_collapse_paste(text: &str) -> bool {
        text.lines().count() > 1 || text.chars().count() > 200
    }

    /// Insert pasted text. If it spans multiple lines, collapse it to a
    /// `[Pasted ~N lines]` placeholder while keeping the real text for `build_prompt`.
    pub fn insert_collapsed_paste_at_cursor(
        &mut self,
        section_id: &str,
        text: &str,
        field_width: usize,
    ) {
        if !Self::should_collapse_paste(text) {
            self.insert_text_at_cursor(section_id, text, field_width);
            return;
        }

        self.expand_collapsed_paste(section_id);
        let (before, after, cursor) = self.split_content_at_cursor(section_id);
        let placeholder = format!("[Pasted ~{} lines]", text.lines().count().max(1));
        self.collapsed_pastes
            .insert(section_id.to_string(), format!("{before}{text}{after}"));
        self.set_content_and_cursor(
            section_id,
            format!("{before}{placeholder}{after}"),
            cursor + placeholder.chars().count(),
            field_width,
        );
    }

    /// Expand a collapsed paste for the given section, restoring real content.
    pub fn expand_collapsed_paste(&mut self, section_id: &str) {
        if let Some(real) = self.collapsed_pastes.remove(section_id) {
            self.set_section_content(section_id, real);
        }
    }

    /// Check if section has a collapsed paste.
    pub fn has_collapsed_paste(&self, section_id: &str) -> bool {
        self.collapsed_pastes.contains_key(section_id)
    }

    /// Check if cursor is positioned inside a collapsed placeholder text.
    /// Returns true if the section has a collapsed paste and cursor is within the placeholder.
    pub fn cursor_in_collapsed_placeholder(&self, section_id: &str) -> bool {
        if !self.has_collapsed_paste(section_id) {
            return false;
        }

        let content = self.get_section_content(section_id);
        let cursor_pos = self.cursor(section_id);

        // Find the collapsed placeholder pattern: "[Pasted ~N lines]"
        if let Some(start_byte) = content.find("[Pasted ~") {
            if let Some(end_byte) = content[start_byte..].find(']') {
                let start_char = content[..start_byte].chars().count();
                let end_char = start_char
                    + content[start_byte..start_byte + end_byte + 1]
                        .chars()
                        .count();
                return cursor_pos > start_char && cursor_pos <= end_char;
            }
        }
        false
    }

    /// Delete the entire collapsed paste block and restore cursor position.
    /// Called when backspace is pressed while cursor is inside the placeholder.
    pub fn backspace_collapsed_paste(&mut self, section_id: &str, field_width: usize) {
        if !self.has_collapsed_paste(section_id) {
            return;
        }

        let content = self.get_section_content(section_id);

        // Find and remove the collapsed placeholder
        if let Some(start_byte) = content.find("[Pasted ~") {
            if let Some(end_byte) = content[start_byte..].find(']') {
                let placeholder_end_byte = start_byte + end_byte + 1;
                let start_char = content[..start_byte].chars().count();
                let mut new_content = String::with_capacity(content.len());
                new_content.push_str(&content[..start_byte]);
                new_content.push_str(&content[placeholder_end_byte..]);

                self.collapsed_pastes.remove(section_id);
                self.set_content_and_cursor(section_id, new_content, start_char, field_width);
            }
        }
    }
}

fn append_tool_skill(result: &mut String, tool_line: &str) {
    result.push_str("  <skill>\n");
    result.push_str(&format!("    {tool_line}\n"));
    result.push_str("  </skill>\n\n");
}

fn strip_resources_section(prompt: &str) -> String {
    let header = "# [RESOURCES]: Knowledge Base & Data\n<resources>\n";
    let Some(start) = prompt.find(header) else {
        return prompt.to_string();
    };
    let Some(end_rel) = prompt[start..].find("</resources>\n\n") else {
        return prompt.to_string();
    };

    let end = start + end_rel + "</resources>\n\n".len();
    let mut stripped = String::with_capacity(prompt.len().saturating_sub(end - start));
    stripped.push_str(&prompt[..start]);
    stripped.push_str(&prompt[end..]);
    stripped
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Datelike;
    use tempfile::tempdir;

    #[test]
    fn preset_is_discoverable_via_the_add_section_list_like_tools() {
        let dialog = SimplePromptDialog::new();
        let names: Vec<&str> = dialog
            .get_addable_sections()
            .into_iter()
            .map(|(name, _)| name)
            .collect();
        assert!(names.contains(&"preset"));
        assert!(names.contains(&"tools"));
    }

    #[test]
    fn collect_presets_for_picker_lists_md_files_sorted_with_preview() {
        let dir = tempdir().unwrap();
        std::fs::write(
            dir.path().join("reviewer.md"),
            "You are the reviewer.\n\nMore text.",
        )
        .unwrap();
        std::fs::write(
            dir.path().join("implementer.md"),
            "You are the implementer.",
        )
        .unwrap();
        std::fs::write(dir.path().join("notes.txt"), "ignored, not .md").unwrap();

        let entries = SimplePromptDialog::collect_presets_for_picker(dir.path());

        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].0, "implementer");
        assert_eq!(entries[0].1, "You are the implementer.");
        assert_eq!(entries[0].2, "You are the implementer.");
        assert_eq!(entries[1].0, "reviewer");
        assert_eq!(entries[1].1, "You are the reviewer.");
    }

    #[test]
    fn collect_presets_for_picker_returns_empty_for_missing_directory() {
        let dir = tempdir().unwrap();
        let missing = dir.path().join("does-not-exist");

        assert!(SimplePromptDialog::collect_presets_for_picker(&missing).is_empty());
    }

    #[test]
    fn filtered_preset_indices_matches_case_insensitive_substring() {
        let entries = vec![
            (
                "implementer".to_string(),
                "preview".to_string(),
                "content".to_string(),
            ),
            (
                "reviewer".to_string(),
                "preview".to_string(),
                "content".to_string(),
            ),
            (
                "resilience".to_string(),
                "preview".to_string(),
                "content".to_string(),
            ),
        ];

        let filtered = SimplePromptDialog::filtered_preset_indices(&entries, "RE");
        let names: Vec<&str> = filtered.iter().map(|&i| entries[i].0.as_str()).collect();
        assert_eq!(names, vec!["reviewer", "resilience"]);

        assert_eq!(
            SimplePromptDialog::filtered_preset_indices(&entries, "").len(),
            3
        );
        assert!(SimplePromptDialog::filtered_preset_indices(&entries, "zzz").is_empty());
    }

    #[test]
    fn build_prompt_resolves_project_context_and_file_resources() {
        let temp = tempdir().unwrap();
        let db_path = temp.path().join("canopy.db");
        let db = Database::new(&db_path).unwrap();

        let project_dir = temp.path().join("sample-project");
        std::fs::create_dir(&project_dir).unwrap();
        let project = db.register_project_path(&project_dir).unwrap();

        let resource = project_dir.join("guide.txt");
        std::fs::write(&resource, "hello from resource").unwrap();

        let mut dialog = SimplePromptDialog::new();
        dialog.set_section_content("instruction", "do the thing".to_string());
        dialog.add_section_with_content("project_context", project.path.clone());
        dialog.add_section_with_content("resources", resource.display().to_string());

        let prompt = dialog
            .build_prompt_with_resolved_resources(&db, &project_dir)
            .unwrap();

        assert!(prompt.contains("# [PROJECT CONTEXT]: Registered Project Metadata"));
        assert!(prompt.contains(&project.hash));
        assert!(prompt.contains("kind: file"));
        assert!(prompt.contains("guide.txt"));
    }

    #[test]
    fn build_prompt_resolves_project_directory_in_resources() {
        let temp = tempdir().unwrap();
        let db_path = temp.path().join("canopy.db");
        let db = Database::new(&db_path).unwrap();

        let project_dir = temp.path().join("dir-project");
        std::fs::create_dir(&project_dir).unwrap();
        let project = db.register_project_path(&project_dir).unwrap();

        let mut dialog = SimplePromptDialog::new();
        dialog.set_section_content("instruction", "summarize".to_string());
        dialog.add_section_with_content("resources", project.path.clone());

        let prompt = dialog
            .build_prompt_with_resolved_resources(&db, &project_dir)
            .unwrap();

        assert!(prompt.contains("workdir_hash:"));
        assert!(prompt.contains(&project.hash));
    }

    #[test]
    fn build_prompt_auto_injects_project_context_for_resource_descendant() {
        let temp = tempdir().unwrap();
        let db_path = temp.path().join("canopy.db");
        let db = Database::new(&db_path).unwrap();

        let project_dir = temp.path().join("linked-project");
        std::fs::create_dir(&project_dir).unwrap();
        let project = db.register_project_path(&project_dir).unwrap();

        let nested_dir = project_dir.join("src").join("module");
        std::fs::create_dir_all(&nested_dir).unwrap();
        let file_path = nested_dir.join("lib.rs");
        std::fs::write(&file_path, "pub fn demo() {}").unwrap();

        let mut dialog = SimplePromptDialog::new();
        dialog.set_section_content("instruction_1", "summarize".to_string());
        dialog.add_section_with_content("resources", file_path.display().to_string());

        let prompt = dialog
            .build_prompt_with_resolved_resources(&db, temp.path())
            .unwrap();

        assert!(prompt.contains("# [PROJECT CONTEXT]: Registered Project Metadata"));
        assert!(prompt.contains(&project.hash));
        assert!(prompt.contains("kind: file"));
        assert!(prompt.contains("lib.rs"));
    }

    #[test]
    fn system_block_leads_the_prompt() {
        let temp = tempdir().unwrap();
        let db_path = temp.path().join("canopy.db");
        let db = Database::new(&db_path).unwrap();
        let project_dir = temp.path().join("proj");
        std::fs::create_dir(&project_dir).unwrap();

        let mut dialog = SimplePromptDialog::new();
        dialog.set_section_content("instruction_1", "do it".to_string());
        dialog.system_content = Some("[START HERE — required] call get_tools".to_string());

        let prompt = dialog
            .build_prompt_with_resolved_resources(&db, &project_dir)
            .unwrap();

        assert!(prompt.starts_with("<system>\n"));
        let system_end = prompt.find("</system>").expect("system block present");
        let instructions = prompt
            .find("# [INSTRUCTIONS]")
            .expect("instructions present");
        // The whole system block must come before the task instructions.
        assert!(system_end < instructions);
    }

    #[test]
    fn migrate_legacy_sections_replaces_memory_with_project_context() {
        let mut dialog = SimplePromptDialog::new();
        dialog.add_section_with_content("memory_context", "legacy".to_string());
        dialog.add_section_with_content("examples", "old".to_string());

        dialog.migrate_legacy_sections(Some("/tmp/project"));

        assert!(dialog
            .enabled_sections
            .iter()
            .all(|section_id| !section_id.starts_with("memory_context_")
                && !section_id.starts_with("examples_")));
        assert!(dialog
            .enabled_sections
            .iter()
            .any(|section_id| section_id.starts_with("project_context_")));
    }

    #[test]
    fn move_cursor_vertical_respects_hard_newlines() {
        let mut dialog = SimplePromptDialog::new();
        dialog.set_section_content("instruction_1", "abc\ndefgh\nij".to_string());
        // Cursor on "defgh" line, column 4 (after 'g': indices a=0..c=2,\n=3,d=4..h=8)
        dialog
            .section_cursors
            .insert("instruction_1".to_string(), 8);

        dialog.move_cursor_up("instruction_1", 40);
        // First line only has 3 columns; cursor clamps to its end (index 3, col 3).
        assert_eq!(dialog.cursor("instruction_1"), 3);

        dialog.move_cursor_down("instruction_1", 40);
        // Back down to "defgh" at column 3 → index 7.
        assert_eq!(dialog.cursor("instruction_1"), 7);

        dialog.move_cursor_down("instruction_1", 40);
        // "ij" line, column 2 max → index 12 (end of text).
        assert_eq!(dialog.cursor("instruction_1"), 12);

        // Down on the last line jumps to end; up from the first line jumps to 0.
        dialog.move_cursor_down("instruction_1", 40);
        assert_eq!(dialog.cursor("instruction_1"), 12);
        dialog
            .section_cursors
            .insert("instruction_1".to_string(), 1);
        dialog.move_cursor_up("instruction_1", 40);
        assert_eq!(dialog.cursor("instruction_1"), 0);
    }

    #[test]
    fn move_cursor_vertical_handles_soft_wrap() {
        let mut dialog = SimplePromptDialog::new();
        // width 5: "aaaaa" | "bbbbb" as two visual lines, no '\n' present
        dialog.set_section_content("instruction_1", "aaaaabbbbb".to_string());
        dialog
            .section_cursors
            .insert("instruction_1".to_string(), 8);

        dialog.move_cursor_up("instruction_1", 5);
        // Same column (3) on the first visual line → index 3.
        assert_eq!(dialog.cursor("instruction_1"), 3);

        dialog.move_cursor_down("instruction_1", 5);
        assert_eq!(dialog.cursor("instruction_1"), 8);
    }

    #[test]
    fn test_non_ascii_handling_does_not_panic() {
        // Typing past the field width must never mutate the stored content:
        // soft wrapping is a render-time concern only.
        let mut dialog = SimplePromptDialog::new();
        dialog.set_section_content("instruction_1", "áéíóú ".to_string());
        dialog
            .section_cursors
            .insert("instruction_1".to_string(), 6);
        for ch in "world".chars() {
            dialog.insert_char_at_cursor("instruction_1", ch, 10);
        }
        assert_eq!(dialog.get_section_content("instruction_1"), "áéíóú world");
        assert_eq!(dialog.cursor("instruction_1"), 11);
    }

    #[test]
    fn test_collapsed_placeholder_non_ascii_indices() {
        let mut dialog = SimplePromptDialog::new();
        dialog.set_section_content("instruction_1", "áéíóú [Pasted ~5 lines] extra".to_string());
        dialog.collapsed_pastes.insert(
            "instruction_1".to_string(),
            "original content\nwith multiple lines".to_string(),
        );

        // Character indices:
        // "áéíóú " is 6 characters.
        // "[Pasted ~5 lines]" is 18 characters.
        // "start" char of placeholder is 6.
        // "end" char of placeholder is 6 + 18 = 24.

        // Let's test cursor_in_collapsed_placeholder
        // Cursor at 5 (on the space) -> false
        dialog
            .section_cursors
            .insert("instruction_1".to_string(), 5);
        assert!(!dialog.cursor_in_collapsed_placeholder("instruction_1"));

        // Cursor at 7 (inside placeholder) -> true
        dialog
            .section_cursors
            .insert("instruction_1".to_string(), 7);
        assert!(dialog.cursor_in_collapsed_placeholder("instruction_1"));

        // Cursor at 23 (on ']') -> true
        dialog
            .section_cursors
            .insert("instruction_1".to_string(), 23);
        assert!(dialog.cursor_in_collapsed_placeholder("instruction_1"));

        // Cursor at 24 (after placeholder) -> false
        dialog
            .section_cursors
            .insert("instruction_1".to_string(), 24);
        assert!(!dialog.cursor_in_collapsed_placeholder("instruction_1"));

        // Test backspace_collapsed_paste with cursor inside placeholder
        dialog
            .section_cursors
            .insert("instruction_1".to_string(), 15);
        dialog.backspace_collapsed_paste("instruction_1", 80);
        // It should delete the placeholder "[Pasted ~5 lines]" and leave "áéíóú  extra"
        assert_eq!(dialog.get_section_content("instruction_1"), "áéíóú  extra");
        // Cursor should be at 6 (the start of deleted placeholder)
        assert_eq!(dialog.cursor("instruction_1"), 6);
    }

    #[test]
    fn insert_collapsed_paste_at_cursor_collapses_multiline_paste() {
        let mut dialog = SimplePromptDialog::new();
        let pasted = "line one\nline two\nline three";

        dialog.insert_collapsed_paste_at_cursor("instruction_1", pasted, 80);

        let displayed = dialog.get_section_content("instruction_1");
        assert!(displayed.contains("[Pasted ~3 lines]"));
        assert!(!displayed.contains("line one"));
        // The full pasted text (all 3 lines) is still what gets sent to the CLI.
        assert_eq!(
            dialog.section_content_for_build("instruction_1"),
            Some(pasted)
        );
    }

    #[test]
    fn insert_at_completion_removes_leftover_manual_at_before_trigger() {
        let temp = tempdir().unwrap();
        let workdir = temp.path().to_path_buf();

        let mut dialog = SimplePromptDialog::new();
        // Simulate: user typed "@" (left over from a dismissed picker), then
        // typed "@" again right after it — trigger_pos points at the second "@".
        dialog.set_section_content("instruction_1", "look @@".to_string());
        dialog
            .section_cursors
            .insert("instruction_1".to_string(), 7);
        dialog.at_picker = Some(AtPicker::new(workdir, 6));

        dialog.insert_at_completion("instruction_1", "src/lib.rs", "/abs/src/lib.rs", 80);

        assert_eq!(
            dialog.get_section_content("instruction_1"),
            "look @src/lib.rs"
        );
    }

    #[test]
    fn is_empty_true_for_fresh_dialog_and_false_once_filled() {
        let mut dialog = SimplePromptDialog::new();
        assert!(dialog.is_empty());

        dialog.set_section_content("instruction_1", "do the thing".to_string());
        assert!(!dialog.is_empty());
    }

    #[test]
    fn is_empty_true_when_sections_are_only_whitespace() {
        let mut dialog = SimplePromptDialog::new();
        dialog.set_section_content("instruction_1", "   \n  ".to_string());
        dialog.add_section_with_content("context", "  ".to_string());
        assert!(dialog.is_empty());
    }

    #[test]
    fn load_flat_text_replaces_all_content_with_a_single_instruction() {
        let mut dialog = SimplePromptDialog::new();
        dialog.set_section_content("instruction_1", "stale draft".to_string());
        dialog.add_section_with_content("context", "stale context".to_string());
        dialog.lock_section("context_1");

        dialog.load_flat_text("recovered prompt text");

        assert_eq!(dialog.enabled_sections, vec!["instruction_1".to_string()]);
        assert_eq!(
            dialog.get_section_content("instruction_1"),
            "recovered prompt text"
        );
        assert!(dialog.locked_sections.is_empty());
        assert_eq!(
            dialog.cursor("instruction_1"),
            "recovered prompt text".chars().count()
        );
    }

    #[test]
    fn persisted_builder_state_round_trips_through_json() {
        let mut dialog = SimplePromptDialog::new();
        dialog.set_section_content("instruction_1", "ship the feature".to_string());
        dialog.add_section_with_content("tools", "skill:code-engineering".to_string());
        dialog.send_choice = SendChoice::Date;
        dialog.send_at = Some(
            chrono::NaiveDate::from_ymd_opt(2026, 7, 20)
                .unwrap()
                .and_hms_opt(14, 30, 0)
                .unwrap(),
        );

        let snapshot = PersistedBuilderState::from_dialog(&dialog);
        let json = serde_json::to_string(&snapshot).expect("serialize");
        let restored: PersistedBuilderState = serde_json::from_str(&json).expect("deserialize");

        let mut target = SimplePromptDialog::new();
        restored.restore_into(&mut target);

        assert_eq!(
            target.get_section_content("instruction_1"),
            "ship the feature"
        );
        assert_eq!(
            target.get_section_content("tools_1"),
            "skill:code-engineering"
        );
        // send_at is intentionally not part of the snapshot — recall must not
        // resurrect a stale schedule.
        assert!(target.send_at.is_none());
    }

    #[test]
    fn add_section_focuses_the_new_section_with_cursor_at_start() {
        let mut dialog = SimplePromptDialog::new();
        dialog.add_section("goal");

        // Focus must land IN the newly created section, ready to type.
        let focused = dialog
            .focused_section_name()
            .expect("a section is focused, not the send control")
            .to_string();
        assert!(
            focused.starts_with("goal"),
            "expected focus on the new goal section, got {focused}"
        );
        // Cursor sits at position 0 of the empty new section.
        assert_eq!(dialog.cursor(&focused), 0);

        // A second added section also grabs focus (not the previous one).
        dialog.add_section("constraints");
        let focused = dialog.focused_section_name().unwrap().to_string();
        assert!(focused.starts_with("constraints"));
        assert_eq!(dialog.cursor(&focused), 0);
    }

    #[test]
    fn new_dialog_focuses_the_first_section() {
        let dialog = SimplePromptDialog::new();
        assert_eq!(dialog.focused_section, 1);
        assert_eq!(dialog.focused_section_name(), Some("instruction_1"));
    }

    #[test]
    fn restored_session_focuses_first_section_regardless_of_persisted_focus() {
        let mut source = SimplePromptDialog::new();
        source.add_section("goal");
        // Pretend the user left focus parked on the send control (index 0).
        source.focused_section = 0;
        let session = PromptBuilderSession::from_dialog(&source);

        let mut target = SimplePromptDialog::new();
        session.restore_into(&mut target);
        assert_eq!(target.focused_section, 1);
        assert!(target.focused_section_name().is_some());
    }

    #[test]
    fn recalled_persisted_state_focuses_first_section_regardless_of_persisted_focus() {
        let mut source = SimplePromptDialog::new();
        source.focused_section = 0; // persisted on the send control
        let snapshot = PersistedBuilderState::from_dialog(&source);

        let mut target = SimplePromptDialog::new();
        target.focused_section = 0;
        snapshot.restore_into(&mut target);
        assert_eq!(target.focused_section, 1);
        assert_eq!(target.focused_section_name(), Some("instruction_1"));
    }

    // ── Tabs & Raw mode ─────────────────────────────────────────────────

    #[test]
    fn toggle_tab_flips_active_tab_and_focus() {
        let mut dialog = SimplePromptDialog::new();
        assert_eq!(dialog.active_tab, PromptTab::Normal);
        assert_eq!(dialog.focused_section, 1);

        dialog.toggle_tab();
        assert_eq!(dialog.active_tab, PromptTab::Raw);
        // Raw lands focus on the raw buffer (index 1), not the send control.
        assert_eq!(dialog.focused_section, 1);

        dialog.toggle_tab();
        assert_eq!(dialog.active_tab, PromptTab::Normal);
        assert_eq!(dialog.focused_section, 1);
    }

    #[test]
    fn tab_at_maps_clicks_to_the_right_tab() {
        // Tab bar origin at (2, 1). " Normal " spans cols 2..9, " Raw " 10..14.
        let x = 2;
        let y = 1;
        assert_eq!(
            SimplePromptDialog::tab_at(x, y, 3, 1),
            Some(PromptTab::Normal)
        );
        assert_eq!(
            SimplePromptDialog::tab_at(x, y, 9, 1),
            Some(PromptTab::Normal)
        );
        assert_eq!(
            SimplePromptDialog::tab_at(x, y, 10, 1),
            Some(PromptTab::Raw)
        );
        assert_eq!(
            SimplePromptDialog::tab_at(x, y, 13, 1),
            Some(PromptTab::Raw)
        );
        // Left of the bar, right of the bar, and a different row all miss.
        assert_eq!(SimplePromptDialog::tab_at(x, y, 1, 1), None);
        assert_eq!(SimplePromptDialog::tab_at(x, y, 20, 1), None);
        assert_eq!(SimplePromptDialog::tab_at(x, y, 3, 2), None);
    }

    #[test]
    fn raw_non_empty_send_produces_exactly_the_raw_text() {
        let temp = tempdir().unwrap();
        let db = Database::new(&temp.path().join("canopy.db")).unwrap();
        let workdir = temp.path().to_path_buf();

        let mut dialog = SimplePromptDialog::new();
        // Non-raw content that WOULD be composed if we were on the Normal tab —
        // proves the raw path bypasses XML/system composition entirely.
        dialog.set_section_content("instruction_1", "compose me".to_string());
        dialog.system_content = Some("SYSTEM PROTOCOL".to_string());
        dialog.set_tab(PromptTab::Raw);
        dialog
            .sections
            .insert(RAW_SECTION_ID.to_string(), "/compact".to_string());

        let out = dialog.resolve_outgoing_prompt(&db, &workdir).unwrap();
        assert_eq!(out, "/compact");
        assert!(!out.contains("SYSTEM"));
        assert!(!out.contains("[INSTRUCTIONS]"));
    }

    #[test]
    fn raw_empty_preview_and_send_equal_the_composed_prompt() {
        let temp = tempdir().unwrap();
        let db = Database::new(&temp.path().join("canopy.db")).unwrap();
        let workdir = temp.path().to_path_buf();

        let mut dialog = SimplePromptDialog::new();
        dialog.set_section_content("instruction_1", "do the thing".to_string());
        let composed = dialog
            .build_prompt_with_resolved_resources(&db, &workdir)
            .unwrap();

        dialog.set_tab(PromptTab::Raw);
        assert!(dialog.raw_is_empty());

        // Sending from the empty Raw tab sends the composed prompt verbatim.
        let out = dialog.resolve_outgoing_prompt(&db, &workdir).unwrap();
        assert_eq!(out, composed);

        // The cached preview shows exactly that same composed string.
        dialog.refresh_raw_preview(&db, &workdir);
        assert_eq!(dialog.raw_preview.as_deref(), Some(composed.as_str()));
    }

    // ── Scheduled-sends list panel (B33) ────────────────────────────────

    #[test]
    fn scheduled_list_view_no_scroll_when_all_rows_fit() {
        // 3 entries, panel shows up to 4 → everything visible, never scrolled.
        assert_eq!(
            SimplePromptDialog::scheduled_list_view(3, 4, Some(2)),
            (3, 0)
        );
        assert_eq!(SimplePromptDialog::scheduled_list_view(3, 4, None), (3, 0));
    }

    #[test]
    fn scheduled_list_view_scrolls_to_keep_selection_in_view() {
        // 6 entries, 3 visible rows. Selecting row 4 scrolls so it is the last
        // visible row (offset 2 → rows 2,3,4).
        assert_eq!(
            SimplePromptDialog::scheduled_list_view(6, 3, Some(4)),
            (3, 2)
        );
        // The last row never scrolls past the end (offset clamps to total-visible).
        assert_eq!(
            SimplePromptDialog::scheduled_list_view(6, 3, Some(5)),
            (3, 3)
        );
        // Early rows keep the panel pinned to the top.
        assert_eq!(
            SimplePromptDialog::scheduled_list_view(6, 3, Some(1)),
            (3, 0)
        );
    }

    #[test]
    fn scheduled_list_view_empty_is_zero() {
        assert_eq!(SimplePromptDialog::scheduled_list_view(0, 4, None), (0, 0));
        assert_eq!(
            SimplePromptDialog::scheduled_list_view(5, 0, Some(1)),
            (0, 0)
        );
    }

    #[test]
    fn load_scheduled_for_edit_loads_raw_and_records_editing_id() {
        let mut dialog = SimplePromptDialog::new();
        // Some pre-existing Normal-form content that must be bypassed.
        dialog.set_section_content("instruction_1", "compose me".to_string());
        dialog.scheduled_list_selected = Some(2);

        let fire = chrono::NaiveDate::from_ymd_opt(2030, 1, 2)
            .unwrap()
            .and_hms_opt(9, 15, 0)
            .unwrap();
        dialog.load_scheduled_for_edit("ss-42", "deliver this later", fire);

        assert_eq!(dialog.active_tab, PromptTab::Raw);
        assert_eq!(dialog.raw_text(), "deliver this later");
        assert_eq!(
            dialog.cursor(RAW_SECTION_ID),
            "deliver this later".chars().count()
        );
        assert_eq!(dialog.editing_scheduled_id.as_deref(), Some("ss-42"));
        assert_eq!(dialog.send_choice, SendChoice::Date);
        assert_eq!(dialog.send_at, Some(fire));
        // Focus lands on the raw buffer, and the list focus is released.
        assert_eq!(dialog.focused_section, 1);
        assert!(dialog.scheduled_list_selected.is_none());
    }

    #[test]
    fn raw_buffer_and_active_tab_survive_session_round_trip() {
        let mut source = SimplePromptDialog::new();
        source.set_tab(PromptTab::Raw);
        source.sections.insert(
            RAW_SECTION_ID.to_string(),
            "/compact when quota back".to_string(),
        );

        let session = PromptBuilderSession::from_dialog(&source);
        let mut target = SimplePromptDialog::new();
        session.restore_into(&mut target);

        assert_eq!(target.active_tab, PromptTab::Raw);
        assert_eq!(target.raw_text(), "/compact when quota back");
        // Restoring the Raw tab lands focus on the buffer, not the send control.
        assert_eq!(target.focused_section, 1);
    }

    // ── Raw-tab mouse-wheel scrolling (U13) ────────────────────────────────

    #[test]
    fn raw_content_rect_matches_draw_geometry() {
        // A typical inner rect (border excluded). The content area should
        // start 3 rows below inner.y (tab bar + hint + gap) and end 2 rows
        // above inner.bottom (send line + gap), minus the list panel.
        let inner = ratatui::layout::Rect::new(5, 3, 60, 30);
        let rect = SimplePromptDialog::raw_content_rect(inner, 0);
        assert_eq!(rect.x, 6); // inner.x + 1
        assert_eq!(rect.y, 6); // inner.y + 3
        assert_eq!(rect.width, 58); // inner.width - 2
                                    // height = inner.height - 2 (send+gap) - 3 (top) = 30 - 5 = 25
        assert_eq!(rect.height, 25);
    }

    #[test]
    fn raw_content_rect_with_list_panel() {
        let inner = ratatui::layout::Rect::new(0, 0, 60, 20);
        let rect = SimplePromptDialog::raw_content_rect(inner, 3);
        // bottom = 20 - (2 + 3) = 15; top = 3; height = 15 - 3 = 12
        assert_eq!(rect.height, 12);
    }

    #[test]
    fn raw_content_rect_small_dialog() {
        // Minimum-size dialog: inner height 6 (borders + tab + hint + gap + content + gap + send).
        let inner = ratatui::layout::Rect::new(0, 0, 40, 6);
        let rect = SimplePromptDialog::raw_content_rect(inner, 0);
        // bottom = 6 - 2 = 4; top = 3; height = max(4-3, 1) = 1
        assert_eq!(rect.height, 1);
        assert!(rect.width > 0);
    }

    #[test]
    fn scroll_raw_edit_clamps_at_zero() {
        let mut dialog = SimplePromptDialog::new();
        assert!(dialog.raw_edit_scroll.is_none());
        dialog.scroll_raw_edit(-5, 100, 20);
        // Clamped to 0 (cannot scroll above the first line).
        assert_eq!(dialog.raw_edit_scroll, Some(0));
    }

    #[test]
    fn scroll_raw_edit_clamps_at_max() {
        let mut dialog = SimplePromptDialog::new();
        // total_lines = 50, avail_h = 10 → max_scroll = 40
        dialog.scroll_raw_edit(100, 50, 10);
        assert_eq!(dialog.raw_edit_scroll, Some(40));
    }

    #[test]
    fn scroll_raw_edit_accumulates() {
        let mut dialog = SimplePromptDialog::new();
        dialog.scroll_raw_edit(3, 100, 20);
        assert_eq!(dialog.raw_edit_scroll, Some(3));
        dialog.scroll_raw_edit(2, 100, 20);
        assert_eq!(dialog.raw_edit_scroll, Some(5));
        dialog.scroll_raw_edit(-1, 100, 20);
        assert_eq!(dialog.raw_edit_scroll, Some(4));
    }

    #[test]
    fn raw_edit_scroll_cleared_on_tab_switch() {
        let mut dialog = SimplePromptDialog::new();
        dialog.set_tab(PromptTab::Raw);
        dialog.raw_edit_scroll = Some(5);
        dialog.set_tab(PromptTab::Normal);
        assert!(dialog.raw_edit_scroll.is_none());
        dialog.set_tab(PromptTab::Raw);
        assert!(dialog.raw_edit_scroll.is_none());
    }

    #[test]
    fn raw_edit_scroll_initially_none() {
        let dialog = SimplePromptDialog::new();
        assert!(dialog.raw_edit_scroll.is_none());
    }

    // ── field_digit_width ────────────────────────────────────────

    #[test]
    fn field_digit_width_year_is_4() {
        assert_eq!(field_digit_width(0), 4);
    }

    #[test]
    fn field_digit_width_month_is_2() {
        assert_eq!(field_digit_width(1), 2);
    }

    #[test]
    fn field_digit_width_day_is_2() {
        assert_eq!(field_digit_width(2), 2);
    }

    #[test]
    fn field_digit_width_hour_is_2() {
        assert_eq!(field_digit_width(3), 2);
    }

    #[test]
    fn field_digit_width_minute_is_2() {
        assert_eq!(field_digit_width(4), 2);
    }

    // ── days_in_month ────────────────────────────────────────────

    #[test]
    fn days_in_month_january() {
        assert_eq!(days_in_month(2024, 1), 31);
    }

    #[test]
    fn days_in_month_february_leap_year() {
        assert_eq!(days_in_month(2024, 2), 29);
    }

    #[test]
    fn days_in_month_february_non_leap() {
        assert_eq!(days_in_month(2023, 2), 28);
    }

    #[test]
    fn days_in_month_april() {
        assert_eq!(days_in_month(2024, 4), 30);
    }

    #[test]
    fn days_in_month_december() {
        assert_eq!(days_in_month(2024, 12), 31);
    }

    // ── add_months ───────────────────────────────────────────────

    #[test]
    fn add_months_forward() {
        let dt = chrono::NaiveDate::from_ymd_opt(2024, 1, 15)
            .unwrap()
            .and_hms_opt(10, 30, 0)
            .unwrap();
        let result = add_months(dt, 3).unwrap();
        assert_eq!(result.month(), 4);
        assert_eq!(result.year(), 2024);
    }

    #[test]
    fn add_months_backward() {
        let dt = chrono::NaiveDate::from_ymd_opt(2024, 3, 15)
            .unwrap()
            .and_hms_opt(10, 30, 0)
            .unwrap();
        let result = add_months(dt, -1).unwrap();
        assert_eq!(result.month(), 2);
    }

    #[test]
    fn add_months_year_rollover() {
        let dt = chrono::NaiveDate::from_ymd_opt(2024, 11, 15)
            .unwrap()
            .and_hms_opt(10, 30, 0)
            .unwrap();
        let result = add_months(dt, 2).unwrap();
        assert_eq!(result.month(), 1);
        assert_eq!(result.year(), 2025);
    }

    #[test]
    fn add_months_day_clamping() {
        // Jan 31 + 1 month = Feb 29 (2024 is leap)
        let dt = chrono::NaiveDate::from_ymd_opt(2024, 1, 31)
            .unwrap()
            .and_hms_opt(12, 0, 0)
            .unwrap();
        let result = add_months(dt, 1).unwrap();
        assert_eq!(result.month(), 2);
        assert_eq!(result.day(), 29);
    }

    // ── with_field ───────────────────────────────────────────────

    #[test]
    fn with_field_year() {
        let dt = chrono::NaiveDate::from_ymd_opt(2024, 6, 15)
            .unwrap()
            .and_hms_opt(10, 30, 0)
            .unwrap();
        let result = with_field(dt, 0, 2030).unwrap();
        assert_eq!(result.year(), 2030);
        assert_eq!(result.month(), 6);
        assert_eq!(result.day(), 15);
    }

    #[test]
    fn with_field_month_clamps_day() {
        let dt = chrono::NaiveDate::from_ymd_opt(2024, 1, 31)
            .unwrap()
            .and_hms_opt(10, 30, 0)
            .unwrap();
        let result = with_field(dt, 1, 2).unwrap();
        assert_eq!(result.month(), 2);
        assert_eq!(result.day(), 29); // Clamped to Feb 29 (leap year)
    }

    #[test]
    fn with_field_day_clamped_to_month_length() {
        let dt = chrono::NaiveDate::from_ymd_opt(2024, 2, 15)
            .unwrap()
            .and_hms_opt(10, 30, 0)
            .unwrap();
        let result = with_field(dt, 2, 31).unwrap();
        assert_eq!(result.day(), 29); // Feb 2024 has 29 days
    }

    #[test]
    fn with_field_hour_clamped() {
        let dt = chrono::NaiveDate::from_ymd_opt(2024, 6, 15)
            .unwrap()
            .and_hms_opt(10, 30, 0)
            .unwrap();
        let result = with_field(dt, 3, 25).unwrap();
        assert_eq!(result.hour(), 23);
    }

    #[test]
    fn with_field_minute_clamped() {
        let dt = chrono::NaiveDate::from_ymd_opt(2024, 6, 15)
            .unwrap()
            .and_hms_opt(10, 30, 0)
            .unwrap();
        let result = with_field(dt, 4, 65).unwrap();
        assert_eq!(result.minute(), 59);
    }

    // ── strip_resources_section ──────────────────────────────────

    #[test]
    fn strip_resources_section_removes_present_section() {
        let input = "before\n# [RESOURCES]: Knowledge Base & Data\n<resources>\n  <resource>\n    path: /foo\n  </resource>\n</resources>\n\nafter";
        let result = strip_resources_section(input);
        assert!(result.starts_with("before"));
        assert!(result.ends_with("after"));
        assert!(!result.contains("RESOURCES"));
    }

    #[test]
    fn strip_resources_section_no_section_unchanged() {
        let input = "no resources section here";
        assert_eq!(strip_resources_section(input), input);
    }

    #[test]
    fn strip_resources_section_missing_closing_tag() {
        let input = "# [RESOURCES]: Knowledge Base & Data\n<resources>\n  content\n";
        assert_eq!(strip_resources_section(input), input);
    }

    // ── format_indexed_xml_section ────────────────────────────────

    #[test]
    fn format_indexed_xml_section_empty_items() {
        assert!(format_indexed_xml_section("header", "outer", "item", &[]).is_none());
    }

    #[test]
    fn format_indexed_xml_section_with_items() {
        let items = vec!["item1".to_string(), "item2".to_string()];
        let result = format_indexed_xml_section("# Header\n", "outer", "item", &items).unwrap();
        assert!(result.contains("# Header"));
        assert!(result.contains("<outer>"));
        assert!(result.contains("</outer>"));
        assert!(result.contains("<item>"));
        assert!(result.contains("item1"));
        assert!(result.contains("item2"));
    }

    // ── push_xml_item ────────────────────────────────────────────

    #[test]
    fn push_xml_item_basic() {
        let mut result = String::new();
        push_xml_item(&mut result, "tag", "content here");
        assert!(result.contains("<tag>"));
        assert!(result.contains("content here"));
        assert!(result.contains("</tag>"));
    }

    #[test]
    fn push_xml_item_multiline() {
        let mut result = String::new();
        push_xml_item(&mut result, "tag", "line1\nline2");
        assert!(result.contains("line1"));
        assert!(result.contains("line2"));
    }

    // ── append_tool_skill ────────────────────────────────────────

    #[test]
    fn append_tool_skill_basic() {
        let mut result = String::new();
        append_tool_skill(&mut result, "skill:code-engineering");
        assert!(result.contains("<skill>"));
        assert!(result.contains("skill:code-engineering"));
        assert!(result.contains("</skill>"));
    }

    // ── visual_line_count edge cases ─────────────────────────────

    #[test]
    fn visual_line_count_empty_string() {
        assert_eq!(SimplePromptDialog::visual_line_count("", 40), 1);
    }

    #[test]
    fn visual_line_count_single_line() {
        assert_eq!(SimplePromptDialog::visual_line_count("hello", 40), 1);
    }

    #[test]
    fn visual_line_count_zero_width() {
        assert_eq!(SimplePromptDialog::visual_line_count("hello", 0), 1);
    }

    #[test]
    fn visual_line_count_newlines() {
        assert_eq!(SimplePromptDialog::visual_line_count("a\nb\nc", 40), 3);
    }

    #[test]
    fn visual_line_count_soft_wrap() {
        // 10 chars in a field_width of 5 → 2 visual lines
        assert_eq!(SimplePromptDialog::visual_line_count("abcdefghij", 5), 2);
    }

    #[test]
    fn visual_line_count_tabs() {
        // Tab at col 0 takes 4 cols, 'x' at col 4 within width 5 → 1 line total
        assert_eq!(SimplePromptDialog::visual_line_count("\tx", 5), 1);
    }

    #[test]
    fn visual_line_count_tab_no_wrap() {
        // Tab at col 0 takes 4 cols, then "ab" (2 chars) → col 6, within width 10
        assert_eq!(SimplePromptDialog::visual_line_count("\tab", 10), 1);
    }

    // ── max_visible_lines ────────────────────────────────────────

    #[test]
    fn max_visible_lines_instruction_section() {
        assert_eq!(SimplePromptDialog::max_visible_lines("instruction_1"), 5);
    }

    #[test]
    fn max_visible_lines_other_section() {
        assert_eq!(SimplePromptDialog::max_visible_lines("context_1"), 3);
        assert_eq!(SimplePromptDialog::max_visible_lines("goal_1"), 3);
        assert_eq!(SimplePromptDialog::max_visible_lines("tools_1"), 3);
    }

    // ── section_type ─────────────────────────────────────────────

    #[test]
    fn section_type_instruction() {
        assert_eq!(SimplePromptDialog::section_type("instruction_1"), "instruction");
        assert_eq!(SimplePromptDialog::section_type("instruction"), "instruction");
    }

    #[test]
    fn section_type_context() {
        assert_eq!(SimplePromptDialog::section_type("context_2"), "context");
    }

    #[test]
    fn section_type_unknown() {
        assert_eq!(SimplePromptDialog::section_type("custom_thing"), "custom_thing");
    }

    // ── section_matches_prefix ───────────────────────────────────

    #[test]
    fn section_matches_prefix_exact() {
        assert!(SimplePromptDialog::section_matches_prefix("instruction", "instruction"));
    }

    #[test]
    fn section_matches_prefix_with_suffix() {
        assert!(SimplePromptDialog::section_matches_prefix("instruction_1", "instruction"));
    }

    #[test]
    fn section_matches_prefix_no_match() {
        assert!(!SimplePromptDialog::section_matches_prefix("context_1", "instruction"));
    }

    // ── is_tools_section ─────────────────────────────────────────

    #[test]
    fn is_tools_section_exact() {
        assert!(SimplePromptDialog::is_tools_section("tools"));
    }

    #[test]
    fn is_tools_section_with_suffix() {
        assert!(SimplePromptDialog::is_tools_section("tools_1"));
    }

    #[test]
    fn is_tools_section_no_match() {
        assert!(!SimplePromptDialog::is_tools_section("context"));
        assert!(!SimplePromptDialog::is_tools_section("tool"));
    }

    // ── is_file_reference ────────────────────────────────────────

    #[test]
    fn is_file_reference_valid() {
        assert!(SimplePromptDialog::is_file_reference("@src/lib.rs"));
        assert!(SimplePromptDialog::is_file_reference("@file.txt"));
        assert!(SimplePromptDialog::is_file_reference("@a"));
    }

    #[test]
    fn is_file_reference_single_at() {
        assert!(!SimplePromptDialog::is_file_reference("@"));
    }

    #[test]
    fn is_file_reference_special_chars() {
        assert!(!SimplePromptDialog::is_file_reference("@file with spaces"));
        assert!(SimplePromptDialog::is_file_reference("@file_name-1.0.rs"));
    }

    // ── resolve_rag_scope ────────────────────────────────────────

    #[test]
    fn resolve_rag_scope_global_prefix() {
        let (scope, query) = SimplePromptDialog::resolve_rag_scope("global:my query", None);
        assert!(matches!(scope, RagScope::Global));
        assert_eq!(query, "my query");
    }

    #[test]
    fn resolve_rag_scope_project_prefix() {
        let (scope, query) =
            SimplePromptDialog::resolve_rag_scope("project:abc123:my query", None);
        assert!(matches!(scope, RagScope::Project("abc123")));
        assert_eq!(query, "my query");
    }

    #[test]
    fn resolve_rag_scope_no_prefix_with_default() {
        let (scope, query) = SimplePromptDialog::resolve_rag_scope("my query", Some("hash1"));
        assert!(matches!(scope, RagScope::Project("hash1")));
        assert_eq!(query, "my query");
    }

    #[test]
    fn resolve_rag_scope_no_prefix_no_default() {
        let (scope, query) = SimplePromptDialog::resolve_rag_scope("my query", None);
        assert!(matches!(scope, RagScope::Global));
        assert_eq!(query, "my query");
    }

    // ── next_file_reference ──────────────────────────────────────

    #[test]
    fn next_file_reference_finds_at() {
        let text = "look at @src/lib.rs for details";
        let result = SimplePromptDialog::next_file_reference(text, 0);
        assert!(result.is_some());
        let (pos, _ref, next) = result.unwrap();
        assert_eq!(pos, 8); // "look at @" — @ is at index 8
        assert_eq!(_ref, "@src/lib.rs");
        assert!(next > pos);
    }

    #[test]
    fn next_file_reference_no_at() {
        let text = "no references here";
        assert!(SimplePromptDialog::next_file_reference(text, 0).is_none());
    }

    #[test]
    fn next_file_reference_with_offset() {
        let text = "first @one then @two";
        let (pos1, ref1, next1) = SimplePromptDialog::next_file_reference(text, 0).unwrap();
        assert_eq!(pos1, 6);
        assert_eq!(ref1, "@one");
        let (pos2, ref2, _) = SimplePromptDialog::next_file_reference(text, next1).unwrap();
        assert_eq!(pos2, 16);
        assert_eq!(ref2, "@two");
    }

    // ── focus navigation ─────────────────────────────────────────

    #[test]
    fn focus_next_wraps_from_last_to_send() {
        let mut dialog = SimplePromptDialog::new();
        dialog.add_section("goal");
        let last = dialog.enabled_sections.len();
        dialog.focused_section = last; // last section
        dialog.focus_next();
        assert_eq!(dialog.focused_section, 0); // send control
    }

    #[test]
    fn focus_next_wraps_from_send_to_first() {
        let mut dialog = SimplePromptDialog::new();
        dialog.focused_section = 0;
        dialog.focus_next();
        assert_eq!(dialog.focused_section, 1);
    }

    #[test]
    fn focus_prev_from_send_goes_to_last() {
        let mut dialog = SimplePromptDialog::new();
        dialog.add_section("goal");
        dialog.focused_section = 0;
        dialog.focus_prev();
        assert_eq!(dialog.focused_section, dialog.enabled_sections.len());
    }

    #[test]
    fn focus_prev_from_first_goes_to_send() {
        let mut dialog = SimplePromptDialog::new();
        dialog.focused_section = 1;
        dialog.focus_prev();
        assert_eq!(dialog.focused_section, 0);
    }

    #[test]
    fn focused_section_index_send_returns_none() {
        let mut dialog = SimplePromptDialog::new();
        dialog.focused_section = 0;
        assert!(dialog.focused_section_index().is_none());
    }

    #[test]
    fn focused_section_index_section_returns_some() {
        let dialog = SimplePromptDialog::new();
        assert_eq!(dialog.focused_section_index(), Some(0));
    }

    // ── send display ─────────────────────────────────────────────

    #[test]
    fn send_display_now() {
        let dialog = SimplePromptDialog::new();
        assert_eq!(dialog.send_display(), "now");
    }

    #[test]
    fn send_display_date_no_time() {
        let mut dialog = SimplePromptDialog::new();
        dialog.send_choice = SendChoice::Date;
        assert_eq!(dialog.send_display(), "date");
    }

    #[test]
    fn send_display_date_with_time() {
        let mut dialog = SimplePromptDialog::new();
        dialog.send_choice = SendChoice::Date;
        dialog.send_at = Some(
            chrono::NaiveDate::from_ymd_opt(2026, 7, 20)
                .unwrap()
                .and_hms_opt(14, 30, 0)
                .unwrap(),
        );
        assert_eq!(dialog.send_display(), "2026-07-20 14:30");
    }

    // ── send toggle and clear ────────────────────────────────────

    #[test]
    fn send_toggle_now_to_date() {
        let mut dialog = SimplePromptDialog::new();
        dialog.send_toggle();
        assert_eq!(dialog.send_choice, SendChoice::Date);
    }

    #[test]
    fn send_toggle_date_to_now_clears() {
        let mut dialog = SimplePromptDialog::new();
        dialog.send_choice = SendChoice::Date;
        dialog.send_at = Some(chrono::Local::now().naive_local());
        dialog.send_edit = Some(SendAtEdit {
            value: chrono::Local::now().naive_local(),
            field: 0,
            typed: 0,
            typed_len: 0,
        });
        dialog.send_toggle();
        assert_eq!(dialog.send_choice, SendChoice::Now);
        assert!(dialog.send_at.is_none());
        assert!(dialog.send_edit.is_none());
    }

    #[test]
    fn clear_send_at() {
        let mut dialog = SimplePromptDialog::new();
        dialog.send_choice = SendChoice::Date;
        dialog.send_at = Some(chrono::Local::now().naive_local());
        dialog.send_edit = Some(SendAtEdit {
            value: chrono::Local::now().naive_local(),
            field: 0,
            typed: 0,
            typed_len: 0,
        });
        dialog.clear_send_at();
        assert_eq!(dialog.send_choice, SendChoice::Now);
        assert!(dialog.send_at.is_none());
        assert!(dialog.send_edit.is_none());
    }

    // ── send_edit operations ─────────────────────────────────────

    #[test]
    fn send_edit_move_clamps_field() {
        let mut dialog = SimplePromptDialog::new();
        dialog.send_begin_edit();
        dialog.send_edit_move(10);
        let edit = dialog.send_edit.unwrap();
        assert_eq!(edit.field, 4); // clamped to minute
        dialog.send_edit_move(-100);
        let edit = dialog.send_edit.unwrap();
        assert_eq!(edit.field, 0); // clamped to year
    }

    #[test]
    fn send_edit_adjust_day() {
        let mut dialog = SimplePromptDialog::new();
        dialog.send_begin_edit();
        dialog.send_edit.as_mut().unwrap().field = 2; // day
        dialog.send_edit_adjust(1);
        let edit = dialog.send_edit.unwrap();
        assert_eq!(edit.value.day(), chrono::Local::now().naive_local().day() + 1);
    }

    #[test]
    fn send_edit_type_digit_auto_advances_field() {
        let mut dialog = SimplePromptDialog::new();
        dialog.send_begin_edit();
        dialog.send_edit.as_mut().unwrap().field = 3; // hour (width 2)
        dialog.send_edit_type_digit(1);
        // After 1 digit (not yet full), typed=1, typed_len=1
        assert_eq!(dialog.send_edit.as_ref().unwrap().typed, 1);
        assert_eq!(dialog.send_edit.as_ref().unwrap().typed_len, 1);
        dialog.send_edit_type_digit(4);
        // After 2 digits (field full), auto-advances to minute, typed resets
        assert_eq!(dialog.send_edit.as_ref().unwrap().field, 4);
        assert_eq!(dialog.send_edit.as_ref().unwrap().typed, 0);
    }

    #[test]
    fn send_edit_confirm_past_rejects() {
        let mut dialog = SimplePromptDialog::new();
        dialog.send_begin_edit();
        dialog.send_edit.as_mut().unwrap().value = chrono::NaiveDateTime::default(); // epoch
        assert!(!dialog.send_edit_confirm());
        assert!(dialog.send_error.is_some());
    }

    #[test]
    fn send_edit_cancel_no_prior_send_returns_now() {
        let mut dialog = SimplePromptDialog::new();
        dialog.send_begin_edit();
        dialog.send_edit_cancel();
        assert!(dialog.send_edit.is_none());
        assert_eq!(dialog.send_choice, SendChoice::Now);
    }

    #[test]
    fn send_edit_cancel_with_prior_send_stays_date() {
        let mut dialog = SimplePromptDialog::new();
        dialog.send_choice = SendChoice::Date;
        dialog.send_at = Some(chrono::Local::now().naive_local());
        dialog.send_begin_edit();
        dialog.send_edit_cancel();
        assert!(dialog.send_edit.is_none());
        assert_eq!(dialog.send_choice, SendChoice::Date);
    }

    // ── is_send_at_focused ───────────────────────────────────────

    #[test]
    fn is_send_at_focused_true() {
        let mut dialog = SimplePromptDialog::new();
        dialog.focused_section = 0;
        assert!(dialog.is_send_at_focused());
    }

    #[test]
    fn is_send_at_focused_false_on_section() {
        let dialog = SimplePromptDialog::new();
        assert!(!dialog.is_send_at_focused());
    }

    // ── total_focusable ──────────────────────────────────────────

    #[test]
    fn total_focusable_empty_sections() {
        let mut dialog = SimplePromptDialog::new();
        dialog.enabled_sections.clear();
        assert_eq!(dialog.total_focusable(), 1); // just send control
    }

    #[test]
    fn total_focusable_with_sections() {
        let mut dialog = SimplePromptDialog::new();
        dialog.add_section("goal");
        dialog.add_section("context");
        // instruction_1 + goal_1 + context_1 = 3 sections + send = 4
        assert_eq!(dialog.total_focusable(), 4);
    }

    // ── section_entries and section_lines ─────────────────────────

    #[test]
    fn section_entries_filters_empty() {
        let mut dialog = SimplePromptDialog::new();
        dialog.set_section_content("instruction_1", "  ".to_string());
        assert!(dialog.section_entries("instruction").is_empty());
    }

    #[test]
    fn section_entries_includes_non_empty() {
        let mut dialog = SimplePromptDialog::new();
        dialog.set_section_content("instruction_1", "do something".to_string());
        assert_eq!(dialog.section_entries("instruction").len(), 1);
    }

    #[test]
    fn section_lines_splits_multiline() {
        let mut dialog = SimplePromptDialog::new();
        dialog.set_section_content("instruction_1", "line1\nline2\nline3".to_string());
        let lines = dialog.section_lines("instruction");
        assert_eq!(lines.len(), 3);
    }

    // ── build_body ───────────────────────────────────────────────

    #[test]
    fn build_body_empty_dialog() {
        let dialog = SimplePromptDialog::new();
        let body = dialog.build_body();
        assert!(!body.contains("GOAL"));
        assert!(!body.contains("CONTEXT"));
        assert!(body.contains("INSTRUCTIONS"));
    }

    #[test]
    fn build_body_with_goal() {
        let mut dialog = SimplePromptDialog::new();
        dialog.set_section_content("instruction_1", "do it".to_string());
        dialog.add_section_with_content("goal", "ship the feature".to_string());
        let body = dialog.build_body();
        assert!(body.contains("GOAL"));
        assert!(body.contains("ship the feature"));
    }

    // ── collect_tool_lines ───────────────────────────────────────

    #[test]
    fn collect_tool_lines_empty() {
        let dialog = SimplePromptDialog::new();
        assert!(dialog.collect_tool_lines().is_empty());
    }

    #[test]
    fn collect_tool_lines_with_content() {
        let mut dialog = SimplePromptDialog::new();
        dialog.add_section_with_content("tools", "skill:code-engineering\nskill:rust-idiomatic".to_string());
        let lines = dialog.collect_tool_lines();
        assert_eq!(lines.len(), 2);
    }

    // ── PromptBuilderSession round-trip ──────────────────────────

    #[test]
    fn prompt_builder_session_from_dialog_copies_all_fields() {
        let mut dialog = SimplePromptDialog::new();
        dialog.set_section_content("instruction_1", "test".to_string());
        dialog.add_section_with_content("context", "ctx".to_string());
        dialog.send_choice = SendChoice::Date;
        dialog.send_at = Some(chrono::Local::now().naive_local());

        let session = PromptBuilderSession::from_dialog(&dialog);
        assert_eq!(
            session.sections.get("instruction_1").map(String::as_str),
            Some("test")
        );
        // Find the context section (it will have a generated name like "context_1" or "context_2")
        let ctx_val = session
            .sections
            .iter()
            .find(|(k, _)| k.starts_with("context"))
            .map(|(_, v)| v.as_str());
        assert_eq!(ctx_val, Some("ctx"));
        // send_at is persisted in PromptBuilderSession
        assert!(session.send_at.is_some());
    }

    // ── PersistedBuilderState round-trip ─────────────────────────

    #[test]
    fn persisted_builder_state_excludes_send_at() {
        let mut dialog = SimplePromptDialog::new();
        dialog.set_section_content("instruction_1", "test".to_string());
        dialog.send_choice = SendChoice::Date;
        dialog.send_at = Some(chrono::Local::now().naive_local());

        let snapshot = PersistedBuilderState::from_dialog(&dialog);
        let json = serde_json::to_string(&snapshot).unwrap();
        let restored: PersistedBuilderState = serde_json::from_str(&json).unwrap();

        let mut target = SimplePromptDialog::new();
        restored.restore_into(&mut target);
        // send_at must NOT be restored from persisted state
        assert!(target.send_at.is_none());
    }

    // ── get_removable_sections ───────────────────────────────────

    #[test]
    fn get_removable_sections_single_instruction_not_removable() {
        let dialog = SimplePromptDialog::new();
        let removable = dialog.get_removable_sections();
        assert!(removable.is_empty());
    }

    #[test]
    fn get_removable_sections_multiple_instructions_all_removable() {
        let mut dialog = SimplePromptDialog::new();
        dialog.add_section("instruction");
        let removable = dialog.get_removable_sections();
        assert_eq!(removable.len(), 2);
    }

    #[test]
    fn get_removable_sections_non_instruction_always_removable() {
        let mut dialog = SimplePromptDialog::new();
        dialog.add_section("context");
        let removable = dialog.get_removable_sections();
        assert!(removable.iter().any(|(id, _)| id.starts_with("context")));
    }

    // ── remove_section protection ────────────────────────────────

    #[test]
    fn remove_section_last_instruction_protected() {
        let mut dialog = SimplePromptDialog::new();
        let id = dialog.enabled_sections[0].clone();
        dialog.remove_section(&id);
        // Should not have removed it
        assert_eq!(dialog.enabled_sections.len(), 1);
    }

    #[test]
    fn remove_section_non_instruction_removable() {
        let mut dialog = SimplePromptDialog::new();
        dialog.add_section("context");
        let ctx_id = dialog
            .enabled_sections
            .iter()
            .find(|id| id.starts_with("context"))
            .cloned()
            .unwrap();
        let len_before = dialog.enabled_sections.len();
        dialog.remove_section(&ctx_id);
        assert_eq!(dialog.enabled_sections.len(), len_before - 1);
    }

    // ── get_section_content / set_section_content ─────────────────

    #[test]
    fn get_section_content_missing_returns_empty() {
        let dialog = SimplePromptDialog::new();
        assert!(dialog.get_section_content("nonexistent").is_empty());
    }

    #[test]
    fn set_section_content_round_trip() {
        let mut dialog = SimplePromptDialog::new();
        dialog.set_section_content("instruction_1", "hello".to_string());
        assert_eq!(dialog.get_section_content("instruction_1"), "hello");
    }

    // ── section_content_for_build with collapsed paste ────────────

    #[test]
    fn section_content_for_build_returns_collapsed_paste() {
        let mut dialog = SimplePromptDialog::new();
        dialog.set_section_content("instruction_1", "[Pasted ~3 lines]".to_string());
        dialog.collapsed_pastes.insert(
            "instruction_1".to_string(),
            "real content\nmore lines\nfinal".to_string(),
        );
        assert_eq!(
            dialog.section_content_for_build("instruction_1"),
            Some("real content\nmore lines\nfinal")
        );
    }

    // ── should_collapse_paste ────────────────────────────────────

    #[test]
    fn should_collapse_paste_single_short_line() {
        assert!(!SimplePromptDialog::should_collapse_paste("hello"));
    }

    #[test]
    fn should_collapse_paste_multiline() {
        assert!(SimplePromptDialog::should_collapse_paste("line1\nline2"));
    }

    #[test]
    fn should_collapse_paste_long_line() {
        assert!(SimplePromptDialog::should_collapse_paste(&"a".repeat(201)));
    }

    #[test]
    fn should_collapse_paste_exactly_200_chars() {
        assert!(!SimplePromptDialog::should_collapse_paste(&"a".repeat(200)));
    }

    // ── expand_collapsed_paste ───────────────────────────────────

    #[test]
    fn expand_collapsed_paste_restores_content() {
        let mut dialog = SimplePromptDialog::new();
        dialog.set_section_content("instruction_1", "[Pasted ~2 lines]".to_string());
        dialog.collapsed_pastes.insert(
            "instruction_1".to_string(),
            "real\ncontent".to_string(),
        );
        dialog.expand_collapsed_paste("instruction_1");
        assert_eq!(dialog.get_section_content("instruction_1"), "real\ncontent");
        assert!(!dialog.has_collapsed_paste("instruction_1"));
    }

    #[test]
    fn expand_collapsed_paste_noop_without_paste() {
        let mut dialog = SimplePromptDialog::new();
        dialog.set_section_content("instruction_1", "normal".to_string());
        dialog.expand_collapsed_paste("instruction_1");
        assert_eq!(dialog.get_section_content("instruction_1"), "normal");
    }

    // ── cursor_in_collapsed_placeholder ───────────────────────────

    #[test]
    fn cursor_in_collapsed_placeholder_no_paste() {
        let dialog = SimplePromptDialog::new();
        assert!(!dialog.cursor_in_collapsed_placeholder("instruction_1"));
    }

    // ── scroll_raw_preview ───────────────────────────────────────

    #[test]
    fn scroll_raw_preview_clamps() {
        let mut dialog = SimplePromptDialog::new();
        dialog.raw_preview = Some("line1\nline2\nline3".to_string());
        dialog.scroll_raw_preview(100);
        assert_eq!(dialog.raw_preview_scroll, 2); // max = 3 lines - 1 = 2
    }

    #[test]
    fn scroll_raw_preview_negative_clamps_to_zero() {
        let mut dialog = SimplePromptDialog::new();
        dialog.raw_preview = Some("line1\nline2".to_string());
        dialog.raw_preview_scroll = 0;
        dialog.scroll_raw_preview(-5);
        assert_eq!(dialog.raw_preview_scroll, 0);
    }

    // ── load_flat_text ───────────────────────────────────────────

    #[test]
    fn load_flat_text_resets_counters() {
        let mut dialog = SimplePromptDialog::new();
        dialog.add_section("goal");
        dialog.add_section("context");
        dialog.load_flat_text("recovered");
        assert_eq!(dialog.enabled_sections.len(), 1);
        assert_eq!(dialog.get_section_content("instruction_1"), "recovered");
    }

    // ── insert_collapsed_paste_at_cursor short text ──────────────

    #[test]
    fn insert_collapsed_paste_short_text_not_collapsed() {
        let mut dialog = SimplePromptDialog::new();
        dialog.insert_collapsed_paste_at_cursor("instruction_1", "short", 80);
        assert_eq!(dialog.get_section_content("instruction_1"), "short");
        assert!(!dialog.has_collapsed_paste("instruction_1"));
    }

    // ── backspace_collapsed_paste without paste ───────────────────

    #[test]
    fn backspace_collapsed_paste_noop_without_paste() {
        let mut dialog = SimplePromptDialog::new();
        dialog.set_section_content("instruction_1", "normal text".to_string());
        dialog.backspace_collapsed_paste("instruction_1", 80);
        assert_eq!(dialog.get_section_content("instruction_1"), "normal text");
    }

    // ── add_resource_reference ───────────────────────────────────

    #[test]
    fn add_resource_reference_no_existing_section() {
        let mut dialog = SimplePromptDialog::new();
        dialog.add_resource_reference("/path/to/file");
        assert!(dialog
            .enabled_sections
            .iter()
            .any(|id| id.starts_with("resources")));
        assert_eq!(
            dialog.get_section_content(
                &dialog
                    .resources_section_id()
                    .unwrap()
            ),
            "/path/to/file"
        );
    }

    #[test]
    fn add_resource_reference_appends_to_existing() {
        let mut dialog = SimplePromptDialog::new();
        dialog.add_section_with_content("resources", "existing".to_string());
        dialog.add_resource_reference("/new/path");
        let content = dialog.get_section_content(&dialog.resources_section_id().unwrap());
        assert!(content.contains("existing"));
        assert!(content.contains("/new/path"));
    }

    // ── resources_section_id ─────────────────────────────────────

    #[test]
    fn resources_section_id_none_when_empty() {
        let dialog = SimplePromptDialog::new();
        assert!(dialog.resources_section_id().is_none());
    }

    #[test]
    fn resources_section_id_found() {
        let mut dialog = SimplePromptDialog::new();
        dialog.add_section("resources");
        assert!(dialog.resources_section_id().is_some());
    }

    // ── display name ─────────────────────────────────────────────

    #[test]
    fn section_display_name_with_number() {
        let name = SimplePromptDialog::section_display_name("instruction_1");
        assert!(name.contains("Instruction"));
        assert!(name.contains("1"));
    }

    #[test]
    fn section_display_name_without_number() {
        let name = SimplePromptDialog::section_display_name("tools");
        assert!(name.contains("Tools"));
    }

    // ── format_file_resource ────────────────────────────────────

    #[test]
    fn format_file_resource_shows_path_and_kind() {
        let result = SimplePromptDialog::format_file_resource(Path::new("/tmp/test.rs"));
        assert!(result.contains("path: /tmp/test.rs"));
        assert!(result.contains("kind: file"));
    }

    // ── format_project_block ────────────────────────────────────

    #[test]
    fn format_project_block_required_fields() {
        use crate::domain::project::Project;
        let project = Project {
            hash: "abc123".to_string(),
            name: "test-project".to_string(),
            path: "/tmp/project".to_string(),
            description: None,
            tags: None,
            indexed_at: None,
            created_at: 0,
        };
        let result = SimplePromptDialog::format_project_block(&project);
        assert!(result.contains("name: test-project"));
        assert!(result.contains("workdir_hash: abc123"));
        assert!(result.contains("path: /tmp/project"));
        assert!(!result.contains("description:"));
        assert!(!result.contains("tags:"));
        assert!(!result.contains("indexed_at:"));
    }

    #[test]
    fn format_project_block_with_description() {
        use crate::domain::project::Project;
        let project = Project {
            hash: "def456".to_string(),
            name: "proj".to_string(),
            path: "/p".to_string(),
            description: Some("a project".to_string()),
            tags: None,
            indexed_at: None,
            created_at: 0,
        };
        let result = SimplePromptDialog::format_project_block(&project);
        assert!(result.contains("description: a project"));
    }

    #[test]
    fn format_project_block_with_tags() {
        use crate::domain::project::Project;
        let project = Project {
            hash: "ghi".to_string(),
            name: "p".to_string(),
            path: "/p".to_string(),
            description: None,
            tags: Some("rust,tui".to_string()),
            indexed_at: None,
            created_at: 0,
        };
        let result = SimplePromptDialog::format_project_block(&project);
        assert!(result.contains("tags: rust,tui"));
    }

    #[test]
    fn format_project_block_with_indexed_at() {
        use crate::domain::project::Project;
        let project = Project {
            hash: "jkl".to_string(),
            name: "p".to_string(),
            path: "/p".to_string(),
            description: None,
            tags: None,
            indexed_at: Some(1700000000),
            created_at: 0,
        };
        let result = SimplePromptDialog::format_project_block(&project);
        assert!(result.contains("indexed_at:"));
    }

    // ── is_locked / lock_section ────────────────────────────────

    #[test]
    fn is_locked_false_for_new_section() {
        let dialog = SimplePromptDialog::new();
        assert!(!dialog.is_locked("instruction_1"));
    }

    #[test]
    fn is_locked_true_after_lock() {
        let mut dialog = SimplePromptDialog::new();
        dialog.lock_section("instruction_1");
        assert!(dialog.is_locked("instruction_1"));
    }

    #[test]
    fn is_locked_false_for_other_section() {
        let mut dialog = SimplePromptDialog::new();
        dialog.lock_section("instruction_1");
        assert!(!dialog.is_locked("context_1"));
    }

    // ── set_tools_section_skill ─────────────────────────────────

    #[test]
    fn set_tools_section_skill_sets_content() {
        let mut dialog = SimplePromptDialog::new();
        dialog.add_section("tools");
        let tools_id = dialog
            .enabled_sections
            .iter()
            .find(|id| id.starts_with("tools"))
            .cloned()
            .unwrap();
        dialog.set_tools_section_skill(&tools_id, "skill:code-engineering");
        assert_eq!(
            dialog.get_section_content(&tools_id),
            "skill:code-engineering"
        );
    }

    #[test]
    fn set_tools_section_skill_replaces_existing() {
        let mut dialog = SimplePromptDialog::new();
        dialog.add_section_with_content("tools", "old-skill".to_string());
        let tools_id = dialog
            .enabled_sections
            .iter()
            .find(|id| id.starts_with("tools"))
            .cloned()
            .unwrap();
        dialog.set_tools_section_skill(&tools_id, "new-skill");
        assert_eq!(dialog.get_section_content(&tools_id), "new-skill");
    }

    // ── generate_section_id ─────────────────────────────────────

    #[test]
    fn generate_section_id_increments_counter() {
        let mut dialog = SimplePromptDialog::new();
        let id1 = dialog.generate_section_id("goal");
        let id2 = dialog.generate_section_id("goal");
        let id3 = dialog.generate_section_id("goal");
        assert_eq!(id1, "goal_1");
        assert_eq!(id2, "goal_2");
        assert_eq!(id3, "goal_3");
    }

    #[test]
    fn generate_section_id_independent_per_type() {
        let mut dialog = SimplePromptDialog::new();
        let g = dialog.generate_section_id("goal");
        let c = dialog.generate_section_id("context");
        assert_eq!(g, "goal_1");
        // context counter starts at 2 from new()
        assert_eq!(c, "context_2");
    }

    // ── instruction_count ───────────────────────────────────────

    #[test]
    fn instruction_count_single() {
        let dialog = SimplePromptDialog::new();
        assert_eq!(dialog.instruction_count(), 1);
    }

    #[test]
    fn instruction_count_after_add() {
        let mut dialog = SimplePromptDialog::new();
        dialog.add_section("instruction");
        assert_eq!(dialog.instruction_count(), 2);
    }

    #[test]
    fn instruction_count_after_remove() {
        let mut dialog = SimplePromptDialog::new();
        dialog.add_section("instruction");
        assert_eq!(dialog.instruction_count(), 2);
        let extra = dialog
            .enabled_sections
            .iter()
            .find(|id| id.starts_with("instruction") && *id != "instruction_1")
            .cloned()
            .unwrap();
        dialog.remove_section(&extra);
        assert_eq!(dialog.instruction_count(), 1);
    }

    // ── cursor / scroll edge cases ──────────────────────────────

    #[test]
    fn cursor_default_for_missing_section() {
        let dialog = SimplePromptDialog::new();
        assert_eq!(dialog.cursor("nonexistent"), 0);
    }

    #[test]
    fn scroll_default_for_missing_section() {
        let dialog = SimplePromptDialog::new();
        assert_eq!(dialog.scroll("nonexistent"), 0);
    }

    #[test]
    fn cursor_returns_stored_value() {
        let mut dialog = SimplePromptDialog::new();
        dialog.section_cursors.insert("instruction_1".to_string(), 5);
        assert_eq!(dialog.cursor("instruction_1"), 5);
    }

    #[test]
    fn scroll_returns_stored_value() {
        let mut dialog = SimplePromptDialog::new();
        dialog.section_scrolls.insert("instruction_1".to_string(), 3);
        assert_eq!(dialog.scroll("instruction_1"), 3);
    }

    // ── is_file_reference more edge cases ───────────────────────

    #[test]
    fn is_file_reference_with_path_separator() {
        assert!(SimplePromptDialog::is_file_reference("@src/main.rs"));
        assert!(SimplePromptDialog::is_file_reference("@a/b/c"));
    }

    #[test]
    fn is_file_reference_with_underscore() {
        assert!(SimplePromptDialog::is_file_reference("@my_file"));
    }

    #[test]
    fn is_file_reference_with_hyphen() {
        assert!(SimplePromptDialog::is_file_reference("@my-file"));
    }

    #[test]
    fn is_file_reference_with_dot() {
        assert!(SimplePromptDialog::is_file_reference("@file.txt"));
        assert!(SimplePromptDialog::is_file_reference("@.hidden"));
    }

    // ── next_file_reference edge cases ──────────────────────────

    #[test]
    fn next_file_reference_at_end_of_string() {
        let text = "hello @";
        assert!(SimplePromptDialog::next_file_reference(text, 0).is_some());
    }

    #[test]
    fn next_file_reference_at_only_char() {
        let text = "@";
        let result = SimplePromptDialog::next_file_reference(text, 0);
        assert!(result.is_some());
    }

    // ── visual_line_count more edge cases ───────────────────────

    #[test]
    fn visual_line_count_mixed_newlines_and_wrap() {
        // "ab\ncdefghij" with width 5 → "ab" (1 line) + newline → "cdef" + "ghij" = 3 lines
        assert_eq!(SimplePromptDialog::visual_line_count("ab\ncdefghij", 5), 3);
    }

    #[test]
    fn visual_line_count_tab_at_end_of_line() {
        // Tab at col 2 with width 5: tab takes 2 cols (4 - 2%4 = 2), total col=4 ≤ 5 → 1 line
        assert_eq!(SimplePromptDialog::visual_line_count("ab\t", 5), 1);
    }

    #[test]
    fn visual_line_count_multiple_tabs() {
        assert_eq!(SimplePromptDialog::visual_line_count("\t\t", 10), 1);
    }

    // ── get_available_sections ──────────────────────────────────

    #[test]
    fn get_available_sections_has_core_types() {
        let sections = SimplePromptDialog::get_available_sections();
        let names: Vec<&str> = sections.iter().map(|(name, _)| *name).collect();
        assert!(names.contains(&"instruction"));
        assert!(names.contains(&"goal"));
        assert!(names.contains(&"context"));
        assert!(names.contains(&"resources"));
        assert!(names.contains(&"constraints"));
        assert!(names.contains(&"tools"));
        assert!(names.contains(&"preset"));
    }

    // ── focused_section_name edge cases ─────────────────────────

    #[test]
    fn focused_section_name_send_control_returns_none() {
        let mut dialog = SimplePromptDialog::new();
        dialog.focused_section = 0;
        assert!(dialog.focused_section_name().is_none());
    }

    #[test]
    fn focused_section_name_first_section() {
        let dialog = SimplePromptDialog::new();
        assert_eq!(dialog.focused_section_name(), Some("instruction_1"));
    }

    #[test]
    fn focused_section_name_out_of_bounds_returns_none() {
        let mut dialog = SimplePromptDialog::new();
        dialog.focused_section = 99;
        assert!(dialog.focused_section_name().is_none());
    }

    // ── add_resource_reference: existing content is empty ───────

    #[test]
    fn add_resource_reference_empty_existing_content() {
        let mut dialog = SimplePromptDialog::new();
        dialog.add_section_with_content("resources", String::new());
        dialog.add_resource_reference("/new/path");
        let rid = dialog.resources_section_id().unwrap();
        assert_eq!(dialog.get_section_content(&rid), "/new/path");
    }

    // ── strip_resources_section edge cases ───────────────────────

    #[test]
    fn strip_resources_section_empty_string() {
        assert_eq!(strip_resources_section(""), "");
    }

    #[test]
    fn strip_resources_section_only_header_no_closing() {
        let input = "# [RESOURCES]: Knowledge Base & Data\n<resources>\n";
        assert_eq!(strip_resources_section(input), input);
    }

    // ── remove_section: second instruction removable ────────────

    #[test]
    fn remove_section_second_instruction_removable() {
        let mut dialog = SimplePromptDialog::new();
        dialog.add_section("instruction");
        let extra_id = dialog
            .enabled_sections
            .iter()
            .find(|id| id.starts_with("instruction") && *id != "instruction_1")
            .cloned()
            .unwrap();
        let len_before = dialog.enabled_sections.len();
        dialog.remove_section(&extra_id);
        assert_eq!(dialog.enabled_sections.len(), len_before - 1);
    }

    // ── set_tab no-op when already active ───────────────────────

    #[test]
    fn set_tab_same_tab_is_noop() {
        let mut dialog = SimplePromptDialog::new();
        dialog.set_tab(PromptTab::Normal);
        assert_eq!(dialog.active_tab, PromptTab::Normal);
        assert_eq!(dialog.focused_section, 1);
    }

    #[test]
    fn set_tab_raw_to_normal_focuses_first_section() {
        let mut dialog = SimplePromptDialog::new();
        dialog.set_tab(PromptTab::Raw);
        assert_eq!(dialog.active_tab, PromptTab::Raw);
        dialog.set_tab(PromptTab::Normal);
        assert_eq!(dialog.active_tab, PromptTab::Normal);
        assert_eq!(dialog.focused_section, 1);
    }

    // ── send_edit_type_digit: restart on full field ─────────────

    #[test]
    fn send_edit_type_digit_restarts_full_field() {
        let mut dialog = SimplePromptDialog::new();
        dialog.send_begin_edit();
        dialog.send_edit.as_mut().unwrap().field = 3; // hour (width 2)
        dialog.send_edit_type_digit(1);
        dialog.send_edit_type_digit(4);
        // Now on minute, auto-advanced
        assert_eq!(dialog.send_edit.as_ref().unwrap().field, 4);
        // Type a digit that fills the field
        dialog.send_edit_type_digit(5);
        dialog.send_edit_type_digit(9);
        // Minute full, auto-advanced would go to field 5 (clamped to 4)
        // But if we type again on a full field, it restarts
        dialog.send_edit_type_digit(3);
        let edit = dialog.send_edit.as_ref().unwrap();
        assert_eq!(edit.typed, 3);
        assert_eq!(edit.typed_len, 1);
    }

    // ── send_edit_adjust: all fields ────────────────────────────

    #[test]
    fn send_edit_adjust_year() {
        let mut dialog = SimplePromptDialog::new();
        dialog.send_begin_edit();
        dialog.send_edit.as_mut().unwrap().field = 0;
        dialog.send_edit_adjust(2);
        let year = dialog.send_edit.unwrap().value.year();
        let now_year = chrono::Local::now().naive_local().year();
        assert_eq!(year, now_year + 2);
    }

    #[test]
    fn send_edit_adjust_month() {
        let mut dialog = SimplePromptDialog::new();
        dialog.send_begin_edit();
        dialog.send_edit.as_mut().unwrap().field = 1;
        dialog.send_edit_adjust(1);
        let month = dialog.send_edit.unwrap().value.month();
        let now_month = chrono::Local::now().naive_local().month();
        assert_eq!(month, (now_month % 12) + 1);
    }

    #[test]
    fn send_edit_adjust_hour() {
        let mut dialog = SimplePromptDialog::new();
        dialog.send_begin_edit();
        dialog.send_edit.as_mut().unwrap().field = 3;
        dialog.send_edit_adjust(1);
        let hour = dialog.send_edit.unwrap().value.hour();
        let now_hour = chrono::Local::now().naive_local().hour();
        assert_eq!(hour, (now_hour + 1) % 24);
    }

    #[test]
    fn send_edit_adjust_minute() {
        let mut dialog = SimplePromptDialog::new();
        dialog.send_begin_edit();
        dialog.send_edit.as_mut().unwrap().field = 4;
        dialog.send_edit_adjust(5);
        let minute = dialog.send_edit.unwrap().value.minute();
        let now_minute = chrono::Local::now().naive_local().minute();
        assert_eq!(minute, (now_minute + 5) % 60);
    }

    // ── send_edit_adjust_no_edit_is_noop ────────────────────────

    #[test]
    fn send_edit_adjust_no_edit_is_noop() {
        let mut dialog = SimplePromptDialog::new();
        dialog.send_edit_adjust(5);
        assert!(dialog.send_edit.is_none());
    }

    #[test]
    fn send_edit_type_digit_no_edit_is_noop() {
        let mut dialog = SimplePromptDialog::new();
        dialog.send_edit_type_digit(5);
        assert!(dialog.send_edit.is_none());
    }

    // ── visual_positions ────────────────────────────────────────

    #[test]
    fn visual_positions_empty_string() {
        let positions = SimplePromptDialog::visual_positions("", 10);
        assert_eq!(positions.len(), 1);
        assert_eq!(positions[0], (0, 0));
    }

    #[test]
    fn visual_positions_single_char() {
        let positions = SimplePromptDialog::visual_positions("a", 10);
        assert_eq!(positions.len(), 2);
        assert_eq!(positions[0], (0, 0));
        assert_eq!(positions[1], (0, 1));
    }

    #[test]
    fn visual_positions_wrap_at_width() {
        let positions = SimplePromptDialog::visual_positions("abcde", 3);
        assert_eq!(positions.len(), 6);
        // 'a' col 0→1, 'b' col 1→2, 'c' col 2→3, 'd' col 3 wraps to (1,1), 'e' col 1→2
        assert_eq!(positions[4], (1, 1));
        assert_eq!(positions[5], (1, 2));
    }

    // ── build_xml_block ─────────────────────────────────────────

    #[test]
    fn build_xml_block_empty_sections() {
        let mut result = String::new();
        build_xml_block(
            &mut result,
            &[],
            |_| true,
            |_| Some("content"),
            "# Header\n",
            "outer",
            "item",
        );
        assert!(result.is_empty());
    }

    #[test]
    fn build_xml_block_no_matching_sections() {
        let mut result = String::new();
        let sections = vec!["goal_1".to_string()];
        build_xml_block(
            &mut result,
            &sections,
            |id| id.starts_with("context"),
            |_| Some("content"),
            "# Header\n",
            "outer",
            "item",
        );
        assert!(result.is_empty());
    }

    #[test]
    fn build_xml_block_matching_empty_content_skipped() {
        let mut result = String::new();
        let sections = vec!["context_1".to_string()];
        build_xml_block(
            &mut result,
            &sections,
            |id| id.starts_with("context"),
            |_| Some("  "),
            "# Header\n",
            "outer",
            "item",
        );
        assert!(result.is_empty());
    }

    // ── get_file_reference_with_styling ─────────────────────────

    #[test]
    fn get_file_reference_with_styling_no_refs() {
        let dialog = SimplePromptDialog::new();
        let result =
            dialog.get_file_reference_with_styling("hello world", Color::Blue);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].0, "hello world");
        assert!(result[0].1.is_none());
    }

    #[test]
    fn get_file_reference_with_styling_with_ref() {
        let dialog = SimplePromptDialog::new();
        let result =
            dialog.get_file_reference_with_styling("look @src/lib.rs here", Color::Blue);
        // "look " + "@src/lib.rs" + " here"
        assert_eq!(result.len(), 3);
        assert_eq!(result[0].0, "look ");
        assert!(result[0].1.is_none());
        assert_eq!(result[1].0, "@src/lib.rs");
        assert_eq!(result[1].1, Some(Color::Blue));
        assert_eq!(result[2].0, " here");
        assert!(result[2].1.is_none());
    }

    #[test]
    fn get_file_reference_with_styling_invalid_ref() {
        let dialog = SimplePromptDialog::new();
        let result =
            dialog.get_file_reference_with_styling("@ invalid", Color::Blue);
        // "@" is a valid ref start but "invalid" is part of " @ invalid"
        // Actually: next_file_reference finds @ at position 0, ref = "@", which has
        // len 1 and starts with @, so is_file_reference returns false.
        assert_eq!(result.len(), 2);
        assert_eq!(result[0].0, "@");
        assert!(result[0].1.is_none());
    }

    // ── insert_at_completion ────────────────────────────────────

    #[test]
    fn insert_at_completion_no_picker_is_noop() {
        let mut dialog = SimplePromptDialog::new();
        dialog.set_section_content("instruction_1", "hello @".to_string());
        dialog.at_picker = None;
        dialog.insert_at_completion("instruction_1", "file.rs", "/abs/file.rs", 80);
        // Content unchanged
        assert_eq!(dialog.get_section_content("instruction_1"), "hello @");
    }

    // ── replace_char_range ──────────────────────────────────────

    #[test]
    fn replace_char_range_basic() {
        let mut dialog = SimplePromptDialog::new();
        dialog.set_section_content("instruction_1", "hello world".to_string());
        dialog.replace_char_range("instruction_1", 5, 6, "@", 80);
        assert_eq!(dialog.get_section_content("instruction_1"), "hello@world");
    }

    #[test]
    fn replace_char_range_clamps_end() {
        let mut dialog = SimplePromptDialog::new();
        dialog.set_section_content("instruction_1", "abc".to_string());
        dialog.replace_char_range("instruction_1", 1, 100, "X", 80);
        assert_eq!(dialog.get_section_content("instruction_1"), "aX");
    }

    #[test]
    fn replace_char_range_empty_replacement() {
        let mut dialog = SimplePromptDialog::new();
        dialog.set_section_content("instruction_1", "abc".to_string());
        dialog.replace_char_range("instruction_1", 1, 2, "", 80);
        assert_eq!(dialog.get_section_content("instruction_1"), "ac");
    }

    // ── insert_section: multiple types ──────────────────────────

    #[test]
    fn insert_section_adds_to_enabled_and_focuses() {
        let mut dialog = SimplePromptDialog::new();
        let id = dialog.insert_section("constraint", "be careful".to_string());
        assert!(id.starts_with("constraint"));
        assert!(dialog.enabled_sections.contains(&id));
        assert_eq!(dialog.get_section_content(&id), "be careful");
        // Focus should be on the new section
        assert_eq!(dialog.focused_section, dialog.enabled_sections.len());
    }

    // ── resolve_resource_entry: URL ─────────────────────────────

    #[test]
    fn resolve_resource_entry_url() {
        let temp = tempdir().unwrap();
        let db_path = temp.path().join("canopy.db");
        let db = Database::new(&db_path).unwrap();
        let result = SimplePromptDialog::resolve_resource_entry(
            &db,
            "https://example.com/doc",
        );
        assert!(result.contains("https://example.com/doc"));
        assert!(result.contains("kind: url"));
    }

    #[test]
    fn resolve_resource_entry_raw_text() {
        let temp = tempdir().unwrap();
        let db_path = temp.path().join("canopy.db");
        let db = Database::new(&db_path).unwrap();
        let result = SimplePromptDialog::resolve_resource_entry(&db, "just some text");
        assert!(result.contains("kind: raw"));
        assert!(result.contains("just some text"));
    }

    // ── section_content_for_build: no paste returns section ──────

    #[test]
    fn section_content_for_build_no_paste() {
        let mut dialog = SimplePromptDialog::new();
        dialog.set_section_content("instruction_1", "real".to_string());
        assert_eq!(
            dialog.section_content_for_build("instruction_1"),
            Some("real")
        );
    }

    #[test]
    fn section_content_for_build_missing_section() {
        let dialog = SimplePromptDialog::new();
        assert!(dialog.section_content_for_build("nonexistent").is_none());
    }
}

/// Snapshot of `SimplePromptDialog` state used to persist the prompt builder
/// per agent/session across openings within the same canopy TUI session.
#[derive(Clone)]
pub struct PromptBuilderSession {
    pub sections: HashMap<String, String>,
    pub enabled_sections: Vec<String>,
    pub section_counters: HashMap<String, usize>,
    pub section_cursors: HashMap<String, usize>,
    pub section_scrolls: HashMap<String, usize>,
    pub collapsed_pastes: HashMap<String, String>,
    pub locked_sections: HashSet<String>,
    pub send_choice: SendChoice,
    pub send_at: Option<chrono::NaiveDateTime>,
    /// Which tab was active when the builder was hidden. The Raw buffer itself
    /// rides along inside `sections` under `RAW_SECTION_ID`.
    pub active_tab: PromptTab,
}

impl PromptBuilderSession {
    pub fn from_dialog(dialog: &SimplePromptDialog) -> Self {
        Self {
            sections: dialog.sections.clone(),
            enabled_sections: dialog.enabled_sections.clone(),
            section_counters: dialog.section_counters.clone(),
            section_cursors: dialog.section_cursors.clone(),
            section_scrolls: dialog.section_scrolls.clone(),
            collapsed_pastes: dialog.collapsed_pastes.clone(),
            locked_sections: dialog.locked_sections.clone(),
            send_choice: dialog.send_choice,
            send_at: dialog.send_at,
            active_tab: dialog.active_tab,
        }
    }

    pub fn restore_into(&self, dialog: &mut SimplePromptDialog) {
        dialog.sections = self.sections.clone();
        dialog.enabled_sections = self.enabled_sections.clone();
        dialog.section_counters = self.section_counters.clone();
        dialog.section_cursors = self.section_cursors.clone();
        dialog.section_scrolls = self.section_scrolls.clone();
        dialog.collapsed_pastes = self.collapsed_pastes.clone();
        dialog.locked_sections = self.locked_sections.clone();
        dialog.send_choice = self.send_choice;
        dialog.send_at = self.send_at;
        dialog.active_tab = self.active_tab;
        // A reopened builder always starts in the first section, regardless of
        // where focus sat when the session was captured — the send control is
        // the last stop of the cycle, never the entry point. On the Raw tab
        // that first stop is instead the raw buffer (focus index 1).
        match self.active_tab {
            PromptTab::Raw => dialog.focused_section = 1,
            PromptTab::Normal => dialog.focus_first_section(),
        }
        // Reset transient UI state (not persisted across openings)
        dialog.picker_mode = SectionPickerMode::None;
        dialog.at_picker = None;
        dialog.system_content = None; // re-evaluated on each open
        dialog.send_edit = None;
        dialog.send_error = None;
        dialog.raw_preview = None; // recomputed when the Raw tab is shown
        dialog.scheduled_list_selected = None; // list focus never persists
        dialog.editing_scheduled_id = None; // a reopened builder isn't mid-edit
    }
}

/// JSON-serializable snapshot of the builder's structured fields, persisted
/// per project workdir as `last_prompts.builder_state` (U8) so Ctrl+L can
/// rebuild the builder as it was rather than pasting a flattened blob.
///
/// Deliberately excludes `send_at`: recalling a prompt should not silently
/// re-arm a delivery schedule from a previous session. `picker_mode`,
/// `at_picker`, and `system_content` are transient UI/idempotency state that
/// `PromptBuilderSession::restore_into` also never persists.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct PersistedBuilderState {
    pub sections: HashMap<String, String>,
    pub enabled_sections: Vec<String>,
    pub focused_section: usize,
    pub section_counters: HashMap<String, usize>,
    pub section_cursors: HashMap<String, usize>,
    pub section_scrolls: HashMap<String, usize>,
    pub collapsed_pastes: HashMap<String, String>,
    pub locked_sections: HashSet<String>,
}

impl PersistedBuilderState {
    pub fn from_dialog(dialog: &SimplePromptDialog) -> Self {
        Self {
            sections: dialog.sections.clone(),
            enabled_sections: dialog.enabled_sections.clone(),
            focused_section: dialog.focused_section,
            section_counters: dialog.section_counters.clone(),
            section_cursors: dialog.section_cursors.clone(),
            section_scrolls: dialog.section_scrolls.clone(),
            collapsed_pastes: dialog.collapsed_pastes.clone(),
            locked_sections: dialog.locked_sections.clone(),
        }
    }

    pub fn restore_into(&self, dialog: &mut SimplePromptDialog) {
        dialog.sections = self.sections.clone();
        dialog.enabled_sections = self.enabled_sections.clone();
        dialog.section_counters = self.section_counters.clone();
        dialog.section_cursors = self.section_cursors.clone();
        dialog.section_scrolls = self.section_scrolls.clone();
        dialog.collapsed_pastes = self.collapsed_pastes.clone();
        dialog.locked_sections = self.locked_sections.clone();
        // A recalled prompt opens focused on the first section, ready to type —
        // never on the send control (focus 0), whatever was persisted.
        dialog.focus_first_section();
        dialog.picker_mode = SectionPickerMode::None;
        dialog.at_picker = None;
    }
}

// ── Prompt builder helpers ────────────────────────────────────────
// Skill discovery helpers

fn add_skills_from_dir(
    dir: &std::path::Path,
    prefix: &str,
    out: &mut Vec<(String, String, String)>,
) {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in rd.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let Some(raw_name) = path
            .file_name()
            .and_then(|n| n.to_str())
            .map(str::to_string)
        else {
            continue;
        };
        if crate::skills_module::find_skill_instructions(&path).is_none() {
            continue;
        }
        out.push((format!("skill:{raw_name}"), raw_name, prefix.to_string()));
    }
}

// XML formatting helpers

fn push_xml_item(result: &mut String, tag: &str, content: &str) {
    result.push_str(&format!("  <{tag}>\n"));
    for line in content.lines() {
        result.push_str(&format!("    {line}\n"));
    }
    result.push_str(&format!("  </{tag}>\n\n"));
}

fn format_indexed_xml_section(
    header: &str,
    outer_tag: &str,
    item_tag: &str,
    items: &[String],
) -> Option<String> {
    if items.is_empty() {
        return None;
    }

    let mut result = String::new();
    result.push_str(header);
    result.push_str(&format!("<{outer_tag}>\n"));
    for item in items.iter() {
        push_xml_item(&mut result, item_tag, item);
    }
    result.push_str(&format!("</{outer_tag}>\n\n"));
    Some(result)
}

/// Build a wrapped XML section (header + outer tag + items) from matching section IDs.
fn build_xml_block<'a>(
    result: &mut String,
    sections: &'a [String],
    matches: impl Fn(&str) -> bool,
    content_for: impl Fn(&'a str) -> Option<&'a str>,
    header: &str,
    outer_tag: &str,
    item_tag: &str,
) {
    let mut count = 0;
    for id in sections {
        if !matches(id) {
            continue;
        }
        let Some(content) = content_for(id) else {
            continue;
        };
        let trimmed = content.trim();
        if trimmed.is_empty() {
            continue;
        }
        if count == 0 {
            result.push_str(header);
            result.push_str(&format!("<{outer_tag}>\n"));
        }
        count += 1;
        push_xml_item(result, item_tag, trimmed);
    }
    if count > 0 {
        result.push_str(&format!("</{outer_tag}>\n\n"));
    }
}
