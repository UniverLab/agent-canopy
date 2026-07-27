//! Terminal command history — per-session storage with global search for autocomplete.
//!
//! Each terminal session stores its command history in a TOML file at:
//!   `~/.canopy/terminals/<session-name>/history.toml`
//!
//! The autocomplete picker searches across ALL terminal sessions' histories.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Maximum entries per session history file.
const MAX_ENTRIES: usize = 500;
const MAX_SCROLLBACK_LINES: usize = 2000;

// ── Data model ──────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CommandEntry {
    pub cmd: String,
    pub cwd: String,
    pub last_run: DateTime<Utc>,
    pub count: u32,
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct SessionHistory {
    #[serde(default)]
    pub commands: Vec<CommandEntry>,
    #[serde(default)]
    pub scrollback: Vec<String>,
}

impl SessionHistory {
    /// Record a command execution. Increments count if already present (same cmd+cwd).
    pub fn record(&mut self, cmd: &str, cwd: &str) {
        let cmd = cmd.trim();
        if cmd.is_empty() {
            return;
        }
        let now = Utc::now();
        if let Some(entry) = self
            .commands
            .iter_mut()
            .find(|e| e.cmd == cmd && e.cwd == cwd)
        {
            entry.count += 1;
            entry.last_run = now;
        } else {
            self.commands.push(CommandEntry {
                cmd: cmd.to_string(),
                cwd: cwd.to_string(),
                last_run: now,
                count: 1,
            });
        }
        self.enforce_limit();
    }

    /// LRU eviction: keep the most recently used entries up to MAX_ENTRIES.
    fn enforce_limit(&mut self) {
        if self.commands.len() > MAX_ENTRIES {
            self.commands
                .sort_by_key(|entry| std::cmp::Reverse(entry.last_run));
            self.commands.truncate(MAX_ENTRIES);
        }
        if self.scrollback.len() > MAX_SCROLLBACK_LINES {
            self.scrollback
                .drain(0..self.scrollback.len() - MAX_SCROLLBACK_LINES);
        }
    }

    /// Update the scrollback buffer with new lines from the terminal.
    pub fn update_scrollback(&mut self, lines: &[String]) {
        self.scrollback.extend_from_slice(lines);
        if self.scrollback.len() > MAX_SCROLLBACK_LINES * 2 {
            self.scrollback
                .drain(0..self.scrollback.len() - MAX_SCROLLBACK_LINES);
        }
        self.enforce_scrollback_limit();
    }

    fn enforce_scrollback_limit(&mut self) {
        if self.scrollback.len() > MAX_SCROLLBACK_LINES {
            self.scrollback
                .drain(0..self.scrollback.len() - MAX_SCROLLBACK_LINES);
        }
    }

    /// Filter commands matching a prefix (case-insensitive), ordered by count descending.
    pub fn filter(&self, prefix: &str) -> Vec<&CommandEntry> {
        let prefix_lower = prefix.to_lowercase();
        let mut matches: Vec<&CommandEntry> = self
            .commands
            .iter()
            .filter(|e| e.cmd.to_lowercase().starts_with(&prefix_lower))
            .collect();
        matches.sort_by(|a, b| b.count.cmp(&a.count).then(b.last_run.cmp(&a.last_run)));
        matches
    }

    /// First history entry that starts with `prefix` and is longer than
    /// `prefix` (i.e. has something to autocomplete), case-sensitive. Used
    /// for the inline ghost-text suggestion in the warp input box.
    pub fn ghost_suggestion<'a>(&'a self, prefix: &str) -> Option<&'a str> {
        if prefix.is_empty() {
            return None;
        }
        let mut matches: Vec<&CommandEntry> = self
            .commands
            .iter()
            .filter(|e| e.cmd.starts_with(prefix) && e.cmd.len() > prefix.len())
            .collect();
        matches.sort_by(|a, b| b.count.cmp(&a.count).then(b.last_run.cmp(&a.last_run)));
        matches.first().map(|e| e.cmd.as_str())
    }

    /// Get unique CWD paths from the history (for cd picker).
    pub fn known_directories(&self) -> Vec<String> {
        let mut dirs: HashMap<&str, u32> = HashMap::new();
        for entry in &self.commands {
            *dirs.entry(&entry.cwd).or_default() += entry.count;
        }
        let mut sorted: Vec<(&str, u32)> = dirs.into_iter().collect();
        sorted.sort_by_key(|entry| std::cmp::Reverse(entry.1));
        sorted.into_iter().map(|(d, _)| d.to_string()).collect()
    }
}

