//! `.canopy/ragignore` — global exclusion patterns for personal RAG indexing.
//!
//! Lives in the canopy data dir (`~/.canopy/ragignore`).
//! Each non-comment line is a regex pattern matched against the path of a file
//! relative to the personal RAG root.  Lines starting with `#` are comments.

use std::path::{Path, PathBuf};

const DEFAULT_PATTERNS: &[&str] = &[
    r"^\.",    // hidden files / directories (e.g. .git, .DS_Store)
    r"~$",     // editor backup files
    r"\.tmp$", // temp files
    r"\.swp$", // Vim swap files
];

/// Path to the global ragignore file.
pub fn ragignore_path(data_dir: &Path) -> PathBuf {
    data_dir.join("ragignore")
}

/// Ensure the global ragignore exists in `data_dir`.
/// Seeds it with default patterns if it does not exist yet.
pub fn ensure_ragignore(data_dir: &Path) {
    let path = ragignore_path(data_dir);
    if path.exists() {
        return;
    }
    if std::fs::create_dir_all(data_dir).is_err() {
        return;
    }
    let content = format!(
        "# ragignore — regex patterns excluded from personal RAG indexing\n\
         # One regex per line.  Lines starting with # are comments.\n\
         # Patterns are matched against the path relative to the RAG root.\n\n\
         {}\n",
        DEFAULT_PATTERNS.join("\n")
    );
    let _ = std::fs::write(&path, content);
}

/// Load ignore patterns from the global ragignore in `data_dir`.
pub fn load_patterns(data_dir: &Path) -> Vec<String> {
    let path = ragignore_path(data_dir);
    let Ok(content) = std::fs::read_to_string(&path) else {
        return DEFAULT_PATTERNS.iter().map(|s| s.to_string()).collect();
    };
    content
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .map(str::to_string)
        .collect()
}

/// Returns `true` if `file_path` should be excluded given `root` and the
/// loaded regex `patterns`.  Invalid regex patterns are silently skipped.
pub fn is_ignored(file_path: &Path, root: &Path, patterns: &[String]) -> bool {
    let rel = file_path
        .strip_prefix(root)
        .unwrap_or(file_path)
        .to_string_lossy();

    for pattern in patterns {
        let Ok(re) = regex::Regex::new(pattern) else {
            continue;
        };
        if re.is_match(&rel) {
            return true;
        }
    }
    false
}
