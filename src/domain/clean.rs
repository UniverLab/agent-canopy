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
/// running `--hard`. Soft mode only needs the user-visible top-three (the
/// rows a human would scan when judging whether to nuke a project).
#[derive(Debug, Clone, Copy, Default)]
pub struct ProjectDependentCounts {
    pub loops: i64,
    pub interactive_sessions: i64,
    pub terminal_sessions: i64,
}

/// Full row-count breakdown for a project targeted by `--hard`. Beyond the
/// soft-mode trio, this includes every project-scoped table the cascade
/// actually deletes (sessions, prompts, scheduled sends, sync state) plus
/// every row that is auto-cascade-deleted by SQLite's FK rules when the
/// owning loop / interactive_session / intelligence_node is removed (the
/// `loop_*` / `ensemble_*` / `queue_members` / `seed_sessions` /
/// `intelligence_edges` rows). Surfaced in the pre-delete prompt so the
/// reader sees the entire blast radius, not just the rows the cascade
/// driver issues a `DELETE` for.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct HardCascadeCounts {
    // Direct targets (rows whose own `workdir`/`working_dir`/`project_hash`
    // column points at this project).
    pub loops: i64,
    pub interactive_sessions: i64,
    pub terminal_sessions: i64,
    pub last_prompts: i64,
    pub scheduled_sends: i64,
    pub failed_scheduled_sends: i64,
    pub sync_messages: i64,
    pub sync_locks: i64,
    pub intelligence_nodes: i64,
    // Auto-cascade targets (rows removed by FK ON DELETE CASCADE once the
    // direct target above is deleted; counted up front for the prompt).
    pub loop_specs: i64,
    pub loop_nodes: i64,
    pub loop_edges: i64,
    pub loop_runs: i64,
    pub loop_completion_hook_runs: i64,
    pub ensembles: i64,
    pub ensemble_members: i64,
    pub queue_members: i64,
    pub seed_sessions: i64,
    pub intelligence_edges: i64,
}

impl HardCascadeCounts {
    pub fn is_empty(&self) -> bool {
        self.loops == 0
            && self.interactive_sessions == 0
            && self.terminal_sessions == 0
            && self.last_prompts == 0
            && self.scheduled_sends == 0
            && self.failed_scheduled_sends == 0
            && self.sync_messages == 0
            && self.sync_locks == 0
            && self.intelligence_nodes == 0
            && self.loop_specs == 0
            && self.loop_nodes == 0
            && self.loop_edges == 0
            && self.loop_runs == 0
            && self.loop_completion_hook_runs == 0
            && self.ensembles == 0
            && self.ensemble_members == 0
            && self.queue_members == 0
            && self.seed_sessions == 0
            && self.intelligence_edges == 0
    }
}

/// Why a project that *would* have been cascaded was instead skipped.
///
/// Spec C2: a project is never deleted by `--hard` if it has a currently
/// running loop, or an `active`/`resumed` interactive session, even when
/// the workdir is missing. The skip reason is what the prompt reports back
/// (so the user can decide whether to retry after the loop ends).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HardCascadeSkipReason {
    /// At least one loop for this project has `status = 'running'`.
    RunningLoop,
    /// At least one interactive session for this project is `active` or
    /// `resumed`.
    ActiveSession,
}

impl HardCascadeSkipReason {
    pub fn describe(self) -> &'static str {
        match self {
            Self::RunningLoop => "has a running loop",
            Self::ActiveSession => "has an active/resumed interactive session",
        }
    }
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

/// Input for [`plan_hard_cascade`]: a registered project together with the
/// facts (filesystem-existence, dependent-row counts, running/active guard)
/// the caller has already gathered. Same shape as [`ProjectCandidate`]
/// extended with the extra facts `--hard` needs.
#[derive(Debug, Clone)]
pub struct HardCascadeCandidate {
    pub hash: String,
    pub name: String,
    pub path: String,
    pub workdir_exists: bool,
    pub counts: HardCascadeCounts,
    /// `Some(reason)` if this project has a running loop or an
    /// `active`/`resumed` interactive session, in which case the cascade
    /// MUST skip it. The reason is reported in the printed plan so the
    /// user can see why their orphan wasn't eligible.
    pub skip_reason: Option<HardCascadeSkipReason>,
}

