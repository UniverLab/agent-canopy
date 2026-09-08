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

/// Allowed typed relations between project nodes.
///
/// `contains` is derived from registry paths (never hand-linked);
/// `relates_to` is legacy-accepted but deprecated.
pub const PROJECT_RELATIONS: &[&str] = &[
    "contains",
    "depends_on",
    "complements",
    "extends",
    "publishes",
];
/// Legacy relation still accepted for backwards compatibility.
pub const LEGACY_PROJECT_RELATIONS: &[&str] = &["relates_to"];

/// Lexical validator for project relation names (case-sensitive).
/// Accepts the 5-type vocabulary plus legacy `relates_to`.
pub fn validate_project_relation(relation: &str) -> anyhow::Result<()> {
    if PROJECT_RELATIONS.contains(&relation) || LEGACY_PROJECT_RELATIONS.contains(&relation) {
        Ok(())
    } else {
        anyhow::bail!(
            "unknown project relation '{}'; allowed: contains, depends_on, complements, extends, publishes (legacy: relates_to)",
            relation
        )
    }
}

/// Bare marker file `.canopy-project` in the directory root.
pub fn project_marker_present(dir: &std::path::Path) -> bool {
    dir.join(".canopy-project").exists()
}

/// Policy: auto-registration happens only when the marker is present.
pub fn should_auto_register(dir: &std::path::Path) -> bool {
    project_marker_present(dir)
}

/// Whether remapping a project onto a new path keeps the project's own
/// registry row (nothing was there yet) or folds its dependents into a
/// project that's already registered at that path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RemapKind {
    /// No project is registered at the new path: the project row itself is
    /// updated in place (new hash, new path), keeping its identity.
    Move,
    /// A project already exists at the new path: dependents are reassigned
    /// to that project and the stale (old-path) project row is removed.
    Merge,
}

impl RemapKind {
    /// Decide the kind from a single fact: does a project already exist at
    /// the destination path? Pure — the caller gathers `target_exists` via a
    /// DB lookup.
    pub fn decide(target_exists: bool) -> Self {
        if target_exists {
            Self::Merge
        } else {
            Self::Move
        }
    }
}

/// Per-table row counts a project remap re-keys — every table (besides
/// `projects` itself) whose own column holds this project's workdir path or
/// hash. Mirrors [`crate::domain::clean::HardCascadeCounts`]'s shape/purpose,
/// but for rows a remap updates rather than deletes.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RemapCounts {
    pub interactive_sessions: i64,
    pub terminal_sessions: i64,
    pub loops: i64,
    pub loop_specs: i64,
    pub sync_messages: i64,
    pub sync_locks: i64,
    pub last_prompts: i64,
    pub scheduled_sends: i64,
    pub failed_scheduled_sends: i64,
    pub agents: i64,
    pub intelligence_nodes: i64,
}

impl RemapCounts {
    pub fn total(&self) -> i64 {
        self.interactive_sessions
            + self.terminal_sessions
            + self.loops
            + self.loop_specs
            + self.sync_messages
            + self.sync_locks
            + self.last_prompts
            + self.scheduled_sends
            + self.failed_scheduled_sends
            + self.agents
            + self.intelligence_nodes
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remap_kind_decides_move_when_no_target() {
        assert_eq!(RemapKind::decide(false), RemapKind::Move);
    }

    #[test]
    fn remap_kind_decides_merge_when_target_exists() {
        assert_eq!(RemapKind::decide(true), RemapKind::Merge);
    }

    #[test]
    fn remap_counts_total_sums_every_field() {
        let counts = RemapCounts {
            interactive_sessions: 1,
            terminal_sessions: 2,
            loops: 3,
            loop_specs: 4,
            sync_messages: 5,
            sync_locks: 6,
            last_prompts: 7,
            scheduled_sends: 8,
            failed_scheduled_sends: 9,
            agents: 10,
            intelligence_nodes: 11,
        };
        assert_eq!(counts.total(), 66);
    }

    #[test]
    fn remap_counts_total_zero_when_default() {
        assert_eq!(RemapCounts::default().total(), 0);
    }

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

    #[test]
    fn validate_project_relation_accepts_vocab_and_legacy() {
        for rel in [
            "contains",
            "depends_on",
            "complements",
            "extends",
            "publishes",
            "relates_to",
        ] {
            assert!(
                validate_project_relation(rel).is_ok(),
                "{rel} should be accepted"
            );
        }
    }

    #[test]
    fn validate_project_relation_rejects_unknown_and_case() {
        for rel in [
            "",
            "blocks",
            "contained_by",
            "CONTAINS",
            "Depends_on",
            "related",
        ] {
            let err = validate_project_relation(rel).unwrap_err();
            assert!(
                err.to_string().contains("allowed:"),
                "error should list allowed set, got: {err}"
            );
        }
    }

    #[test]
    fn project_marker_present_false_without_file() {
        let dir = tempfile::tempdir().unwrap();
        assert!(!project_marker_present(dir.path()));
        assert!(!should_auto_register(dir.path()));
    }

    #[test]
    fn project_marker_present_true_with_file() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(".canopy-project"), "").unwrap();
        assert!(project_marker_present(dir.path()));
        assert!(should_auto_register(dir.path()));
    }
}
