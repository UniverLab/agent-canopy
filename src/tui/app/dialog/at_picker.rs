use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::time::Instant;

/// Debounce window: the filesystem is searched only after typing pauses this
/// long, so fast typing in a large tree triggers one search, not one per key.
const SEARCH_DEBOUNCE_MS: u128 = 150;
/// Hard ceiling on directory entries examined per search, so a search in a huge
/// tree (e.g. `$HOME`) returns quickly instead of freezing the UI.
const MAX_ENTRIES_SCANNED: usize = 20_000;
/// Depth limit for the breadth-first walk.
const MAX_DEPTH: usize = 8;
/// Maximum matching results collected.
const MAX_RESULTS: usize = 200;

/// Directories ignored when walking for `@` file completion.
pub const AT_IGNORE_DIRS: &[&str] = &[
    ".git",
    ".svn",
    "target",
    "node_modules",
    ".idea",
    ".vscode",
    "build",
    "dist",
    "out",
    "bin",
    "obj",
    "__pycache__",
    ".pytest_cache",
    ".mypy_cache",
    ".tox",
    "venv",
    "env",
    ".venv",
];

/// A single entry shown in the `@`-file picker dropdown.
pub struct AtEntry {
    pub name: String,
    pub path: PathBuf,
    pub is_dir: bool,
}

/// Inline `@`-file picker state for `SimplePromptDialog`.
pub struct AtPicker {
    /// Root workdir — used for computing relative paths.
    pub workdir: PathBuf,
    /// Currently browsed directory (starts at `workdir`).
    pub current_dir: PathBuf,
    /// Filtered + sorted entries (dirs before files).
    pub entries: Vec<AtEntry>,
    /// Selected index into `entries`.
    pub selected: usize,
    /// Text typed after `@` — used for filtering.
    pub query: String,
    /// Char-index of the `@` character in the section text.
    pub trigger_pos: usize,
    /// Selected index at each level of the directory walk, pushed on
    /// `enter_dir` and popped on `go_up` so back-navigation restores the
    /// cursor to the directory that was entered.
    nav_stack: Vec<usize>,
    /// When `Some`, a query edit is waiting for the debounce window (measured
    /// from this instant) to elapse before searching. Resets on each keystroke.
    pending_since: Option<Instant>,
}

impl AtPicker {
    pub fn new(workdir: PathBuf, trigger_pos: usize) -> Self {
        let current_dir = workdir.clone();
        let mut p = Self {
            workdir,
            current_dir,
            entries: Vec::new(),
            selected: 0,
            query: String::new(),
            trigger_pos,
            nav_stack: Vec::new(),
            pending_since: None,
        };
        p.refresh();
        p
    }

    /// Queue a debounced search after a query edit (does not search yet).
    /// Resets the debounce window so the search fires only once typing pauses.
    pub fn queue_search(&mut self) {
        self.pending_since = Some(Instant::now());
    }

    /// Run a pending search if the debounce window has elapsed. Call each tick.
    pub fn tick_search(&mut self) {
        if let Some(since) = self.pending_since {
            if since.elapsed().as_millis() >= SEARCH_DEBOUNCE_MS {
                self.refresh();
            }
        }
    }

    /// Rebuild `entries` from `current_dir` filtered by `query`.
    ///
    /// Results are ordered: directories first, then files — all filtered by `query`.
    /// When a query is active, the search is breadth-first across subdirectories.
    pub fn refresh(&mut self) {
        self.pending_since = None;
        let q = self.query.to_lowercase();
        let mut dirs: Vec<AtEntry> = Vec::new();
        let mut files: Vec<AtEntry> = Vec::new();

        if q.is_empty() {
            // No query — list current directory only (flat browse mode)
            if let Ok(rd) = std::fs::read_dir(&self.current_dir) {
                for entry in rd.flatten() {
                    let path = entry.path();
                    let name = match path.file_name().and_then(|n| n.to_str()) {
                        Some(n) => n.to_string(),
                        None => continue,
                    };
                    if AT_IGNORE_DIRS.contains(&name.as_str()) {
                        continue;
                    }
                    if path.is_dir() {
                        dirs.push(AtEntry {
                            name,
                            path,
                            is_dir: true,
                        });
                    } else {
                        files.push(AtEntry {
                            name,
                            path,
                            is_dir: false,
                        });
                    }
                }
            }
        } else {
            // Query active — breadth-first so the closest (shallowest) matches
            // surface first, and the scan budget keeps it from freezing the UI.
            Self::breadth_first_search(&self.current_dir, &q, &mut dirs, &mut files);
        }

        dirs.sort_by(|a, b| a.name.cmp(&b.name));
        files.sort_by(|a, b| a.name.cmp(&b.name));
        dirs.extend(files);
        self.entries = dirs;
        self.selected = 0;
    }