/// A project that `--hard` will actually delete (passes every guard and
/// has at least one dependent row to clean — the spec allows deleting a
/// project with zero dependents, since the user asked, but reporting it
/// as a target only when something is going away is more useful).
#[derive(Debug, Clone)]
pub struct HardCascadeTarget {
    pub hash: String,
    pub name: String,
    pub missing_path: String,
    pub counts: HardCascadeCounts,
}

/// A project that `--hard` would have deleted but skipped because of an
/// in-flight guard (running loop / active session).
#[derive(Debug, Clone)]
pub struct HardCascadeSkip {
    pub hash: String,
    pub name: String,
    pub missing_path: String,
    pub reason: HardCascadeSkipReason,
}

/// The full `--hard` plan: a list of projects to delete (with their
/// dependent-row counts) and a list of projects to skip (with reasons).
#[derive(Debug, Clone, Default)]
pub struct HardCascadePlan {
    pub targets: Vec<HardCascadeTarget>,
    pub skips: Vec<HardCascadeSkip>,
}

impl HardCascadePlan {
    pub fn is_empty(&self) -> bool {
        self.targets.is_empty()
    }
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
    /// Total filesystem bytes reclaimed by every file/dir in the plan. This
    /// is *only* the log/terminal/RAG-residue candidates — it says nothing
    /// about database rows, which free pages inside the `.db` file without
    /// shrinking it (see [`should_reclaim`] and `Database::reclaim_space`).
    /// Reported separately from [`Self::deleted_row_count`] so a row count
    /// never gets read as a byte count.
    pub fn reclaimed_bytes(&self) -> u64 {
        self.log_files
            .iter()
            .chain(self.terminal_dirs.iter())
            .chain(self.rag_residue_files.iter())
            .map(|f| f.size_bytes)
            .sum()
    }

