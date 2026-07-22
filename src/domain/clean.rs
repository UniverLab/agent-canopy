//! Pure decision logic for `canopy clean` (soft cleanup, C1).
//!
//! Everything here is a pure function over already-gathered facts (DB rows,
//! stat'd files, filesystem-existence checks) — no I/O. The daemon layer
//! (`daemon::clean_cli`) gathers those facts and executes the resulting
//! [`CleanPlan`]; `--dry-run` is simply printing the plan without calling the
//! executor.

use std::collections::HashSet;
use std::path::PathBuf;

/// `interactive_sessions.status` values soft-clean is ever allowed to
/// remove. `active` and `resumed` must never appear here — a live/resumed
/// session's row disappearing out from under a running TUI or daemon would
/// corrupt in-flight state.
const CLEANABLE_SESSION_STATUSES: &[&str] = &["orphaned", "error", "completed"];

/// File extensions that mark a RAG-ingestion artifact as transient residue
/// (never a name LanceDB's own manifest/data files use), so a file matching
/// one of these is provably safe to remove regardless of live-store state.
const RAG_RESIDUE_EXTENSIONS: &[&str] = &["tmp", "partial"];

/// An `interactive_sessions` row as input to [`plan_session_cleanup`].
#[derive(Debug, Clone)]
pub struct SessionCandidate {
    pub id: String,
    pub status: String,
    /// Unix timestamp of last activity: `exited_at`, falling back to
    /// `started_at` for rows that never recorded an exit.
    pub age_ts: i64,
}

/// An on-disk file or directory as input to [`plan_orphan_file_cleanup`] /
/// [`plan_rag_residue_cleanup`].
#[derive(Debug, Clone)]
pub struct FileCandidate {
    pub path: PathBuf,
    /// Cross-reference key: the agent id for a `logs/<id>.log` file, or the
    /// terminal session name for a `terminals/<name>/` directory.
    pub key: String,
    pub mtime: i64,
    pub size_bytes: u64,
}

/// Row counts that depend on a project's workdir, surfaced in the
/// orphaned-project report so a reader can judge blast radius before ever
/// running the (separate, C2) `--hard` cascade.
#[derive(Debug, Clone, Copy, Default)]
pub struct ProjectDependentCounts {
    pub loops: i64,
    pub interactive_sessions: i64,
    pub terminal_sessions: i64,
}

/// A registered project as input to [`plan_orphaned_projects`]. `workdir_exists`
/// and `dependents` are facts gathered by the caller (a filesystem check and a
/// DB query respectively) — this struct just carries them into the pure
/// decision.
#[derive(Debug, Clone)]
pub struct ProjectCandidate {
    pub hash: String,
    pub name: String,
    pub path: String,
    pub workdir_exists: bool,
    pub dependents: ProjectDependentCounts,
}

/// A project reported as orphaned (soft mode: report only, never deleted).
#[derive(Debug, Clone)]
pub struct OrphanProjectReport {
    pub hash: String,
    pub name: String,
    pub missing_path: String,
    pub dependents: ProjectDependentCounts,
}

/// Everything a `canopy clean` run decided to do (or, under `--dry-run`,
/// decided it *would* do).
#[derive(Debug, Clone, Default)]
pub struct CleanPlan {
    pub session_ids: Vec<String>,
    pub log_files: Vec<FileCandidate>,
    pub terminal_dirs: Vec<FileCandidate>,
    pub rag_residue_files: Vec<FileCandidate>,
    pub orphaned_projects: Vec<OrphanProjectReport>,
}

impl CleanPlan {
    /// Total bytes reclaimed by every file/dir in the plan.
    pub fn reclaimed_bytes(&self) -> u64 {
        self.log_files
            .iter()
            .chain(self.terminal_dirs.iter())
            .chain(self.rag_residue_files.iter())
            .map(|f| f.size_bytes)
            .sum()
    }

    /// Whether the plan deletes anything at all (orphaned-project *reports*
    /// don't count — soft mode never deletes those).
    pub fn is_empty(&self) -> bool {
        self.session_ids.is_empty()
            && self.log_files.is_empty()
            && self.terminal_dirs.is_empty()
            && self.rag_residue_files.is_empty()
    }
}

/// Cutoff timestamp (unix seconds): a row/file whose age is strictly older
/// than this instant is outside the retention window and eligible for
/// deletion. A row/file exactly `retention_days` old is still kept.
pub fn cutoff_timestamp(now_ts: i64, retention_days: u64) -> i64 {
    now_ts - (retention_days as i64) * 86_400
}

/// Which `interactive_sessions` rows are safe to delete: only
/// orphaned/error/completed rows strictly older than `cutoff_ts`. `active`
/// and `resumed` sessions are excluded even if present in the input — this
/// filter is the last line of defense, not the only one (the repository
/// query that produces `sessions` should already exclude them).
pub fn plan_session_cleanup(sessions: &[SessionCandidate], cutoff_ts: i64) -> Vec<String> {
    sessions
        .iter()
        .filter(|s| CLEANABLE_SESSION_STATUSES.contains(&s.status.as_str()))
        .filter(|s| s.age_ts < cutoff_ts)
        .map(|s| s.id.clone())
        .collect()
}

/// Which on-disk files/dirs are orphaned: their cross-reference key has no
/// matching DB row, and they're older than the retention window (a safety
/// margin against a file written moments before its owning row is
/// committed).
pub fn plan_orphan_file_cleanup(
    files: &[FileCandidate],
    known_keys: &HashSet<String>,
    cutoff_ts: i64,
) -> Vec<FileCandidate> {
    files
        .iter()
        .filter(|f| !known_keys.contains(&f.key))
        .filter(|f| f.mtime < cutoff_ts)
        .cloned()
        .collect()
}