// ── Persistence ─────────────────────────────────────────────────

fn history_dir(data_dir: &Path) -> PathBuf {
    data_dir.join("terminals")
}

fn global_catalog_path(data_dir: &Path) -> PathBuf {
    history_dir(data_dir).join("global_catalog.toml")
}

/// Load the global command catalog.
pub fn load_global_catalog(data_dir: &Path) -> SessionHistory {
    let path = global_catalog_path(data_dir);
    match fs::read_to_string(&path) {
        Ok(content) => toml::from_str(&content).unwrap_or_default(),
        Err(_) => SessionHistory::default(),
    }
}

/// Record a command to the global catalog (idempotent — deduplicates by cmd).
pub fn record_global_catalog(data_dir: &Path, cmd: &str, cwd: &str) {
    let cmd = cmd.trim();
    if cmd.is_empty() || cmd == "cd" || cmd.starts_with("cd ") || cmd.starts_with("cd\t") {
        return;
    }
    let mut catalog = load_global_catalog(data_dir);
    let now = chrono::Utc::now();
    // Idempotent: match by cmd only (ignore cwd for global catalog)
    if let Some(entry) = catalog.commands.iter_mut().find(|e| e.cmd == cmd) {
        entry.count += 1;
        entry.last_run = now;
        // Update cwd to most recent
        entry.cwd = cwd.to_string();
    } else {
        catalog.commands.push(CommandEntry {
            cmd: cmd.to_string(),
            cwd: cwd.to_string(),
            last_run: now,
            count: 1,
        });
    }
    catalog.enforce_limit();
    let path = global_catalog_path(data_dir);
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    if let Ok(content) = toml::to_string_pretty(&catalog) {
        let _ = fs::write(&path, content);
    }
}

/// Create a suggestion picker from the global catalog.
pub fn from_global_catalog(input: &str, data_dir: &Path, _cwd: &str) -> SuggestionPicker {
    let catalog = load_global_catalog(data_dir);
    let matches = catalog.filter(input);
    let items: Vec<SuggestionItem> = matches
        .into_iter()
        .map(|e| SuggestionItem {
            label: format!("{}  ×{}", e.cmd, e.count),
            text: e.cmd.clone(),
            count: e.count,
        })
        .collect();
    SuggestionPicker {
        input: input.to_string(),
        mode: PickerMode::CommandHistory,
        all_items: items.clone(),
        items,
        selected: 0,
        scroll_offset: 0,
        cd_base_dir: None,
        cd_current_dir: None,
    }
}

fn history_path(data_dir: &Path, session_name: &str) -> PathBuf {
    history_dir(data_dir)
        .join(session_name)
        .join("history.toml")
}

/// Load a session's history from disk.
pub fn load_history(data_dir: &Path, session_name: &str) -> SessionHistory {
    let path = history_path(data_dir, session_name);
    match fs::read_to_string(&path) {
        Ok(content) => toml::from_str(&content).unwrap_or_default(),
        Err(_) => SessionHistory::default(),
    }
}

/// Save a session's history to disk.
pub fn save_history(data_dir: &Path, session_name: &str, history: &SessionHistory) {
    let path = history_path(data_dir, session_name);
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    if let Ok(content) = toml::to_string_pretty(history) {
        let _ = fs::write(&path, content);
    }
}

/// Delete a session's history from disk.
#[allow(dead_code)]
pub fn delete_history(data_dir: &Path, session_name: &str) {
    let path = history_path(data_dir, session_name);
    let _ = fs::remove_file(&path);
    let _ = fs::remove_dir(path.parent().unwrap());
}

/// Load and merge histories from ALL terminal sessions for global search.
pub fn load_all_histories(data_dir: &Path) -> SessionHistory {
    let dir = history_dir(data_dir);
    let mut merged = SessionHistory::default();
    let Ok(entries) = fs::read_dir(&dir) else {
        return merged;
    };
    for entry in entries.flatten() {
        if !entry.path().is_dir() {
            continue;
        }
        let hist_file = entry.path().join("history.toml");
        let Ok(content) = fs::read_to_string(&hist_file) else {
            continue;
        };
        let Ok(hist) = toml::from_str::<SessionHistory>(&content) else {
            continue;
        };
        for cmd in hist.commands {
            merge_history_entry(&mut merged, cmd);
        }
    }
    merged
}

