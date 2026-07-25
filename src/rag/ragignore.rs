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

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    // ── ragignore_path ────────────────────────────────────────────────

    #[test]
    fn ragignore_path_appends_filename() {
        let dir = Path::new("/some/data/dir");
        assert_eq!(ragignore_path(dir), PathBuf::from("/some/data/dir/ragignore"));
    }

    #[test]
    fn ragignore_path_works_with_nested_dirs() {
        let dir = Path::new("/a/b/c");
        assert_eq!(ragignore_path(dir), PathBuf::from("/a/b/c/ragignore"));
    }

    // ── load_patterns ─────────────────────────────────────────────────

    #[test]
    fn load_patterns_returns_defaults_when_file_missing() {
        let tmp = TempDir::new().unwrap();
        let patterns = load_patterns(tmp.path());
        assert_eq!(patterns.len(), DEFAULT_PATTERNS.len());
        for (p, d) in patterns.iter().zip(DEFAULT_PATTERNS) {
            assert_eq!(p, d);
        }
    }

    #[test]
    fn load_patterns_skips_comments_and_blank_lines() {
        let tmp = TempDir::new().unwrap();
        let content = "# first comment\n\n   \n^\\..*\n# another comment\n~$\n";
        std::fs::write(tmp.path().join("ragignore"), content).unwrap();

        let patterns = load_patterns(tmp.path());
        assert_eq!(patterns, vec![r"^\..*", r"~$"]);
    }

    #[test]
    fn load_patterns_trims_whitespace() {
        let tmp = TempDir::new().unwrap();
        let content = "   ^\\.tmp$   \n  ~$  \n";
        std::fs::write(tmp.path().join("ragignore"), content).unwrap();

        let patterns = load_patterns(tmp.path());
        assert_eq!(patterns, vec![r"^\.tmp$", r"~$"]);
    }

    #[test]
    fn load_patterns_returns_empty_when_file_has_only_comments() {
        let tmp = TempDir::new().unwrap();
        let content = "# only comments\n# here\n";
        std::fs::write(tmp.path().join("ragignore"), content).unwrap();

        let patterns = load_patterns(tmp.path());
        assert!(patterns.is_empty());
    }

    #[test]
    fn load_patterns_returns_empty_when_file_is_empty() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("ragignore"), "").unwrap();

        let patterns = load_patterns(tmp.path());
        assert!(patterns.is_empty());
    }

    #[test]
    fn load_patterns_includes_all_default_patterns_after_ensure() {
        let tmp = TempDir::new().unwrap();
        ensure_ragignore(tmp.path());
        let patterns = load_patterns(tmp.path());
        assert!(patterns.len() >= DEFAULT_PATTERNS.len());
        for dp in DEFAULT_PATTERNS {
            assert!(
                patterns.iter().any(|p| p == dp),
                "missing default pattern: {dp}"
            );
        }
    }

    // ── is_ignored ────────────────────────────────────────────────────

    #[test]
    fn is_ignored_matches_hidden_files() {
        let root = Path::new("/root");
        let patterns = vec![r"^\.".to_string()];
        assert!(is_ignored(Path::new("/root/.git/config"), root, &patterns));
        assert!(is_ignored(Path::new("/root/.DS_Store"), root, &patterns));
    }

    #[test]
    fn is_ignored_matches_backup_files() {
        let root = Path::new("/root");
        let patterns = vec![r"~$".to_string()];
        assert!(is_ignored(Path::new("/root/file.txt~"), root, &patterns));
    }

    #[test]
    fn is_ignored_matches_tmp_files() {
        let root = Path::new("/root");
        let patterns = vec![r"\.tmp$".to_string()];
        assert!(is_ignored(Path::new("/root/data.tmp"), root, &patterns));
    }

    #[test]
    fn is_ignored_matches_swap_files() {
        let root = Path::new("/root");
        let patterns = vec![r"\.swp$".to_string()];
        assert!(is_ignored(Path::new("/root/file.swp"), root, &patterns));
    }

    #[test]
    fn is_ignored_returns_false_for_non_matching() {
        let root = Path::new("/root");
        let patterns = vec![r"\.tmp$".to_string()];
        assert!(!is_ignored(Path::new("/root/file.txt"), root, &patterns));
    }

    #[test]
    fn is_ignored_skips_invalid_regex() {
        let root = Path::new("/root");
        let patterns = vec![r"[invalid".to_string(), r"\.tmp$".to_string()];
        // Should not panic; [invalid is skipped, .tmp$ still works.
        assert!(!is_ignored(Path::new("/root/file.txt"), root, &patterns));
        assert!(is_ignored(Path::new("/root/file.tmp"), root, &patterns));
    }

    #[test]
    fn is_ignored_works_with_relative_path_fallback() {
        let root = Path::new("/some/unrelated/root");
        let file = Path::new(".hidden");
        let patterns = vec![r"^\.".to_string()];
        // strip_prefix fails, falls back to the full path string ".hidden"
        assert!(is_ignored(file, root, &patterns));
    }

    #[test]
    fn is_ignored_matches_nested_hidden_dirs_with_broader_pattern() {
        let root = Path::new("/root");
        // A broader pattern that catches .cache anywhere in the path
        let patterns = vec![r"\.cache/".to_string()];
        assert!(is_ignored(
            Path::new("/root/projects/.cache/data"),
            root,
            &patterns,
        ));
    }

    #[test]
    fn is_ignored_multiple_patterns() {
        let root = Path::new("/root");
        let patterns: Vec<String> = DEFAULT_PATTERNS.iter().map(|s| s.to_string()).collect();
        assert!(is_ignored(Path::new("/root/.gitignore"), root, &patterns));
        assert!(is_ignored(Path::new("/root/file.txt~"), root, &patterns));
        assert!(is_ignored(Path::new("/root/file.tmp"), root, &patterns));
        assert!(is_ignored(Path::new("/root/file.swp"), root, &patterns));
        assert!(!is_ignored(Path::new("/root/normal.txt"), root, &patterns));
    }

    // ── ensure_ragignore ──────────────────────────────────────────────

    #[test]
    fn ensure_ragignore_creates_file_when_missing() {
        let tmp = TempDir::new().unwrap();
        let path = ragignore_path(tmp.path());
        assert!(!path.exists());

        ensure_ragignore(tmp.path());
        assert!(path.exists());

        let content = std::fs::read_to_string(&path).unwrap();
        assert!(content.contains(DEFAULT_PATTERNS[0]));
        assert!(content.contains(DEFAULT_PATTERNS[1]));
        assert!(content.contains(DEFAULT_PATTERNS[2]));
        assert!(content.contains(DEFAULT_PATTERNS[3]));
    }

    #[test]
    fn ensure_ragignore_does_not_overwrite_existing_file() {
        let tmp = TempDir::new().unwrap();
        let path = ragignore_path(tmp.path());
        let custom = "# custom patterns\n^custom$\n";
        std::fs::write(&path, custom).unwrap();

        ensure_ragignore(tmp.path());

        let content = std::fs::read_to_string(&path).unwrap();
        assert_eq!(content, custom);
    }

    #[test]
    fn ensure_ragignore_creates_intermediate_directories() {
        let tmp = TempDir::new().unwrap();
        let nested = tmp.path().join("a").join("b").join("c");
        std::fs::create_dir_all(&nested).unwrap();

        ensure_ragignore(&nested);
        assert!(ragignore_path(&nested).exists());
    }

    #[test]
    fn ensure_ragignore_file_starts_with_comment_header() {
        let tmp = TempDir::new().unwrap();
        ensure_ragignore(tmp.path());

        let content = std::fs::read_to_string(ragignore_path(tmp.path())).unwrap();
        let first_line = content.lines().next().unwrap();
        assert!(first_line.starts_with('#'));
    }
}