/// Which RAG artifacts are safe residue: only files whose name marks them as
/// transient (never a name LanceDB's own files use), older than the
/// retention window.
pub fn plan_rag_residue_cleanup(files: &[FileCandidate], cutoff_ts: i64) -> Vec<FileCandidate> {
    files
        .iter()
        .filter(|f| is_rag_residue_name(&f.path))
        .filter(|f| f.mtime < cutoff_ts)
        .cloned()
        .collect()
}

fn is_rag_residue_name(path: &std::path::Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|ext| RAG_RESIDUE_EXTENSIONS.contains(&ext))
}

/// Which registered projects are orphaned: their workdir no longer exists on
/// disk. Soft mode only reports these — never deletes the project or its
/// dependents.
pub fn plan_orphaned_projects(candidates: &[ProjectCandidate]) -> Vec<OrphanProjectReport> {
    candidates
        .iter()
        .filter(|c| !c.workdir_exists)
        .map(|c| OrphanProjectReport {
            hash: c.hash.clone(),
            name: c.name.clone(),
            missing_path: c.path.clone(),
            dependents: c.dependents,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session(id: &str, status: &str, age_ts: i64) -> SessionCandidate {
        SessionCandidate {
            id: id.to_string(),
            status: status.to_string(),
            age_ts,
        }
    }

    #[test]
    fn retention_boundary_row_at_exactly_n_days_is_kept() {
        let now = 1_000_000_000_i64;
        let retention_days = 7;
        let cutoff = cutoff_timestamp(now, retention_days);
        // Exactly 7 days old: age_ts == cutoff, must be kept (not `< cutoff`).
        let sessions = vec![session("s-boundary", "completed", cutoff)];
        assert!(plan_session_cleanup(&sessions, cutoff).is_empty());
    }

    #[test]
    fn retention_boundary_row_at_n_plus_one_days_is_deleted() {
        let now = 1_000_000_000_i64;
        let retention_days = 7;
        let cutoff = cutoff_timestamp(now, retention_days);
        // One day past the boundary.
        let sessions = vec![session("s-old", "completed", cutoff - 86_400)];
        assert_eq!(plan_session_cleanup(&sessions, cutoff), vec!["s-old"]);
    }

    #[test]
    fn active_and_resumed_sessions_are_never_deleted_regardless_of_age() {
        let cutoff = 1_000_000_000_i64;
        let ancient = cutoff - 365 * 86_400;
        let sessions = vec![
            session("s-active", "active", ancient),
            session("s-resumed", "resumed", ancient),
            session("s-orphaned", "orphaned", ancient),
            session("s-error", "error", ancient),
            session("s-completed", "completed", ancient),
        ];
        let mut deleted = plan_session_cleanup(&sessions, cutoff);
        deleted.sort();
        assert_eq!(deleted, vec!["s-completed", "s-error", "s-orphaned"]);
    }

    #[test]
    fn orphan_file_cleanup_skips_known_keys_and_recent_files() {
        let cutoff = 1_000_000_000_i64;
        let known: HashSet<String> = ["agent-known".to_string()].into_iter().collect();
        let files = vec![
            FileCandidate {
                path: PathBuf::from("/logs/agent-known.log"),
                key: "agent-known".to_string(),
                mtime: cutoff - 86_400,
                size_bytes: 10,
            },
            FileCandidate {
                path: PathBuf::from("/logs/agent-gone.log"),
                key: "agent-gone".to_string(),
                mtime: cutoff - 86_400,
                size_bytes: 20,
            },
            FileCandidate {
                path: PathBuf::from("/logs/agent-too-new.log"),
                key: "agent-too-new".to_string(),
                mtime: cutoff + 86_400,
                size_bytes: 30,
            },
        ];
        let plan = plan_orphan_file_cleanup(&files, &known, cutoff);
        assert_eq!(plan.len(), 1);
        assert_eq!(plan[0].key, "agent-gone");
    }

    #[test]
    fn rag_residue_cleanup_only_matches_temp_like_extensions() {
        let cutoff = 1_000_000_000_i64;
        let files = vec![
            FileCandidate {
                path: PathBuf::from("/rag/leftover.tmp"),
                key: "leftover.tmp".to_string(),
                mtime: cutoff - 86_400,
                size_bytes: 5,
            },
            FileCandidate {
                path: PathBuf::from("/rag/manifest.json"),
                key: "manifest.json".to_string(),
                mtime: cutoff - 86_400,
                size_bytes: 5,
            },
        ];
        let plan = plan_rag_residue_cleanup(&files, cutoff);
        assert_eq!(plan.len(), 1);
        assert_eq!(plan[0].key, "leftover.tmp");
    }

    #[test]
    fn orphaned_project_reported_only_when_workdir_missing() {
        let candidates = vec![
            ProjectCandidate {
                hash: "aaaa".to_string(),
                name: "exists".to_string(),
                path: "/exists".to_string(),
                workdir_exists: true,
                dependents: ProjectDependentCounts::default(),
            },
            ProjectCandidate {
                hash: "bbbb".to_string(),
                name: "missing".to_string(),
                path: "/missing".to_string(),
                workdir_exists: false,
                dependents: ProjectDependentCounts {
                    loops: 2,
                    interactive_sessions: 3,
                    terminal_sessions: 1,
                },
            },
        ];
        let report = plan_orphaned_projects(&candidates);
        assert_eq!(report.len(), 1);
        assert_eq!(report[0].hash, "bbbb");
        assert_eq!(report[0].dependents.loops, 2);
    }

    #[test]
    fn empty_plan_reports_zero_bytes_and_is_empty() {
        let plan = CleanPlan::default();
        assert_eq!(plan.reclaimed_bytes(), 0);
        assert!(plan.is_empty());
    }
}