fn merge_history_entry(
    merged: &mut SessionHistory,
    cmd: crate::tui::terminal_history::CommandEntry,
) {
    if let Some(existing) = merged
        .commands
        .iter_mut()
        .find(|e| e.cmd == cmd.cmd && e.cwd == cmd.cwd)
    {
        existing.count += cmd.count;
        if cmd.last_run > existing.last_run {
            existing.last_run = cmd.last_run;
        }
    } else {
        merged.commands.push(cmd);
    }
}

// ── Autocomplete picker state ───────────────────────────────────

/// The suggestion picker shown as an overlay when Tab is pressed.
#[derive(Debug)]
#[allow(dead_code)]
pub struct SuggestionPicker {
    /// Current input text (filters suggestions in real time).
    pub input: String,
    /// Whether we're in cd-directory mode vs command-history mode.
    pub mode: PickerMode,
    /// Filtered suggestion entries.
    pub items: Vec<SuggestionItem>,
    /// All original items (for re-filtering in command-history mode).
    pub all_items: Vec<SuggestionItem>,
    /// Currently highlighted index.
    pub selected: usize,
    /// Scroll offset for windowed rendering (first visible item index).
    pub scroll_offset: usize,
    /// For cd mode: the base directory from which navigation started.
    pub cd_base_dir: Option<PathBuf>,
    /// For cd mode: the current directory being browsed.
    pub cd_current_dir: Option<PathBuf>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PickerMode {
    /// History-based command autocomplete.
    CommandHistory,
    /// Directory picker for `cd`.
    CdDirectory,
}

#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct SuggestionItem {
    /// The text to insert (command or path).
    pub text: String,
    /// Display label (may include count, cwd abbreviation, etc.).
    pub label: String,
    /// Execution count (for sorting/display).
    pub count: u32,
}

impl SuggestionPicker {
    /// Create a new command history picker from session history.
    #[allow(dead_code)]
    pub fn from_history(input: &str, session_history: &SessionHistory, cwd: &str) -> Self {
        let matches = session_history.filter(input);
        let items: Vec<SuggestionItem> = matches
            .into_iter()
            .map(|e| {
                let cwd_display = if e.cwd == cwd {
                    ".".to_string()
                } else {
                    abbreviate_path(&e.cwd)
                };
                SuggestionItem {
                    text: e.cmd.clone(),
                    label: format!("{}  ×{}  cwd:{}", e.cmd, e.count, cwd_display),
                    count: e.count,
                }
            })
            .collect();
        Self {
            input: input.to_string(),
            mode: PickerMode::CommandHistory,
            all_items: items.clone(),
            items,
            selected: 0,
            scroll_offset: 0,
            cd_base_dir: None,
            cd_current_dir: None,
        }
    }

    /// Create a directory picker for `cd` from CWD children + history dirs.
    pub fn for_cd(partial: &str, cwd: &str, global_history: &SessionHistory) -> Self {
        let mut items = list_subdirs(cwd, false);

        // Add history-derived directories (deduplicated, not already in CWD children)
        for dir in global_history.known_directories() {
            if dir == cwd {
                continue;
            }
            let abbreviated = abbreviate_path(&dir);
            if !items.iter().any(|i| i.text == dir || i.text == abbreviated) {
                items.push(SuggestionItem {
                    text: dir,
                    label: abbreviated,
                    count: 0,
                });
            }
        }

        if !partial.is_empty() {
            let partial_lower = partial.to_lowercase();
            items.retain(|i| i.text.to_lowercase().contains(&partial_lower));
        }

        Self {
            input: partial.to_string(),
            mode: PickerMode::CdDirectory,
            all_items: items.clone(),
            items,
            selected: 0,
            scroll_offset: 0,
            cd_base_dir: Some(PathBuf::from(cwd)),
            cd_current_dir: Some(PathBuf::from(cwd)),
        }
    }