    /// Breadth-first search for files/dirs matching `q`. Visits the tree level
    /// by level so nearby matches appear first, and stops once it hits the
    /// result cap, the depth limit, or the entry-scan budget — the budget is
    /// what keeps a search in a giant tree from blocking the UI.
    fn breadth_first_search(
        root: &Path,
        q: &str,
        dirs: &mut Vec<AtEntry>,
        files: &mut Vec<AtEntry>,
    ) {
        let mut queue: VecDeque<(PathBuf, usize)> = VecDeque::new();
        queue.push_back((root.to_path_buf(), 0));
        let mut scanned = 0usize;

        while let Some((dir, depth)) = queue.pop_front() {
            if dirs.len() + files.len() >= MAX_RESULTS || scanned >= MAX_ENTRIES_SCANNED {
                break;
            }
            let Ok(rd) = std::fs::read_dir(&dir) else {
                continue;
            };
            for entry in rd.flatten() {
                scanned += 1;
                if dirs.len() + files.len() >= MAX_RESULTS || scanned >= MAX_ENTRIES_SCANNED {
                    break;
                }
                let path = entry.path();
                let name = match path.file_name().and_then(|n| n.to_str()) {
                    Some(n) => n.to_string(),
                    None => continue,
                };
                if AT_IGNORE_DIRS.contains(&name.as_str()) {
                    continue;
                }
                // file_type() avoids an extra stat syscall per entry.
                let is_dir = entry.file_type().map(|ft| ft.is_dir()).unwrap_or(false);
                let matches = name.to_lowercase().contains(q);
                if is_dir {
                    if matches {
                        dirs.push(AtEntry {
                            name,
                            path: path.clone(),
                            is_dir: true,
                        });
                    }
                    // Enqueue for a later level instead of recursing now (BFS).
                    if depth < MAX_DEPTH {
                        queue.push_back((path, depth + 1));
                    }
                } else if matches {
                    files.push(AtEntry {
                        name,
                        path,
                        is_dir: false,
                    });
                }
            }
        }
    }

    /// Navigate into the currently selected directory.
    pub fn enter_dir(&mut self) {
        if let Some(e) = self.entries.get(self.selected) {
            if e.is_dir {
                self.nav_stack.push(self.selected);
                self.current_dir = e.path.clone();
                self.query.clear();
                self.refresh();
            }
        }
    }

    /// Navigate one level up — no upper limit, allows going above `workdir`.
    /// Restores the cursor to the directory just left, if it was reached via
    /// `enter_dir` (i.e. there is a matching entry on `nav_stack`).
    pub fn go_up(&mut self) {
        if let Some(parent) = self.current_dir.parent() {
            self.current_dir = parent.to_path_buf();
            self.query.clear();
            self.refresh();
            if let Some(prev_selected) = self.nav_stack.pop() {
                if prev_selected < self.entries.len() {
                    self.selected = prev_selected;
                }
            }
        }
    }

    /// Path of the selected entry: relative to workdir when inside it, absolute otherwise.
    pub fn relative_path_of_selected(&self) -> Option<String> {
        let e = self.entries.get(self.selected)?;
        if let Ok(rel) = e.path.strip_prefix(&self.workdir) {
            Some(rel.to_string_lossy().replace('\\', "/"))
        } else {
            // Outside workdir — use absolute path so the reference is unambiguous.
            Some(e.path.to_string_lossy().replace('\\', "/"))
        }
    }

    /// Absolute/full path of the selected entry.
    pub fn full_path_of_selected(&self) -> Option<PathBuf> {
        self.entries.get(self.selected).map(|e| e.path.clone())
    }

    /// If the selected entry is a skill, return its instructions file path.
    /// Display title: `@` + current dir (relative inside workdir, absolute outside) + `/` + query.
    pub fn title(&self) -> String {
        let dir_label = if let Ok(rel) = self.current_dir.strip_prefix(&self.workdir) {
            if rel.as_os_str().is_empty() {
                String::new()
            } else {
                format!("{}/", rel.to_string_lossy())
            }
        } else {
            format!("{}/", self.current_dir.to_string_lossy())
        };
        format!("@{}{}", dir_label, self.query)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    #[test]
    fn bfs_finds_matches_across_depths() {
        let tmp = tempdir().unwrap();
        let root = tmp.path();
        fs::write(root.join("alpha_match.txt"), "").unwrap();
        let sub = root.join("sub");
        fs::create_dir(&sub).unwrap();
        fs::write(sub.join("beta_match.txt"), "").unwrap();
        let deep = sub.join("deep");
        fs::create_dir(&deep).unwrap();
        fs::write(deep.join("gamma_match.txt"), "").unwrap();

        let mut dirs = Vec::new();
        let mut files = Vec::new();
        AtPicker::breadth_first_search(root, "match", &mut dirs, &mut files);

        let names: Vec<&str> = files.iter().map(|f| f.name.as_str()).collect();
        assert!(names.contains(&"alpha_match.txt"));
        assert!(names.contains(&"beta_match.txt"));
        assert!(names.contains(&"gamma_match.txt"));
    }

    #[test]
    fn bfs_skips_ignored_directories() {
        let tmp = tempdir().unwrap();
        let root = tmp.path();
        let ignored = root.join("node_modules");
        fs::create_dir(&ignored).unwrap();
        fs::write(ignored.join("ignored_match.txt"), "").unwrap();

        let mut dirs = Vec::new();
        let mut files = Vec::new();
        AtPicker::breadth_first_search(root, "match", &mut dirs, &mut files);

        assert!(dirs.is_empty());
        assert!(files.is_empty());
    }

    #[test]
    fn go_up_restores_cursor_to_the_directory_just_entered() {
        let tmp = tempdir().unwrap();
        let root = tmp.path();
        fs::create_dir(root.join("alpha")).unwrap();
        fs::create_dir(root.join("tui")).unwrap();

        let mut picker = AtPicker::new(root.to_path_buf(), 0);
        // Flat browse mode sorts dirs alphabetically: "alpha" (0), "tui" (1).
        assert_eq!(picker.entries[1].name, "tui");

        picker.selected = 1;
        picker.enter_dir();
        assert_eq!(picker.current_dir, root.join("tui"));

        picker.go_up();
        assert_eq!(picker.current_dir, root);
        assert_eq!(picker.entries[picker.selected].name, "tui");
    }
}
