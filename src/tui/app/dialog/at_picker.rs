use std::path::PathBuf;

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
        };
        p.refresh();
        p
    }

    /// Rebuild `entries` from `current_dir` filtered by `query`.
    ///
    /// Results are ordered: directories first, then files — all filtered by `query`.
    /// When a query is active, search is recursive across all subdirectories.
    pub fn refresh(&mut self) {
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
            // Query active — recursive search across subdirectories
            self.recursive_search(&self.current_dir, &q, &mut dirs, &mut files, 0);
        }

        dirs.sort_by(|a, b| a.name.cmp(&b.name));
        files.sort_by(|a, b| a.name.cmp(&b.name));
        dirs.extend(files);
        self.entries = dirs;
        self.selected = 0;
    }

    /// Recursively search for files/dirs matching `q`, up to a depth limit.
    fn recursive_search(
        &self,
        dir: &std::path::Path,
        q: &str,
        dirs: &mut Vec<AtEntry>,
        files: &mut Vec<AtEntry>,
        depth: usize,
    ) {
        const MAX_DEPTH: usize = 8;
        const MAX_RESULTS: usize = 200;
        if depth > MAX_DEPTH || (dirs.len() + files.len()) >= MAX_RESULTS {
            return;
        }
        let Ok(rd) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in rd.flatten() {
            if dirs.len() + files.len() >= MAX_RESULTS {
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
            let matches = name.to_lowercase().contains(q);
            if path.is_dir() {
                if matches {
                    dirs.push(AtEntry {
                        name,
                        path: path.clone(),
                        is_dir: true,
                    });
                }
                // Always recurse into dirs to find matching files deeper
                self.recursive_search(&path, q, dirs, files, depth + 1);
            } else if matches {
                files.push(AtEntry {
                    name,
                    path,
                    is_dir: false,
                });
            }
        }
    }

    /// Navigate into the currently selected directory.
    pub fn enter_dir(&mut self) {
        if let Some(e) = self.entries.get(self.selected) {
            if e.is_dir {
                self.current_dir = e.path.clone();
                self.query.clear();
                self.refresh();
            }
        }
    }

    /// Navigate one level up — no upper limit, allows going above `workdir`.
    pub fn go_up(&mut self) {
        if let Some(parent) = self.current_dir.parent() {
            self.current_dir = parent.to_path_buf();
            self.query.clear();
            self.refresh();
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