    /// Apply a filter to the picker items (command-history mode only).
    pub fn apply_filter(&mut self, filter: &str) {
        let filter_lower = filter.to_lowercase();
        self.items = self
            .all_items
            .iter()
            .filter(|i| {
                i.text.to_lowercase().contains(&filter_lower)
                    || i.label.to_lowercase().contains(&filter_lower)
            })
            .cloned()
            .collect();
        self.selected = 0;
        self.scroll_offset = 0;
    }

    /// Maximum visible items in the picker window.
    const MAX_VISIBLE: usize = 10;

    pub fn move_up(&mut self) {
        if self.selected > 0 {
            self.selected -= 1;
            if self.selected < self.scroll_offset {
                self.scroll_offset = self.selected;
            }
        }
    }

    pub fn move_down(&mut self) {
        if !self.items.is_empty() && self.selected < self.items.len() - 1 {
            self.selected += 1;
            if self.selected >= self.scroll_offset + Self::MAX_VISIBLE {
                self.scroll_offset = self.selected + 1 - Self::MAX_VISIBLE;
            }
        }
    }

    /// Returns the slice of items currently visible in the scroll window.
    pub fn visible_items(&self) -> &[SuggestionItem] {
        let end = (self.scroll_offset + Self::MAX_VISIBLE).min(self.items.len());
        &self.items[self.scroll_offset..end]
    }

    /// Visible count for layout sizing.
    pub fn visible_count(&self) -> usize {
        self.visible_items().len()
    }

    /// Navigate into the selected directory (cd mode only).
    pub fn navigate_into(&mut self, base_cwd: &str) -> Option<String> {
        if self.mode != PickerMode::CdDirectory {
            return None;
        }
        let selected_path = self.selected_text()?.to_string();

        // ".." means go to parent
        if selected_path == ".." {
            return self.navigate_parent(base_cwd);
        }

        let current_dir = self.cd_current_dir.as_ref()?.to_string_lossy().to_string();

        let new_path = if let Some(stripped) = selected_path.strip_prefix("./") {
            format!("{}/{}", current_dir, stripped)
        } else if selected_path.starts_with('/') || selected_path.starts_with('~') {
            selected_path.clone()
        } else {
            format!("{}/{}", current_dir, selected_path)
        };

        let path = PathBuf::from(&new_path);
        if path.is_dir() {
            self.cd_current_dir = Some(path);
            self.refresh_items(base_cwd);
            return Some(new_path);
        }
        None
    }

    /// Navigate to parent directory (cd mode only).
    /// Remembers the directory we came from and positions cursor on it.
    pub fn navigate_parent(&mut self, base_cwd: &str) -> Option<String> {
        if self.mode != PickerMode::CdDirectory {
            return None;
        }
        let current = self.cd_current_dir.as_ref()?;
        // Remember the directory name we're leaving
        let leaving_name = current.file_name().map(|n| n.to_string_lossy().to_string());
        if let Some(parent) = current.parent() {
            if parent.to_string_lossy().is_empty() {
                return None;
            }
            let parent_path = parent.to_path_buf();
            let result = parent_path.to_string_lossy().to_string();
            self.cd_current_dir = Some(parent_path);
            self.refresh_items(base_cwd);
            // Position cursor on the directory we came from
            if let Some(name) = leaving_name {
                let target = format!("./{name}");
                if let Some(idx) = self.items.iter().position(|i| i.text == target) {
                    self.selected = idx;
                    // Adjust scroll to keep selection visible
                    if self.selected >= self.scroll_offset + Self::MAX_VISIBLE {
                        self.scroll_offset = self.selected + 1 - Self::MAX_VISIBLE;
                    } else if self.selected < self.scroll_offset {
                        self.scroll_offset = self.selected;
                    }
                }
            }
            return Some(result);
        }
        None
    }

    /// Refresh the items list based on current cd_current_dir.
    fn refresh_items(&mut self, _base_cwd: &str) {
        if self.mode != PickerMode::CdDirectory {
            return;
        }
        let Some(cwd_path) = self.cd_current_dir.as_ref() else {
            return;
        };
        let Some(base_path) = self.cd_base_dir.as_ref() else {
            return;
        };
        let cwd = cwd_path.to_string_lossy().to_string();

        // Update input to show relative path from base
        self.input = pathdiff::diff_paths(&cwd, base_path)
            .map(|rel| {
                let s = rel.to_string_lossy();
                if s.starts_with("..") {
                    s.to_string()
                } else if s == "." {
                    ".".to_string()
                } else {
                    format!("./{s}")
                }
            })
            .unwrap_or_else(|| abbreviate_path(&cwd));

        self.items = list_subdirs(&cwd, true);
        self.selected = 0;
        self.scroll_offset = 0;
    }

