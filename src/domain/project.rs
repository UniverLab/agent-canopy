#![allow(dead_code)]
//! Project domain model and workdir_hash utility.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// First 8 hex chars of SHA-256 of the canonical path.
pub fn workdir_hash(canonical_path: &str) -> String {
    let mut h = Sha256::new();
    h.update(canonical_path.as_bytes());
    h.finalize()
        .iter()
        .take(4)
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// Extract a description from README.md content per spec rules:
/// - Not a heading line
/// - Not a badge/URL-only line
/// - At least 20 words
/// - Before the first `##` heading
pub fn extract_readme_description(content: &str) -> Option<String> {
    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("## ") {
            break;
        }
        // Skip headings, blank lines, badge/image lines
        if trimmed.starts_with('#')
            || trimmed.is_empty()
            || trimmed.starts_with("[![")
            || trimmed.starts_with("[!")
            || trimmed.starts_with("![")
        {
            continue;
        }
        if trimmed.split_whitespace().count() >= 20 {
            return Some(trimmed.to_owned());
        }
    }
    None
}

/// A registered project in the Canopy project registry.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Project {
    /// SHA-256(canonical_path)[..8] — primary key.
    pub hash: String,
    /// Canonical (resolved) path of the project root.
    pub path: String,
    /// Display name (directory name by default).
    pub name: String,
    /// Optional description extracted from README or set manually.
    pub description: Option<String>,
    /// Comma-separated tags.
    pub tags: Option<String>,
    /// Unix timestamp of last indexing.
    pub indexed_at: Option<i64>,
    /// Unix timestamp of registration.
    pub created_at: i64,
}

impl Project {
    pub fn new(canonical_path: &str) -> Self {
        let hash = workdir_hash(canonical_path);
        let name = std::path::Path::new(canonical_path)
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| canonical_path.to_owned());
        Self {
            hash,
            path: canonical_path.to_owned(),
            name,
            description: None,
            tags: None,
            indexed_at: None,
            created_at: chrono::Utc::now().timestamp(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn workdir_hash_is_8_hex_chars() {
        let h = workdir_hash("/home/user/my-project");
        assert_eq!(h.len(), 8);
        assert!(h.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn workdir_hash_is_deterministic() {
        assert_eq!(
            workdir_hash("/home/user/proj"),
            workdir_hash("/home/user/proj")
        );
    }

    #[test]
    fn workdir_hash_differs_for_different_paths() {
        assert_ne!(
            workdir_hash("/home/user/proj-a"),
            workdir_hash("/home/user/proj-b")
        );
    }

    #[test]
    fn extract_readme_description_finds_first_long_paragraph() {
        let md = "# Title\n\nShort.\n\nThis is a long enough description that has more than twenty words in it to satisfy the minimum word count requirement for the extractor.\n\n## Section";
        let desc = extract_readme_description(md).unwrap();
        assert!(desc.contains("long enough description"));
    }

    #[test]
    fn extract_readme_description_stops_at_h2() {
        let md = "# Title\n\n## Section\n\nThis paragraph has more than twenty words and should not be returned because it is after the h2 heading boundary.";
        assert!(extract_readme_description(md).is_none());
    }

    #[test]
    fn extract_readme_description_skips_badges() {
        let md = "# Title\n\n[![badge](url)](link)\n\nThis is a real description with more than twenty words that should be returned by the extractor function working correctly.\n\n## Section";
        let desc = extract_readme_description(md).unwrap();
        assert!(desc.contains("real description"));
    }
}