    /// Database rows this plan removes (currently just `interactive_sessions`
    /// — soft mode's only row-level deletion; `--hard`'s cascade counts are
    /// tracked separately in [`HardCascadeTarget::counts`]).
    pub fn deleted_row_count(&self) -> usize {
        self.session_ids.len()
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

/// Minimum number of database rows a clean run must have removed before
/// `canopy clean` bothers reclaiming space with a `VACUUM`. A full `VACUUM`
/// rewrites the entire database file and takes an exclusive lock, so
/// running it after deleting a handful of rows would cost far more than it
/// gives back.
pub const RECLAIM_ROW_THRESHOLD: usize = 50;

/// Whether a clean run deleted enough database rows to justify reclaiming
/// space. Pure function over the row count so both the real run and
/// `--dry-run`'s projection agree on the same threshold.
pub fn should_reclaim(rows_deleted: usize) -> bool {
    rows_deleted >= RECLAIM_ROW_THRESHOLD
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

/// Decide which orphaned projects `--hard` will actually delete and which
/// it must skip. A project is included in `targets` iff:
///
/// - its workdir is missing (the only thing `--hard` cleans up), AND
/// - it has no running loop and no `active`/`resumed` session (the
///   in-flight guard: deleting state out from under a live agent would
///   corrupt the run), AND
/// - the caller has at least one row to clean (an orphan with zero
///   dependents is still deletable, but reporting it as a target only
///   when there's something to remove keeps the printed plan honest about
///   blast radius).
///
/// Pure function over the facts in `candidates` — no I/O.
pub fn plan_hard_cascade(candidates: &[HardCascadeCandidate]) -> HardCascadePlan {
    let mut plan = HardCascadePlan::default();
    for c in candidates {
        if c.workdir_exists {
            // Hard mode is orphan-only by design: a project whose workdir
            // still exists is NEVER a target, regardless of what its
            // dependents look like.
            continue;
        }
        if let Some(reason) = c.skip_reason {
            plan.skips.push(HardCascadeSkip {
                hash: c.hash.clone(),
                name: c.name.clone(),
                missing_path: c.path.clone(),
                reason,
            });
            continue;
        }
        if c.counts.is_empty() {
            // Nothing to cascade: don't pretend we will. The project row
            // itself can still be removed by the caller if it wants, but
            // the spec's prompt-and-confirm model is about showing the
            // blast radius, and an empty blast radius is a no-op.
            continue;
        }
        plan.targets.push(HardCascadeTarget {
            hash: c.hash.clone(),
            name: c.name.clone(),
            missing_path: c.path.clone(),
            counts: c.counts,
        });
    }
    plan
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

    fn hard_candidate(
        hash: &str,
        name: &str,
        path: &str,
        workdir_exists: bool,
        counts: HardCascadeCounts,
        skip_reason: Option<HardCascadeSkipReason>,
    ) -> HardCascadeCandidate {
        HardCascadeCandidate {
            hash: hash.to_string(),
            name: name.to_string(),
            path: path.to_string(),
            workdir_exists,
            counts,
            skip_reason,
        }
    }

    #[test]
    fn hard_plan_only_targets_missing_workdirs() {
        let counts = HardCascadeCounts {
            loops: 1,
            ..Default::default()
        };
        let candidates = vec![
            hard_candidate("aaa", "exists", "/exists", true, counts, None),
            hard_candidate("bbb", "missing", "/missing", false, counts, None),
        ];
        let plan = plan_hard_cascade(&candidates);
        assert_eq!(plan.targets.len(), 1);
        assert_eq!(plan.targets[0].hash, "bbb");
        assert!(plan.skips.is_empty());
    }

    #[test]
    fn hard_plan_skips_projects_with_running_loops() {
        let counts = HardCascadeCounts {
            loops: 1,
            ..Default::default()
        };
        let candidates = vec![hard_candidate(
            "running",
            "r",
            "/r",
            false,
            counts,
            Some(HardCascadeSkipReason::RunningLoop),
        )];
        let plan = plan_hard_cascade(&candidates);
        assert!(plan.targets.is_empty());
        assert_eq!(plan.skips.len(), 1);
        assert_eq!(plan.skips[0].reason, HardCascadeSkipReason::RunningLoop);
        assert_eq!(plan.skips[0].hash, "running");
    }

    #[test]
    fn hard_plan_skips_projects_with_active_sessions() {
        let counts = HardCascadeCounts {
            interactive_sessions: 1,
            ..Default::default()
        };
        let candidates = vec![hard_candidate(
            "live",
            "l",
            "/l",
            false,
            counts,
            Some(HardCascadeSkipReason::ActiveSession),
        )];
        let plan = plan_hard_cascade(&candidates);
        assert!(plan.targets.is_empty());
        assert_eq!(plan.skips[0].reason, HardCascadeSkipReason::ActiveSession);
    }

    #[test]
    fn hard_plan_omits_orphans_with_zero_dependents() {
        // An orphan with no dependents and no in-flight guard is not
        // strictly a target (nothing to cascade), so the plan reports
        // nothing for it — the project row itself is still removed by the
        // executor if the caller chose to invoke the cascade unconditionally.
        let candidates = vec![hard_candidate(
            "empty",
            "e",
            "/e",
            false,
            HardCascadeCounts::default(),
            None,
        )];
        let plan = plan_hard_cascade(&candidates);
        assert!(plan.is_empty());
        assert!(plan.skips.is_empty());
    }

    #[test]
    fn hard_plan_reports_all_targets_and_skips_independently() {
        let counts_with_loops = HardCascadeCounts {
            loops: 2,
            interactive_sessions: 1,
            ..Default::default()
        };
        let counts_with_sessions = HardCascadeCounts {
            interactive_sessions: 3,
            ..Default::default()
        };
        let candidates = vec![
            hard_candidate("ok", "ok", "/ok", false, counts_with_loops, None),
            hard_candidate(
                "live",
                "live",
                "/live",
                false,
                counts_with_sessions,
                Some(HardCascadeSkipReason::ActiveSession),
            ),
            hard_candidate("kept", "kept", "/kept", true, counts_with_loops, None),
        ];
        let plan = plan_hard_cascade(&candidates);
        assert_eq!(plan.targets.len(), 1);
        assert_eq!(plan.targets[0].hash, "ok");
        assert_eq!(plan.targets[0].counts.loops, 2);
        assert_eq!(plan.targets[0].counts.interactive_sessions, 1);
        assert_eq!(plan.skips.len(), 1);
        assert_eq!(plan.skips[0].hash, "live");
    }

    #[test]
    fn skip_reason_describe_is_human_readable() {
        assert_eq!(
            HardCascadeSkipReason::RunningLoop.describe(),
            "has a running loop"
        );
        assert_eq!(
            HardCascadeSkipReason::ActiveSession.describe(),
            "has an active/resumed interactive session"
        );
    }

    #[test]
    fn hard_cascade_counts_is_empty_when_all_zero() {
        assert!(HardCascadeCounts::default().is_empty());
        assert!(!HardCascadeCounts {
            loop_edges: 1,
            ..Default::default()
        }
        .is_empty());
    }

    // ── cutoff_timestamp ───────────────────────────────────────────────

    #[test]
    fn cutoff_timestamp_zero_retention() {
        let now = 1_000_000_000_i64;
        assert_eq!(cutoff_timestamp(now, 0), now);
    }

    #[test]
    fn cutoff_timestamp_one_day() {
        let now = 1_000_000_000_i64;
        assert_eq!(cutoff_timestamp(now, 1), now - 86_400);
    }

    #[test]
    fn cutoff_timestamp_large_retention() {
        let now = 1_000_000_000_i64;
        assert_eq!(cutoff_timestamp(now, 365), now - 365 * 86_400);
    }

    // ── reclaimed_bytes ────────────────────────────────────────────────

    #[test]
    fn reclaimed_bytes_sums_all_file_candidates() {
        let plan = CleanPlan {
            session_ids: vec!["s1".to_string()],
            log_files: vec![FileCandidate {
                path: PathBuf::from("/a.log"),
                key: "a".to_string(),
                mtime: 0,
                size_bytes: 100,
            }],
            terminal_dirs: vec![FileCandidate {
                path: PathBuf::from("/b"),
                key: "b".to_string(),
                mtime: 0,
                size_bytes: 200,
            }],
            rag_residue_files: vec![FileCandidate {
                path: PathBuf::from("/c.tmp"),
                key: "c".to_string(),
                mtime: 0,
                size_bytes: 300,
            }],
            orphaned_projects: vec![],
        };
        assert_eq!(plan.reclaimed_bytes(), 600);
    }

    #[test]
    fn reclaimed_bytes_ignores_orphaned_projects() {
        let plan = CleanPlan {
            session_ids: vec![],
            log_files: vec![],
            terminal_dirs: vec![],
            rag_residue_files: vec![],
            orphaned_projects: vec![OrphanProjectReport {
                hash: "h".to_string(),
                name: "n".to_string(),
                missing_path: "/missing".to_string(),
                dependents: ProjectDependentCounts::default(),
            }],
        };
        assert_eq!(plan.reclaimed_bytes(), 0);
    }

    // ── CleanPlan::is_empty with partial data ──────────────────────────

    #[test]
    fn clean_plan_not_empty_when_only_sessions() {
        let plan = CleanPlan {
            session_ids: vec!["s1".to_string()],
            log_files: vec![],
            terminal_dirs: vec![],
            rag_residue_files: vec![],
            orphaned_projects: vec![],
        };
        assert!(!plan.is_empty());
    }

    #[test]
    fn clean_plan_not_empty_when_only_logs() {
        let plan = CleanPlan {
            session_ids: vec![],
            log_files: vec![FileCandidate {
                path: PathBuf::from("/a.log"),
                key: "a".to_string(),
                mtime: 0,
                size_bytes: 10,
            }],
            terminal_dirs: vec![],
            rag_residue_files: vec![],
            orphaned_projects: vec![],
        };
        assert!(!plan.is_empty());
    }

    #[test]
    fn clean_plan_not_empty_when_only_rag_residue() {
        let plan = CleanPlan {
            session_ids: vec![],
            log_files: vec![],
            terminal_dirs: vec![],
            rag_residue_files: vec![FileCandidate {
                path: PathBuf::from("/a.tmp"),
                key: "a".to_string(),
                mtime: 0,
                size_bytes: 5,
            }],
            orphaned_projects: vec![],
        };
        assert!(!plan.is_empty());
    }

    // ── HardCascadePlan::is_empty ──────────────────────────────────────

    #[test]
    fn hard_cascade_plan_empty_when_no_targets() {
        let plan = HardCascadePlan {
            targets: vec![],
            skips: vec![HardCascadeSkip {
                hash: "h".to_string(),
                name: "n".to_string(),
                missing_path: "/p".to_string(),
                reason: HardCascadeSkipReason::RunningLoop,
            }],
        };
        // is_empty only checks targets, not skips
        assert!(plan.is_empty());
    }

    #[test]
    fn hard_cascade_plan_not_empty_with_targets() {
        let plan = HardCascadePlan {
            targets: vec![HardCascadeTarget {
                hash: "h".to_string(),
                name: "n".to_string(),
                missing_path: "/p".to_string(),
                counts: HardCascadeCounts {
                    loops: 1,
                    ..Default::default()
                },
            }],
            skips: vec![],
        };
        assert!(!plan.is_empty());
    }

    // ── HardCascadeCounts edge cases ───────────────────────────────────

    #[test]
    fn hard_cascade_counts_any_nonzero_field_makes_it_not_empty() {
        assert!(!HardCascadeCounts {
            loops: 0,
            interactive_sessions: 0,
            terminal_sessions: 0,
            last_prompts: 1,
            ..Default::default()
        }
        .is_empty());
        assert!(!HardCascadeCounts {
            ensembles: 1,
            ..Default::default()
        }
        .is_empty());
        assert!(!HardCascadeCounts {
            intelligence_edges: 1,
            ..Default::default()
        }
        .is_empty());
    }

    #[test]
    fn hard_cascade_counts_all_fields_zero() {
        let c = HardCascadeCounts {
            loops: 0,
            interactive_sessions: 0,
            terminal_sessions: 0,
            last_prompts: 0,
            scheduled_sends: 0,
            failed_scheduled_sends: 0,
            sync_messages: 0,
            sync_locks: 0,
            intelligence_nodes: 0,
            loop_specs: 0,
            loop_nodes: 0,
            loop_edges: 0,
            loop_runs: 0,
            loop_completion_hook_runs: 0,
            ensembles: 0,
            ensemble_members: 0,
            queue_members: 0,
            seed_sessions: 0,
            intelligence_edges: 0,
        };
        assert!(c.is_empty());
    }

    // ── HardCascadeSkipReason ──────────────────────────────────────────

    #[test]
    fn hard_cascade_skip_reason_eq() {
        assert_eq!(
            HardCascadeSkipReason::RunningLoop,
            HardCascadeSkipReason::RunningLoop
        );
        assert_eq!(
            HardCascadeSkipReason::ActiveSession,
            HardCascadeSkipReason::ActiveSession
        );
        assert_ne!(
            HardCascadeSkipReason::RunningLoop,
            HardCascadeSkipReason::ActiveSession
        );
    }

    #[test]
    fn hard_cascade_skip_reason_debug() {
        let _ = format!("{:?}", HardCascadeSkipReason::RunningLoop);
        let _ = format!("{:?}", HardCascadeSkipReason::ActiveSession);
    }

    // ── plan_session_cleanup edge cases ────────────────────────────────

    #[test]
    fn plan_session_cleanup_empty_input() {
        let plan = plan_session_cleanup(&[], 1_000_000);
        assert!(plan.is_empty());
    }

    #[test]
    fn plan_session_cleanup_keeps_active_even_if_ancient() {
        let sessions = vec![session("s1", "active", 0)];
        let plan = plan_session_cleanup(&sessions, 1_000_000);
        assert!(plan.is_empty());
    }

    #[test]
    fn plan_session_cleanup_keeps_resumed_even_if_ancient() {
        let sessions = vec![session("s1", "resumed", 0)];
        let plan = plan_session_cleanup(&sessions, 1_000_000);
        assert!(plan.is_empty());
    }

    #[test]
    fn plan_session_cleanup_mixed_statuses_and_ages() {
        let sessions = vec![
            session("s1", "completed", 0),
            session("s2", "active", 0),
            session("s3", "orphaned", 0),
            session("s4", "error", 0),
            session("s5", "completed", 1_000_000),
        ];
        let plan = plan_session_cleanup(&sessions, 1_000_000);
        assert_eq!(plan.len(), 3);
        assert!(plan.contains(&"s1".to_string()));
        assert!(plan.contains(&"s3".to_string()));
        assert!(plan.contains(&"s4".to_string()));
    }

    // ── plan_orphan_file_cleanup edge cases ────────────────────────────

    #[test]
    fn plan_orphan_file_cleanup_empty_inputs() {
        let plan = plan_orphan_file_cleanup(&[], &HashSet::new(), 1_000_000);
        assert!(plan.is_empty());
    }

    #[test]
    fn plan_orphan_file_cleanup_all_known_keys() {
        let files = vec![FileCandidate {
            path: PathBuf::from("/a.log"),
            key: "a".to_string(),
            mtime: 0,
            size_bytes: 10,
        }];
        let known: HashSet<String> = ["a".to_string()].into_iter().collect();
        let plan = plan_orphan_file_cleanup(&files, &known, 1_000_000);
        assert!(plan.is_empty());
    }

    #[test]
    fn plan_orphan_file_cleanup_all_recent() {
        let files = vec![FileCandidate {
            path: PathBuf::from("/a.log"),
            key: "unknown".to_string(),
            mtime: 2_000_000,
            size_bytes: 10,
        }];
        let known = HashSet::new();
        let plan = plan_orphan_file_cleanup(&files, &known, 1_000_000);
        assert!(plan.is_empty());
    }

    // ── plan_rag_residue_cleanup edge cases ────────────────────────────

    #[test]
    fn plan_rag_residue_cleanup_empty_input() {
        let plan = plan_rag_residue_cleanup(&[], 1_000_000);
        assert!(plan.is_empty());
    }

    #[test]
    fn plan_rag_residue_cleanup_ignores_non_residue_files() {
        let files = vec![
            FileCandidate {
                path: PathBuf::from("/a.json"),
                key: "a".to_string(),
                mtime: 0,
                size_bytes: 10,
            },
            FileCandidate {
                path: PathBuf::from("/b.lance"),
                key: "b".to_string(),
                mtime: 0,
                size_bytes: 10,
            },
        ];
        let plan = plan_rag_residue_cleanup(&files, 1_000_000);
        assert!(plan.is_empty());
    }

    #[test]
    fn plan_rag_residue_cleanup_matches_partial_extension() {
        let files = vec![
            FileCandidate {
                path: PathBuf::from("/a.tmp"),
                key: "a".to_string(),
                mtime: 0,
                size_bytes: 10,
            },
            FileCandidate {
                path: PathBuf::from("/b.partial"),
                key: "b".to_string(),
                mtime: 0,
                size_bytes: 20,
            },
            FileCandidate {
                path: PathBuf::from("/c.tmpp"),
                key: "c".to_string(),
                mtime: 0,
                size_bytes: 30,
            },
        ];
        let plan = plan_rag_residue_cleanup(&files, 1_000_000);
        // .tmp and .partial match; .tmpp does not
        assert_eq!(plan.len(), 2);
    }

    #[test]
    fn plan_rag_residue_cleanup_recent_files_excluded() {
        let files = vec![FileCandidate {
            path: PathBuf::from("/a.tmp"),
            key: "a".to_string(),
            mtime: 2_000_000,
            size_bytes: 10,
        }];
        let plan = plan_rag_residue_cleanup(&files, 1_000_000);
        assert!(plan.is_empty());
    }

    // ── plan_orphaned_projects edge cases ───────────────────────────────

    #[test]
    fn plan_orphaned_projects_empty_input() {
        let plan = plan_orphaned_projects(&[]);
        assert!(plan.is_empty());
    }

    #[test]
    fn plan_orphaned_projects_all_existing() {
        let candidates = vec![ProjectCandidate {
            hash: "a".to_string(),
            name: "a".to_string(),
            path: "/a".to_string(),
            workdir_exists: true,
            dependents: ProjectDependentCounts::default(),
        }];
        let plan = plan_orphaned_projects(&candidates);
        assert!(plan.is_empty());
    }

    #[test]
    fn plan_orphaned_projects_all_missing() {
        let candidates = vec![
            ProjectCandidate {
                hash: "a".to_string(),
                name: "a".to_string(),
                path: "/a".to_string(),
                workdir_exists: false,
                dependents: ProjectDependentCounts::default(),
            },
            ProjectCandidate {
                hash: "b".to_string(),
                name: "b".to_string(),
                path: "/b".to_string(),
                workdir_exists: false,
                dependents: ProjectDependentCounts::default(),
            },
        ];
        let plan = plan_orphaned_projects(&candidates);
        assert_eq!(plan.len(), 2);
    }

    // ── plan_hard_cascade edge cases ───────────────────────────────────

    #[test]
    fn hard_plan_empty_candidates() {
        let plan = plan_hard_cascade(&[]);
        assert!(plan.is_empty());
        assert!(plan.skips.is_empty());
    }

    #[test]
    fn hard_plan_skips_only_when_counts_nonempty() {
        // Missing workdir but zero counts → not a target (nothing to cascade)
        let candidates = vec![hard_candidate(
            "a",
            "a",
            "/a",
            false,
            HardCascadeCounts::default(),
            None,
        )];
        let plan = plan_hard_cascade(&candidates);
        assert!(plan.is_empty());
        assert!(plan.skips.is_empty());
    }

    #[test]
    fn hard_plan_both_targets_and_skips() {
        let candidates = vec![
            hard_candidate(
                "target",
                "t",
                "/t",
                false,
                HardCascadeCounts {
                    loops: 1,
                    ..Default::default()
                },
                None,
            ),
            hard_candidate(
                "skip",
                "s",
                "/s",
                false,
                HardCascadeCounts {
                    interactive_sessions: 1,
                    ..Default::default()
                },
                Some(HardCascadeSkipReason::ActiveSession),
            ),
            hard_candidate(
                "exists",
                "e",
                "/e",
                true,
                HardCascadeCounts {
                    loops: 1,
                    ..Default::default()
                },
                None,
            ),
        ];
        let plan = plan_hard_cascade(&candidates);
        assert_eq!(plan.targets.len(), 1);
        assert_eq!(plan.targets[0].hash, "target");
        assert_eq!(plan.skips.len(), 1);
        assert_eq!(plan.skips[0].hash, "skip");
    }

    // ── should_reclaim / deleted_row_count ─────────────────────────────

    #[test]
    fn should_reclaim_false_below_threshold() {
        assert!(!should_reclaim(RECLAIM_ROW_THRESHOLD - 1));
    }

    #[test]
    fn should_reclaim_true_at_and_above_threshold() {
        assert!(should_reclaim(RECLAIM_ROW_THRESHOLD));
        assert!(should_reclaim(RECLAIM_ROW_THRESHOLD + 1));
        assert!(should_reclaim(957));
    }

    #[test]
    fn should_reclaim_false_for_zero() {
        assert!(!should_reclaim(0));
    }

    #[test]
    fn deleted_row_count_reflects_session_ids_only() {
        let plan = CleanPlan {
            session_ids: vec!["a".to_string(), "b".to_string(), "c".to_string()],
            log_files: vec![FileCandidate {
                path: PathBuf::from("/a.log"),
                key: "a".to_string(),
                mtime: 0,
                size_bytes: 999,
            }],
            terminal_dirs: vec![],
            rag_residue_files: vec![],
            orphaned_projects: vec![],
        };
        // Bytes from log_files must never leak into the row count.
        assert_eq!(plan.deleted_row_count(), 3);
    }

    // ── is_rag_residue_name ────────────────────────────────────────────

    #[test]
    fn is_rag_residue_name_true_for_tmp_and_partial() {
        assert!(is_rag_residue_name(std::path::Path::new("/a.tmp")));
        assert!(is_rag_residue_name(std::path::Path::new("/a.partial")));
    }

    #[test]
    fn is_rag_residue_name_false_for_other_extensions() {
        assert!(!is_rag_residue_name(std::path::Path::new("/a.json")));
        assert!(!is_rag_residue_name(std::path::Path::new("/a.log")));
        assert!(!is_rag_residue_name(std::path::Path::new("/a.lance")));
    }

    #[test]
    fn is_rag_residue_name_false_for_no_extension() {
        assert!(!is_rag_residue_name(std::path::Path::new("/noext")));
    }
}