    /// Get the currently selected item's text for insertion.
    pub fn selected_text(&self) -> Option<&str> {
        self.items.get(self.selected).map(|i| i.text.as_str())
    }
}

/// List immediate subdirectories of `dir` as `SuggestionItem`s.
/// `use_indent_label`: if true, labels are `"  name/"` (for refresh_items);
/// otherwise labels are `"./name"` (for for_cd).
fn list_subdirs(dir: &str, use_indent_label: bool) -> Vec<SuggestionItem> {
    let Ok(entries) = fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut items: Vec<SuggestionItem> = entries
        .flatten()
        .filter(|e| e.path().is_dir())
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().to_string();
            if name.starts_with('.') {
                return None;
            }
            let label = if use_indent_label {
                format!("  {name}/")
            } else {
                format!("./{name}")
            };
            Some(SuggestionItem {
                text: format!("./{name}"),
                label,
                count: 0,
            })
        })
        .collect();
    items.sort_by(|a, b| a.text.cmp(&b.text));
    items
}

/// Abbreviate a path (replace home dir with ~).
fn abbreviate_path(path: &str) -> String {
    if let Some(home) = dirs::home_dir() {
        let home_str = home.to_string_lossy();
        if let Some(rest) = path.strip_prefix(home_str.as_ref()) {
            return format!("~{rest}");
        }
    }
    path.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn test_record_and_filter() {
        let mut hist = SessionHistory::default();
        hist.record("cargo build", "/home/user/project");
        hist.record("cargo test", "/home/user/project");
        hist.record("cargo build", "/home/user/project");

        assert_eq!(hist.commands.len(), 2);
        let entry = hist
            .commands
            .iter()
            .find(|e| e.cmd == "cargo build")
            .unwrap();
        assert_eq!(entry.count, 2);

        let matches = hist.filter("cargo");
        assert_eq!(matches.len(), 2);
        // cargo build should be first (higher count)
        assert_eq!(matches[0].cmd, "cargo build");
    }

    #[test]
    fn test_filter_case_insensitive() {
        let mut hist = SessionHistory::default();
        hist.record("Cargo Build", "/tmp");
        let matches = hist.filter("cargo");
        assert_eq!(matches.len(), 1);
    }

    #[test]
    fn ghost_suggestion_returns_most_recent_match() {
        let mut hist = SessionHistory::default();
        hist.record("cargo build --release", "/p");
        hist.record("cargo build", "/p");
        hist.record("cargo test", "/p");
        // All three have count=1. Sort is (count desc, last_run desc), so
        // the most recent run wins — that's "cargo test" (recorded last).
        assert_eq!(hist.ghost_suggestion("cargo"), Some("cargo test"));
    }

    #[test]
    fn ghost_suggestion_prefers_higher_count() {
        let mut hist = SessionHistory::default();
        hist.record("cargo build --release", "/p");
        hist.record("cargo build", "/p");
        hist.record("cargo build", "/p");
        hist.record("cargo test", "/p");
        // cargo build (count=2) beats cargo build --release and cargo
        // test (both count=1) regardless of recency.
        assert_eq!(hist.ghost_suggestion("cargo"), Some("cargo build"));
    }

    #[test]
    fn ghost_suggestion_is_case_sensitive() {
        let mut hist = SessionHistory::default();
        hist.record("Cargo Build", "/tmp");
        // Ghost is case-sensitive so we don't silently rewrite the user's
        // capitalization. The user typed lower-case `cargo`; history has
        // `Cargo Build` — no match.
        assert_eq!(hist.ghost_suggestion("cargo"), None);
        // The exact prefix matches.
        assert_eq!(hist.ghost_suggestion("Cargo"), Some("Cargo Build"));
    }

    #[test]
    fn ghost_suggestion_skips_exact_matches() {
        let mut hist = SessionHistory::default();
        hist.record("cargo", "/p");
        // Input is already exactly the history entry — no suffix to
        // complete, so no ghost.
        assert_eq!(hist.ghost_suggestion("cargo"), None);
    }

    #[test]
    fn ghost_suggestion_empty_input_returns_none() {
        let mut hist = SessionHistory::default();
        hist.record("cargo build", "/p");
        // Empty input has no prefix to extend; surfacing a ghost with no
        // prefix would be noise.
        assert_eq!(hist.ghost_suggestion(""), None);
    }

    #[test]
    fn test_record_empty_ignored() {
        let mut hist = SessionHistory::default();
        hist.record("", "/tmp");
        hist.record("  ", "/tmp");
        assert!(hist.commands.is_empty());
    }

    #[test]
    fn test_known_directories() {
        let mut hist = SessionHistory::default();
        hist.record("ls", "/home/user/a");
        hist.record("ls", "/home/user/a");
        hist.record("pwd", "/home/user/b");

        let dirs = hist.known_directories();
        assert_eq!(dirs[0], "/home/user/a"); // higher count
        assert_eq!(dirs.len(), 2);
    }

    #[test]
    fn test_lru_eviction() {
        let mut hist = SessionHistory::default();
        for i in 0..600 {
            hist.record(&format!("cmd-{i}"), "/tmp");
        }
        assert!(hist.commands.len() <= MAX_ENTRIES);
    }

    #[test]
    fn test_picker_from_history() {
        let mut hist = SessionHistory::default();
        hist.record("cargo build", "/project");
        hist.record("cargo test", "/project");
        hist.record("cargo clippy", "/other");

        let picker = SuggestionPicker::from_history("cargo", &hist, "/project");
        assert_eq!(picker.items.len(), 3);
        assert_eq!(picker.mode, PickerMode::CommandHistory);
    }

    #[test]
    fn abbreviate_path_home_dir() {
        if let Some(home) = dirs::home_dir() {
            let home_str = home.to_string_lossy();
            let full_path = format!("{home_str}/Documents/file.txt");
            let result = abbreviate_path(&full_path);
            assert_eq!(result, "~/Documents/file.txt");
        }
    }

    #[test]
    fn abbreviate_path_not_home() {
        let result = abbreviate_path("/tmp/something");
        assert_eq!(result, "/tmp/something");
    }

    #[test]
    fn abbreviate_path_exact_home() {
        if let Some(home) = dirs::home_dir() {
            let home_str = home.to_string_lossy();
            let result = abbreviate_path(&home_str);
            assert_eq!(result, "~");
        }
    }

    #[test]
    fn filter_empty_history() {
        let hist = SessionHistory::default();
        let matches = hist.filter("anything");
        assert!(matches.is_empty());
    }

    #[test]
    fn filter_no_match() {
        let mut hist = SessionHistory::default();
        hist.record("cargo build", "/tmp");
        let matches = hist.filter("xyz");
        assert!(matches.is_empty());
    }

    #[test]
    fn ghost_suggestion_no_history() {
        let hist = SessionHistory::default();
        assert_eq!(hist.ghost_suggestion("cargo"), None);
    }

    #[test]
    fn known_directories_empty() {
        let hist = SessionHistory::default();
        let dirs = hist.known_directories();
        assert!(dirs.is_empty());
    }

    #[test]
    fn record_updates_last_run() {
        let mut hist = SessionHistory::default();
        hist.record("cmd", "/tmp");
        let first_time = hist.commands[0].last_run;
        std::thread::sleep(Duration::from_millis(10));
        hist.record("cmd", "/tmp");
        let second_time = hist.commands[0].last_run;
        assert!(second_time >= first_time);
    }

    #[test]
    fn filter_partial_match() {
        let mut hist = SessionHistory::default();
        hist.record("cargo build", "/tmp");
        hist.record("cargo test", "/tmp");
        let matches = hist.filter("car");
        assert_eq!(matches.len(), 2);
    }

    #[test]
    fn merge_history_entry_deduplicates() {
        let mut hist = SessionHistory::default();
        hist.record("ls", "/tmp");
        hist.record("ls", "/tmp");
        assert_eq!(hist.commands.len(), 1);
        assert_eq!(hist.commands[0].count, 2);
    }

    #[test]
    fn filter_sorted_by_count_desc() {
        let mut hist = SessionHistory::default();
        hist.record("cmd_a", "/tmp");
        hist.record("cmd_b", "/tmp");
        hist.record("cmd_b", "/tmp");
        hist.record("cmd_b", "/tmp");
        let matches = hist.filter("cmd");
        assert_eq!(matches[0].cmd, "cmd_b");
        assert_eq!(matches[1].cmd, "cmd_a");
    }

    #[test]
    fn test_scrollback_update() {
        let mut hist = SessionHistory::default();
        hist.record("echo hello", "/tmp");
        let scrollback = vec!["line1".to_string(), "line2".to_string()];
        hist.update_scrollback(&scrollback);
        assert_eq!(hist.scrollback.len(), 2);
    }

    #[test]
    fn test_scrollback_limit() {
        let mut hist = SessionHistory::default();
        let scrollback: Vec<String> = (0..2000).map(|i| format!("line {i}")).collect();
        hist.update_scrollback(&scrollback);
        assert!(hist.scrollback.len() <= MAX_SCROLLBACK_LINES);
    }

    // ── Additional edge cases ────────────────────────────────────

    #[test]
    fn record_same_cmd_different_cwd_creates_separate_entries() {
        let mut hist = SessionHistory::default();
        hist.record("ls", "/home/user/a");
        hist.record("ls", "/home/user/b");
        assert_eq!(hist.commands.len(), 2);
    }

    #[test]
    fn filter_exact_match() {
        let mut hist = SessionHistory::default();
        hist.record("cargo", "/tmp");
        let matches = hist.filter("cargo");
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].cmd, "cargo");
    }

    #[test]
    fn filter_no_prefix_match() {
        let mut hist = SessionHistory::default();
        hist.record("cargo build", "/tmp");
        let matches = hist.filter("build");
        assert!(matches.is_empty());
    }

    #[test]
    fn filter_multiple_matches_sorted_by_count() {
        let mut hist = SessionHistory::default();
        hist.record("git status", "/tmp");
        hist.record("git push", "/tmp");
        hist.record("git push", "/tmp");
        let matches = hist.filter("git");
        assert_eq!(matches.len(), 2);
        assert_eq!(matches[0].cmd, "git push");
        assert_eq!(matches[0].count, 2);
    }

    #[test]
    fn ghost_suggestion_longer_prefix() {
        let mut hist = SessionHistory::default();
        hist.record("cargo build --release", "/p");
        hist.record("cargo build", "/p");
        let suggestion = hist.ghost_suggestion("cargo build");
        assert_eq!(suggestion, Some("cargo build --release"));
    }

    #[test]
    fn known_directories_deduplicates_same_dir() {
        let mut hist = SessionHistory::default();
        hist.record("ls", "/tmp");
        hist.record("pwd", "/tmp");
        hist.record("echo", "/tmp");
        let dirs = hist.known_directories();
        assert_eq!(dirs.len(), 1);
        assert_eq!(dirs[0], "/tmp");
    }

    #[test]
    fn known_directories_sorted_by_count() {
        let mut hist = SessionHistory::default();
        hist.record("a", "/dir_a");
        hist.record("b", "/dir_b");
        hist.record("c", "/dir_b");
        let dirs = hist.known_directories();
        assert_eq!(dirs[0], "/dir_b");
        assert_eq!(dirs[1], "/dir_a");
    }

    #[test]
    fn picker_move_up_stays_at_top() {
        let mut hist = SessionHistory::default();
        hist.record("cargo build", "/tmp");
        let mut picker = SuggestionPicker::from_history("cargo", &hist, "/tmp");
        picker.move_up();
        picker.move_up();
        assert_eq!(picker.selected, 0);
    }

    #[test]
    fn picker_move_down_stays_at_bottom() {
        let mut hist = SessionHistory::default();
        hist.record("cargo build", "/tmp");
        let mut picker = SuggestionPicker::from_history("cargo", &hist, "/tmp");
        picker.move_down();
        picker.move_down();
        assert_eq!(picker.selected, 0);
    }

    #[test]
    fn picker_apply_filter() {
        let mut hist = SessionHistory::default();
        hist.record("cargo build", "/tmp");
        hist.record("cargo test", "/tmp");
        hist.record("git push", "/tmp");
        let mut picker = SuggestionPicker::from_history("", &hist, "/tmp");
        assert_eq!(picker.items.len(), 3);
        picker.apply_filter("cargo");
        assert_eq!(picker.items.len(), 2);
    }

    #[test]
    fn picker_visible_items() {
        let mut hist = SessionHistory::default();
        for i in 0..20 {
            hist.record(&format!("cmd-{i}"), "/tmp");
        }
        let picker = SuggestionPicker::from_history("cmd", &hist, "/tmp");
        assert_eq!(picker.visible_items().len(), SuggestionPicker::MAX_VISIBLE);
    }

    #[test]
    fn picker_visible_count() {
        let mut hist = SessionHistory::default();
        hist.record("cargo build", "/tmp");
        let picker = SuggestionPicker::from_history("cargo", &hist, "/tmp");
        assert_eq!(picker.visible_count(), 1);
    }

    #[test]
    fn picker_selected_text() {
        let mut hist = SessionHistory::default();
        hist.record("cargo build", "/tmp");
        let picker = SuggestionPicker::from_history("cargo", &hist, "/tmp");
        assert_eq!(picker.selected_text(), Some("cargo build"));
    }

    #[test]
    fn picker_empty_items_selected_text() {
        let hist = SessionHistory::default();
        let picker = SuggestionPicker::from_history("xyz", &hist, "/tmp");
        assert!(picker.selected_text().is_none());
    }

    #[test]
    fn picker_from_history_cwd_abbreviation() {
        let mut hist = SessionHistory::default();
        hist.record("ls", "/home/user/project");
        let picker = SuggestionPicker::from_history("ls", &hist, "/home/user/project");
        // CWD matches, so abbreviation should be "."
        assert!(picker.items[0].label.contains("."));
    }

    #[test]
    fn picker_from_history_different_cwd_shows_path() {
        let mut hist = SessionHistory::default();
        hist.record("ls", "/home/user/project");
        let picker = SuggestionPicker::from_history("ls", &hist, "/tmp");
        // CWD differs, so abbreviation should show the path
        assert!(!picker.items[0].label.contains("cwd:."));
    }

    #[test]
    fn merge_history_entry_takes_newer_last_run() {
        let mut merged = SessionHistory::default();
        let old_entry = CommandEntry {
            cmd: "ls".to_string(),
            cwd: "/tmp".to_string(),
            last_run: chrono::Utc::now() - chrono::Duration::hours(1),
            count: 1,
        };
        let new_entry = CommandEntry {
            cmd: "ls".to_string(),
            cwd: "/tmp".to_string(),
            last_run: chrono::Utc::now(),
            count: 2,
        };
        merge_history_entry(&mut merged, old_entry);
        merge_history_entry(&mut merged, new_entry);
        assert_eq!(merged.commands.len(), 1);
        assert_eq!(merged.commands[0].count, 3);
    }

    #[test]
    fn merge_history_entry_different_cwd() {
        let mut merged = SessionHistory::default();
        let entry1 = CommandEntry {
            cmd: "ls".to_string(),
            cwd: "/a".to_string(),
            last_run: chrono::Utc::now(),
            count: 1,
        };
        let entry2 = CommandEntry {
            cmd: "ls".to_string(),
            cwd: "/b".to_string(),
            last_run: chrono::Utc::now(),
            count: 2,
        };
        merge_history_entry(&mut merged, entry1);
        merge_history_entry(&mut merged, entry2);
        assert_eq!(merged.commands.len(), 2);
    }

    #[test]
    fn filter_empty_string_matches_all() {
        let mut hist = SessionHistory::default();
        hist.record("cargo build", "/tmp");
        hist.record("git push", "/tmp");
        let matches = hist.filter("");
        assert_eq!(matches.len(), 2);
    }

    #[test]
    fn enforce_limit_reduces_to_max() {
        let mut hist = SessionHistory::default();
        for i in 0..600 {
            hist.record(&format!("cmd-{i}"), "/tmp");
        }
        assert_eq!(hist.commands.len(), MAX_ENTRIES);
    }

    #[test]
    fn scrollback_enforce_limit() {
        let mut hist = SessionHistory::default();
        for _ in 0..3000 {
            hist.scrollback.push("line".to_string());
        }
        hist.enforce_limit();
        assert!(hist.scrollback.len() <= MAX_SCROLLBACK_LINES);
    }

    #[test]
    fn record_whitespace_only_ignored() {
        let mut hist = SessionHistory::default();
        hist.record("  \t  ", "/tmp");
        assert!(hist.commands.is_empty());
    }
}
