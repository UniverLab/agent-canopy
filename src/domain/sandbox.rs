use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::db::sandbox::SandboxRun;

use crate::domain::project::workdir_hash;
use crate::domain::prompts::canopy_dir;

pub fn worktrees_base_dir() -> PathBuf {
    canopy_dir().join("worktrees")
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Sandbox {
    pub id: String,
    pub project_hash: String,
    pub base_branch: String,
    pub sandbox_branch: String,
    pub worktree_path: PathBuf,
    pub cli_name: String,
    pub original_workdir: String,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MergeOutcome {
    CleanMerge,
    ConflictResolution,
    MergeFailed(String),
}

pub fn canopy_protocol_block() -> &'static str {
    "You are operating within the Canopy multi-agent framework. Its MCP tools and \
     skills are how work gets coordinated here — use them proactively, on your own \
     initiative, not only when the user asks.\n\
     \n\
     [START HERE — required]\n\
     Your FIRST action this session, before answering or touching any file, is to call \
     get_tools(scope=\"session_start\"). It returns the workspace brief and the exact \
     tools for the job. Do not skip it.\n\
     \n\
     [USE CANOPY TOOLS AT EVERY STEP]\n\
     - Before editing files: get_tools(scope=\"file_write\", path=\"...\"), then \
     sync_get_context to detect conflicts and sync_declare_intent to claim the work.\n\
     - Before tests/builds: get_tools(scope=\"test_run\"), then sync_broadcast the start \
     and the PASS/FAIL result.\n\
     - When you learn a durable fact or reusable pattern: intelligence_upsert \
     (kind=\"fact\"|\"pattern\") — never leave knowledge only in chat history.\n\
     - Session end: get_tools(scope=\"close_session\") — upsert a kind=\"session\" \
     summary and sync_report_status. The daemon closes missions automatically.\n\
     - Scheduled tasks: report progress with agent_report.\n\
     Prefer Canopy's native intelligence/sync tools over ad-hoc shell when both can do \
     the job.\n\
     \n\
     [SKILLS — always active]\n\
     The `execution-mindset` skill governs how you operate (judgment, \
     verify-before-reporting, security, resourcefulness, token efficiency) and applies to \
     every task. Reach for `architect-mindset` when designing or writing specs, \
     `code-engineering` for code work, and Canopy's own tooling skills \
     (`canopy-intelligence`, `canopy-sync`, `canopy-loop-design`, `canopy-capabilities`) \
     when working this MCP surface. Apply the skills directly — they are the source of \
     truth, not this summary."
}

pub async fn create_sandbox(
    original_workdir: &str,
    cli_name: &str,
    protocol_content: &str,
) -> Result<Sandbox> {
    let project_hash = workdir_hash(original_workdir);
    let sandbox_id = uuid::Uuid::new_v4();
    let short_id = &sandbox_id.to_string()[..8];
    let sandbox_branch = format!("canopy/sandbox-{short_id}");

    let base = worktrees_base_dir().join(&project_hash);
    std::fs::create_dir_all(&base)
        .with_context(|| format!("Failed to create worktrees base dir: {}", base.display()))?;

    let worktree_path = base.join(sandbox_id.to_string());

    let base_branch = get_current_branch(original_workdir)?;

    let output = tokio::process::Command::new("git")
        .args([
            "worktree",
            "add",
            "-b",
            &sandbox_branch,
            &worktree_path.to_string_lossy(),
            &base_branch,
        ])
        .current_dir(original_workdir)
        .output()
        .await
        .context("Failed to execute git worktree add")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("git worktree add failed: {stderr}");
    }

    let instr_filename = crate::domain::nursery::instruction_file_for_cli(cli_name);
    let instr_path = worktree_path.join(instr_filename);

    if let Some(parent) = instr_path.parent() {
        std::fs::create_dir_all(parent).with_context(|| {
            format!(
                "Failed to create instruction parent dir: {}",
                parent.display()
            )
        })?;
    }

    std::fs::write(&instr_path, protocol_content)
        .with_context(|| format!("Failed to write instruction file: {}", instr_path.display()))?;

    // Deliberately NOT committed and NOT `git add`ed. The harness reads its
    // instruction file straight off disk, so an untracked file is enough — and
    // it is the only thing that keeps the protocol out of the merge and out of
    // the user's repository (the whole point of CM6). A per-worktree
    // `info/exclude` entry keeps a broad `git add -A` in a loop node from
    // sweeping it into a commit.
    exclude_from_worktree(&worktree_path, instr_filename).await;

    Ok(Sandbox {
        id: sandbox_id.to_string(),
        project_hash,
        base_branch,
        sandbox_branch,
        worktree_path,
        cli_name: cli_name.to_string(),
        original_workdir: original_workdir.to_string(),
        created_at: Utc::now(),
    })
}

pub async fn remove_sandbox(sandbox: &Sandbox) -> Result<()> {
    // `--force`: the worktree always has at least the uncommitted instruction
    // file (and whatever build artifacts a run left), so a plain `remove`
    // would refuse. It only ever deletes the sandbox worktree and its admin
    // entry — never the origin branch or the user's main checkout.
    let output = tokio::process::Command::new("git")
        .args([
            "worktree",
            "remove",
            "--force",
            &sandbox.worktree_path.to_string_lossy(),
        ])
        .current_dir(&sandbox.original_workdir)
        .output()
        .await
        .context("Failed to execute git worktree remove")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        tracing::warn!("git worktree remove failed (non-fatal): {stderr}");
    }

    let output = tokio::process::Command::new("git")
        .args(["branch", "-D", &sandbox.sandbox_branch])
        .current_dir(&sandbox.original_workdir)
        .output()
        .await
        .context("Failed to execute git branch -D")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        tracing::warn!("git branch -D failed (non-fatal): {stderr}");
    }

    let project_dir = worktrees_base_dir().join(&sandbox.project_hash);
    if project_dir.exists() {
        if let Ok(entries) = std::fs::read_dir(&project_dir) {
            if entries.count() == 0 {
                let _ = std::fs::remove_dir(&project_dir);
            }
        }
    }

    Ok(())
}

pub async fn merge_sandbox(sandbox: &Sandbox) -> Result<MergeOutcome> {
    // The merge lands on the branch the sandbox was created from, in the
    // user's own checkout. Only proceed if that checkout is actually on that
    // branch and has nothing uncommitted — otherwise `git merge` would either
    // land the work on the wrong branch or trample the user's working tree.
    // A refusal here is a spec-sanctioned "failed sandbox, left in place".
    let current = get_current_branch(&sandbox.original_workdir)?;
    if current != sandbox.base_branch {
        return Ok(MergeOutcome::MergeFailed(format!(
            "workdir '{}' is on '{}', not the sandbox's base branch '{}' — sandbox branch '{}' left in place",
            sandbox.original_workdir, current, sandbox.base_branch, sandbox.sandbox_branch
        )));
    }
    if !working_tree_clean(&sandbox.original_workdir).await? {
        return Ok(MergeOutcome::MergeFailed(format!(
            "workdir '{}' has uncommitted changes — merge of sandbox branch '{}' skipped, sandbox left in place",
            sandbox.original_workdir, sandbox.sandbox_branch
        )));
    }

    let output = tokio::process::Command::new("git")
        .args(["merge", &sandbox.sandbox_branch, "--no-edit"])
        .current_dir(&sandbox.original_workdir)
        .output()
        .await
        .context("Failed to execute git merge")?;

    if output.status.success() {
        remove_sandbox(sandbox).await?;
        return Ok(MergeOutcome::CleanMerge);
    }

    let stderr = String::from_utf8_lossy(&output.stderr);
    let stdout = String::from_utf8_lossy(&output.stdout);

    if stderr.contains("CONFLICT") || stdout.contains("CONFLICT") {
        match resolve_merge_conflicts(sandbox).await {
            Ok(()) => {
                remove_sandbox(sandbox).await?;
                Ok(MergeOutcome::ConflictResolution)
            }
            Err(e) => {
                // Never leave the user's repo mid-merge with conflict markers
                // in their files.
                abort_merge(&sandbox.original_workdir).await;
                Ok(MergeOutcome::MergeFailed(format!(
                    "conflict resolution failed ({e}); merge aborted, sandbox branch '{}' left in place",
                    sandbox.sandbox_branch
                )))
            }
        }
    } else {
        Ok(MergeOutcome::MergeFailed(stderr.to_string()))
    }
}

/// Append `pattern` to the worktree's git exclude file so a broad `git add`
/// in a loop/session node cannot pull the (uncommitted) instruction file into
/// a commit. Best-effort: a failure here just loses that one safeguard.
async fn exclude_from_worktree(worktree_path: &std::path::Path, pattern: &str) {
    let Ok(output) = tokio::process::Command::new("git")
        .args(["rev-parse", "--git-path", "info/exclude"])
        .current_dir(worktree_path)
        .output()
        .await
    else {
        return;
    };
    if !output.status.success() {
        return;
    }
    let rel = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if rel.is_empty() {
        return;
    }
    let exclude_path = worktree_path.join(rel);
    if let Some(parent) = exclude_path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let mut existing = std::fs::read_to_string(&exclude_path).unwrap_or_default();
    if !existing.lines().any(|l| l.trim() == pattern) {
        if !existing.is_empty() && !existing.ends_with('\n') {
            existing.push('\n');
        }
        existing.push_str(pattern);
        existing.push('\n');
        let _ = std::fs::write(&exclude_path, existing);
    }
}

async fn working_tree_clean(workdir: &str) -> Result<bool> {
    let output = tokio::process::Command::new("git")
        .args(["status", "--porcelain"])
        .current_dir(workdir)
        .output()
        .await
        .context("Failed to run git status")?;
    Ok(output.status.success() && output.stdout.is_empty())
}

async fn abort_merge(workdir: &str) {
    let _ = tokio::process::Command::new("git")
        .args(["merge", "--abort"])
        .current_dir(workdir)
        .output()
        .await;
}

async fn resolve_merge_conflicts(sandbox: &Sandbox) -> Result<()> {
    let diff_output = tokio::process::Command::new("git")
        .args(["diff", "--name-only", "--diff-filter=U"])
        .current_dir(&sandbox.original_workdir)
        .output()
        .await
        .context("Failed to get conflict list")?;

    let conflicted_files = String::from_utf8_lossy(&diff_output.stdout).to_string();

    let prompt = format!(
        "Merge conflicts detected when merging branch '{}' into '{}'. \
         Conflicted files:\n{}\n\n\
         Resolve the conflicts in each file, then run `git add` on each resolved file \
         and `git commit --no-edit` to complete the merge.",
        sandbox.sandbox_branch, sandbox.base_branch, conflicted_files
    );

    let cli = crate::domain::models::Cli::resolve(Some(&sandbox.cli_name))
        .map_err(|e| anyhow::anyhow!(e))?;
    let strategy = cli.strategy();

    // Resolve the conflict in the user's checkout (where the merge is in
    // progress), but bound it: a hung agent must not wedge the loop dispatch
    // forever with the repo stuck mid-merge.
    let mut cmd = strategy.build_command(&prompt, None, Some(&sandbox.original_workdir))?;
    let output = tokio::time::timeout(std::time::Duration::from_secs(15 * 60), cmd.output())
        .await
        .context("Conflict resolution agent timed out")?
        .context("Failed to spawn conflict resolution agent")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("Conflict resolution agent failed: {stderr}");
    }

    // The agent was told to `git add` + `git commit` the resolution. If the
    // merge is still in progress (MERGE_HEAD present) it did not finish the
    // job — treat that as a failure so the caller aborts rather than removing
    // the worktree over an unfinished merge.
    if std::path::Path::new(&sandbox.original_workdir)
        .join(".git")
        .join("MERGE_HEAD")
        .exists()
    {
        bail!("conflict resolution agent exited without completing the merge");
    }

    Ok(())
}

fn get_current_branch(workdir: &str) -> Result<String> {
    let output = std::process::Command::new("git")
        .args(["rev-parse", "--abbrev-ref", "HEAD"])
        .current_dir(workdir)
        .output()
        .context("Failed to get current branch")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("git rev-parse --abbrev-ref HEAD failed: {stderr}");
    }

    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

// ── CB42: sandbox consolidation (list / land / discard / auto-teardown) ──
//
// `merge_sandbox` / `resolve_merge_conflicts` above are deliberately left
// untouched (a later spec deletes them): they auto-resolve conflicts with an
// agent and force-delete the branch, both of which CB42 forbids on the new
// paths. Everything below refuses rather than forces.

/// One row of `canopy sandbox list`: the stored run plus live git facts.
/// `has_unique_commits` / `behind_count` are `None` when git could not
/// answer (missing repo, missing branch, unparseable output) — callers treat
/// `None` as "uncertain, keep".
#[derive(Debug, Clone, Serialize)]
pub struct SandboxInfo {
    pub id: String,
    pub path: PathBuf,
    pub loop_id: String,
    pub branch: String,
    pub base_branch: String,
    pub original_workdir: String,
    pub status: String,
    pub has_unique_commits: Option<bool>,
    pub behind_count: Option<u64>,
    pub worktree_registered: bool,
}

/// Outcome of [`land_sandbox`]: either the work landed, or it was refused
/// with the reason — never forced, never auto-resolved.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LandOutcome {
    Landed,
    RefusedNonClean(String),
    RefusedWrongBranch(String),
    RefusedConflict(Vec<String>),
}

/// Outcome of [`discard_sandbox`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DiscardOutcome {
    Discarded,
    RefusedUniqueCommits,
}

/// Safety guard: only paths under canopy's own worktrees root, on a
/// `canopy/sandbox-*` branch, are ever touched. Called first by every
/// mutating function below.
pub fn guard_is_managed(path: &Path, branch: &str) -> Result<()> {
    let base = worktrees_base_dir();
    if !path.starts_with(&base) {
        bail!(
            "refusing: worktree path '{}' is outside canopy's worktrees root '{}'",
            path.display(),
            base.display()
        );
    }
    if !branch.starts_with("canopy/sandbox-") {
        bail!(
            "refusing: branch '{branch}' is not a canopy sandbox branch (expected 'canopy/sandbox-*')"
        );
    }
    Ok(())
}

fn rev_list_count(workdir: &str, lhs: &str, rhs: &str) -> Option<u64> {
    let output = std::process::Command::new("git")
        .args(["rev-list", "--count", lhs, "--not", rhs])
        .current_dir(workdir)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    String::from_utf8_lossy(&output.stdout).trim().parse().ok()
}

/// Whether the sandbox branch holds commits that exist nowhere else
/// (compared against the loop's base ref). Any error or unparseable output
/// yields `None` — uncertain means keep. Read-only: never fetches, checks
/// out, or modifies anything.
pub fn has_unique_commits(run: &SandboxRun) -> Option<bool> {
    rev_list_count(&run.original_workdir, &run.sandbox_branch, &run.base_branch).map(|n| n > 0)
}

/// How far behind the loop's base ref the sandbox branch is. `None` means
/// unknown — informational only, never fails the listing.
pub fn behind_count(run: &SandboxRun) -> Option<u64> {
    rev_list_count(&run.original_workdir, &run.base_branch, &run.sandbox_branch)
}

/// Whether the worktree still appears in `git worktree list`. Errors read as
/// `false` (the listing still shows the row; teardown treats unregistered as
/// already gone).
pub fn worktree_is_registered(run: &SandboxRun) -> bool {
    let Ok(output) = std::process::Command::new("git")
        .args(["worktree", "list", "--porcelain"])
        .current_dir(&run.original_workdir)
        .output()
    else {
        return false;
    };
    if !output.status.success() {
        return false;
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    stdout.lines().any(|line| {
        line.strip_prefix("worktree ")
            .is_some_and(|p| p.trim() == run.worktree_path.trim())
    })
}

/// Paths currently in merge conflict in `workdir` (one per line of
/// `git diff --name-only --diff-filter=U`). Must be called while the merge
/// is still in progress — after `merge --abort` the answer is empty.
pub fn conflict_paths(workdir: &str) -> Result<Vec<String>> {
    let output = std::process::Command::new("git")
        .args(["diff", "--name-only", "--diff-filter=U"])
        .current_dir(workdir)
        .output()
        .context("Failed to get conflict list")?;
    Ok(String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(str::to_string)
        .collect())
}

fn git_blocking(args: &[&str], dir: &str) -> Result<std::process::Output> {
    std::process::Command::new("git")
        .args(args)
        .current_dir(dir)
        .output()
        .with_context(|| format!("failed to execute git {}", args.join(" ")))
}

fn working_tree_clean_blocking(workdir: &str) -> Result<bool> {
    let output = git_blocking(&["status", "--porcelain"], workdir)?;
    Ok(output.status.success() && output.stdout.is_empty())
}

fn remove_worktree_blocking(run: &SandboxRun) -> Result<()> {
    // `--force`: the worktree always holds at least the uncommitted
    // instruction file, so a plain `remove` would refuse. This only ever
    // deletes the registered worktree dir — git bookkeeping stays
    // consistent, never a recursive directory delete.
    let output = git_blocking(
        &["worktree", "remove", "--force", &run.worktree_path],
        &run.original_workdir,
    )?;
    if !output.status.success() {
        bail!(
            "git worktree remove failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(())
}

fn delete_branch_blocking(run: &SandboxRun, force: bool) -> Result<()> {
    let flag = if force { "-D" } else { "-d" };
    let output = git_blocking(
        &["branch", flag, &run.sandbox_branch],
        &run.original_workdir,
    )?;
    if !output.status.success() {
        bail!(
            "git branch {flag} {} failed: {}",
            run.sandbox_branch,
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(())
}

fn prune_empty_project_dir(project_hash: &str) {
    let project_dir = worktrees_base_dir().join(project_hash);
    if project_dir.exists() {
        if let Ok(entries) = std::fs::read_dir(&project_dir) {
            if entries.count() == 0 {
                let _ = std::fs::remove_dir(&project_dir);
            }
        }
    }
}

/// Land one sandbox's work onto the loop's base branch. Refuses rather than
/// forcing when the merge is not clean, and names the conflicting paths.
/// Never calls the agent auto-resolver; never force-deletes the branch
/// (a merged branch always deletes cleanly with `-d`).
///
/// Row updates are the caller's job (`merged` on [`LandOutcome::Landed`]);
/// a refusal leaves the row untouched.
pub fn land_sandbox_blocking(run: &SandboxRun) -> Result<LandOutcome> {
    guard_is_managed(Path::new(&run.worktree_path), &run.sandbox_branch)?;
    let current = get_current_branch(&run.original_workdir)?;
    if current != run.base_branch {
        return Ok(LandOutcome::RefusedWrongBranch(format!(
            "workdir '{}' is on '{current}', not the sandbox's base branch '{}' — sandbox branch '{}' left in place",
            run.original_workdir, run.base_branch, run.sandbox_branch
        )));
    }
    if !working_tree_clean_blocking(&run.original_workdir)? {
        return Ok(LandOutcome::RefusedNonClean(format!(
            "workdir '{}' has uncommitted changes — merge of sandbox branch '{}' skipped, sandbox left in place",
            run.original_workdir, run.sandbox_branch
        )));
    }

    let output = git_blocking(
        &["merge", "--no-commit", "--no-ff", &run.sandbox_branch],
        &run.original_workdir,
    )?;
    if output.status.success() {
        let stdout = String::from_utf8_lossy(&output.stdout);
        if !stdout.contains("Already up to date") {
            let commit = git_blocking(&["commit", "--no-edit"], &run.original_workdir)?;
            if !commit.status.success() {
                let _ = git_blocking(&["merge", "--abort"], &run.original_workdir);
                return Ok(LandOutcome::RefusedNonClean(format!(
                    "git commit after merge failed: {}",
                    String::from_utf8_lossy(&commit.stderr).trim()
                )));
            }
        }
        remove_worktree_blocking(run)?;
        delete_branch_blocking(run, false)?;
        return Ok(LandOutcome::Landed);
    }

    let stderr = String::from_utf8_lossy(&output.stderr).to_string();
    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    // Collect paths BEFORE aborting — afterwards the unmerged state is gone.
    let paths = conflict_paths(&run.original_workdir).unwrap_or_default();
    let _ = git_blocking(&["merge", "--abort"], &run.original_workdir);
    if stdout.contains("CONFLICT") || stderr.contains("CONFLICT") || !paths.is_empty() {
        return Ok(LandOutcome::RefusedConflict(paths));
    }
    let detail = if stderr.trim().is_empty() {
        stdout
    } else {
        stderr
    };
    Ok(LandOutcome::RefusedNonClean(detail))
}

pub async fn land_sandbox(run: &SandboxRun) -> Result<LandOutcome> {
    land_sandbox_blocking(run)
}

/// Discard one sandbox: worktree removed via git's own machinery, branch
/// deleted, row update left to the caller (`discarded` on
/// [`DiscardOutcome::Discarded`]).
///
/// Refuses — touching nothing — when the branch holds commits that exist
/// nowhere else (or when that is uncertain), unless `force` was passed
/// explicitly in this call.
pub fn discard_sandbox_blocking(run: &SandboxRun, force: bool) -> Result<DiscardOutcome> {
    guard_is_managed(Path::new(&run.worktree_path), &run.sandbox_branch)?;
    match has_unique_commits(run) {
        Some(true) if !force => return Ok(DiscardOutcome::RefusedUniqueCommits),
        // Uncertain = keep: tidiness never outranks possibly-unique work.
        None if !force => return Ok(DiscardOutcome::RefusedUniqueCommits),
        _ => {}
    }
    if worktree_is_registered(run) {
        remove_worktree_blocking(run)?;
    }
    delete_branch_blocking(run, force)?;
    prune_empty_project_dir(&run.project_hash);
    Ok(DiscardOutcome::Discarded)
}

pub async fn discard_sandbox(run: &SandboxRun, force: bool) -> Result<DiscardOutcome> {
    discard_sandbox_blocking(run, force)
}

/// End-of-run teardown. Never returns `Err`: every failure is recorded on
/// the sandbox row (`cleanup_failed` + `cleanup_error`), never silent.
/// A branch with unique commits is kept (`kept` — the path is already on
/// the row, so it stays findable); only a provably-empty branch is removed
/// (`cleaned`). An uncertain unique-commit answer keeps everything but
/// records `cleanup_failed`, since a cleanup that cannot prove the branch
/// is empty is a failure to clean, not a clean keep.
pub async fn teardown_sandbox_at_end(
    db: &crate::db::Database,
    run: &crate::db::sandbox::SandboxRun,
    reason: &str,
) {
    // Same dual guard as land/discard (constraint: only canopy's own
    // sandboxes are ever touched). A row that fails it is kept and the
    // refusal recorded — never acted on, never silent.
    if let Err(e) = guard_is_managed(Path::new(&run.worktree_path), &run.sandbox_branch) {
        let _ = db.update_sandbox_run_status(&run.id, "cleanup_failed");
        let _ = db.set_sandbox_cleanup_error(
            &run.id,
            &format!(
                "{}: {reason}: refusing unmanaged sandbox ({e:#}); keeping worktree and branch",
                run.worktree_path
            ),
        );
        return;
    }
    match has_unique_commits(run) {
        Some(true) => {
            let _ = db.update_sandbox_run_status(&run.id, "kept");
        }
        Some(false) => {
            let mut detail: Option<String> = None;
            if detail.is_none() && worktree_is_registered(run) {
                if let Err(e) = remove_worktree_blocking(run) {
                    detail = Some(format!("{e:#}"));
                }
            }
            if detail.is_none() {
                if let Err(e) = delete_branch_blocking(run, false) {
                    detail = Some(format!("{e:#}"));
                }
            }
            match detail {
                None => {
                    prune_empty_project_dir(&run.project_hash);
                    let _ = db.update_sandbox_run_status(&run.id, "cleaned");
                }
                Some(detail) => {
                    let _ = db.update_sandbox_run_status(&run.id, "cleanup_failed");
                    let _ = db.set_sandbox_cleanup_error(
                        &run.id,
                        &format!("{}: {reason}: {detail}", run.worktree_path),
                    );
                }
            }
        }
        None => {
            let _ = db.update_sandbox_run_status(&run.id, "cleanup_failed");
            let _ = db.set_sandbox_cleanup_error(
                &run.id,
                &format!(
                    "{}: {reason}: could not determine whether branch '{}' holds commits that exist nowhere else; keeping worktree and branch",
                    run.worktree_path, run.sandbox_branch
                ),
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_worktrees_base_dir_uses_canopy_home() {
        let dir = worktrees_base_dir();
        assert!(dir.ends_with("worktrees"));
        assert!(dir.to_string_lossy().contains(".canopy"));
    }

    #[test]
    fn test_canopy_protocol_block_is_nonempty() {
        let block = canopy_protocol_block();
        assert!(!block.is_empty());
        assert!(block.contains("START HERE"));
        assert!(block.contains("Canopy"));
        assert!(block.contains("SKILLS"));
    }

    #[test]
    fn test_sandbox_branch_name_format() {
        let short_id = "abcd1234";
        let branch = format!("canopy/sandbox-{short_id}");
        assert!(branch.starts_with("canopy/sandbox-"));
        assert_eq!(branch.len(), "canopy/sandbox-".len() + 8);
    }

    fn init_test_repo() -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("create temp dir");
        let run = |args: &[&str]| {
            let output = std::process::Command::new("git")
                .args(args)
                .current_dir(dir.path())
                .env("GIT_CONFIG_NOSYSTEM", "true")
                .env("GIT_AUTHOR_NAME", "Test")
                .env("GIT_AUTHOR_EMAIL", "test@test.com")
                .env("GIT_COMMITTER_NAME", "Test")
                .env("GIT_COMMITTER_EMAIL", "test@test.com")
                .output()
                .expect("git command");
            assert!(
                output.status.success(),
                "git {} failed: {}",
                args.join(" "),
                String::from_utf8_lossy(&output.stderr)
            );
        };
        run(&["init", "-b", "main"]);
        run(&["config", "user.email", "test@test.com"]);
        run(&["config", "user.name", "Test"]);
        std::fs::write(dir.path().join("README.md"), "# Test\n").unwrap();
        run(&["add", "."]);
        run(&["commit", "-m", "initial"]);
        dir
    }

    #[tokio::test]
    async fn test_create_sandbox_creates_worktree_and_instruction_file() {
        let repo = init_test_repo();
        let repo_path = repo.path().to_string_lossy().to_string();

        let sandbox = create_sandbox(&repo_path, "opencode", "PROTOCOL_CONTENT")
            .await
            .expect("create sandbox");

        assert!(sandbox.worktree_path.exists());
        assert!(sandbox.worktree_path.join("AGENTS.md").exists());
        let instr_content =
            std::fs::read_to_string(sandbox.worktree_path.join("AGENTS.md")).unwrap();
        assert_eq!(instr_content, "PROTOCOL_CONTENT");
        assert!(sandbox.sandbox_branch.starts_with("canopy/sandbox-"));
        assert_eq!(sandbox.base_branch, "main");
        assert_eq!(sandbox.cli_name, "opencode");
        assert_eq!(sandbox.original_workdir, repo_path);

        let branch_exists = std::process::Command::new("git")
            .args(["branch", "--list", &sandbox.sandbox_branch])
            .current_dir(&repo_path)
            .output()
            .unwrap();
        let branch_list = String::from_utf8_lossy(&branch_exists.stdout);
        assert!(branch_list.contains(&sandbox.sandbox_branch));

        remove_sandbox(&sandbox).await.ok();
    }

    #[tokio::test]
    async fn test_create_sandbox_uses_correct_instruction_file_per_cli() {
        let repo = init_test_repo();
        let repo_path = repo.path().to_string_lossy().to_string();

        let sb_claude = create_sandbox(&repo_path, "claude", "CLAUDE_PROTOCOL")
            .await
            .expect("create claude sandbox");
        assert!(sb_claude.worktree_path.join("CLAUDE.md").exists());
        remove_sandbox(&sb_claude).await.ok();

        let sb_gemini = create_sandbox(&repo_path, "gemini", "GEMINI_PROTOCOL")
            .await
            .expect("create gemini sandbox");
        assert!(sb_gemini.worktree_path.join("GEMINI.md").exists());
        remove_sandbox(&sb_gemini).await.ok();
    }

    #[tokio::test]
    async fn test_merge_sandbox_clean_path() {
        let repo = init_test_repo();
        let repo_path = repo.path().to_string_lossy().to_string();

        let sandbox = create_sandbox(&repo_path, "opencode", "protocol")
            .await
            .expect("create sandbox");

        std::fs::write(sandbox.worktree_path.join("new_file.txt"), "hello").unwrap();
        let run_git = |args: &[&str]| {
            let output = std::process::Command::new("git")
                .args(args)
                .current_dir(&sandbox.worktree_path)
                .env("GIT_AUTHOR_NAME", "Test")
                .env("GIT_AUTHOR_EMAIL", "test@test.com")
                .env("GIT_COMMITTER_NAME", "Test")
                .env("GIT_COMMITTER_EMAIL", "test@test.com")
                .output()
                .unwrap();
            assert!(output.status.success());
        };
        run_git(&["add", "."]);
        run_git(&["commit", "-m", "add new file"]);

        let outcome = merge_sandbox(&sandbox).await.expect("merge sandbox");
        assert_eq!(outcome, MergeOutcome::CleanMerge);

        assert!(repo.path().join("new_file.txt").exists());
        let content = std::fs::read_to_string(repo.path().join("new_file.txt")).unwrap();
        assert_eq!(content, "hello");

        // The whole point of CM6: the protocol instruction file must not reach
        // the user's repository — not in the tree, not in history.
        assert!(
            !repo.path().join("AGENTS.md").exists(),
            "instruction file leaked into the user's worktree"
        );
        let log = std::process::Command::new("git")
            .args(["log", "--all", "--oneline"])
            .current_dir(repo.path())
            .output()
            .unwrap();
        let log = String::from_utf8_lossy(&log.stdout);
        assert!(
            !log.to_lowercase().contains("canopy"),
            "a canopy commit reached the user's history: {log}"
        );

        assert!(!sandbox.worktree_path.exists());
    }

    #[tokio::test]
    async fn test_merge_sandbox_refuses_dirty_workdir() {
        let repo = init_test_repo();
        let repo_path = repo.path().to_string_lossy().to_string();

        let sandbox = create_sandbox(&repo_path, "opencode", "protocol")
            .await
            .expect("create sandbox");

        std::fs::write(repo.path().join("dirty.txt"), "uncommitted").unwrap();

        let outcome = merge_sandbox(&sandbox).await.expect("merge sandbox");
        assert!(matches!(outcome, MergeOutcome::MergeFailed(_)));

        assert!(sandbox.worktree_path.exists());

        std::fs::remove_file(repo.path().join("dirty.txt")).ok();
        remove_sandbox(&sandbox).await.ok();
    }

    #[tokio::test]
    async fn test_remove_sandbox_cleans_up_worktree_and_branch() {
        let repo = init_test_repo();
        let repo_path = repo.path().to_string_lossy().to_string();

        let sandbox = create_sandbox(&repo_path, "opencode", "protocol")
            .await
            .expect("create sandbox");
        let worktree_path = sandbox.worktree_path.clone();
        let branch = sandbox.sandbox_branch.clone();

        remove_sandbox(&sandbox).await.expect("remove sandbox");

        assert!(!worktree_path.exists());

        let branch_output = std::process::Command::new("git")
            .args(["branch", "--list", &branch])
            .current_dir(&repo_path)
            .output()
            .unwrap();
        let branch_list = String::from_utf8_lossy(&branch_output.stdout);
        assert!(!branch_list.contains(&branch));
    }

    // ── CB42 tests ─────────────────────────────────────────────────────

    fn run_git(dir: &std::path::Path, args: &[&str]) {
        let output = std::process::Command::new("git")
            .args(args)
            .current_dir(dir)
            .env("GIT_AUTHOR_NAME", "Test")
            .env("GIT_AUTHOR_EMAIL", "test@test.com")
            .env("GIT_COMMITTER_NAME", "Test")
            .env("GIT_COMMITTER_EMAIL", "test@test.com")
            .output()
            .expect("git command");
        assert!(
            output.status.success(),
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn commit_file(dir: &std::path::Path, file: &str, contents: &str, msg: &str) {
        std::fs::write(dir.join(file), contents).unwrap();
        run_git(dir, &["add", "."]);
        run_git(dir, &["commit", "-m", msg]);
    }

    fn test_db() -> (tempfile::TempDir, crate::db::Database) {
        let dir = tempfile::tempdir().expect("create temp dir");
        let db = crate::db::Database::new(&dir.path().join("test.db")).expect("open db");
        (dir, db)
    }

    fn insert_test_row(
        db: &crate::db::Database,
        sandbox: &Sandbox,
        owner_id: &str,
    ) -> crate::db::sandbox::SandboxRun {
        db.insert_sandbox_run(sandbox, "loop", owner_id)
            .expect("insert sandbox run");
        db.get_sandbox_run(&sandbox.id)
            .expect("get sandbox run")
            .expect("row present")
    }

    fn branch_exists(repo: &std::path::Path, branch: &str) -> bool {
        let output = std::process::Command::new("git")
            .args(["branch", "--list", branch])
            .current_dir(repo)
            .output()
            .unwrap();
        String::from_utf8_lossy(&output.stdout).contains(branch)
    }

    #[tokio::test]
    async fn teardown_no_unique_commits_removes_worktree_and_branch() {
        let repo = init_test_repo();
        let repo_path = repo.path().to_string_lossy().to_string();
        let sandbox = create_sandbox(&repo_path, "opencode", "protocol")
            .await
            .expect("create sandbox");

        let (_dbdir, db) = test_db();
        let row = insert_test_row(&db, &sandbox, "loop-1");
        assert_eq!(has_unique_commits(&row), Some(false));

        teardown_sandbox_at_end(&db, &row, "completed").await;

        let row = db.get_sandbox_run(&sandbox.id).unwrap().unwrap();
        assert_eq!(row.status, "cleaned");
        assert!(!sandbox.worktree_path.exists());
        assert!(!branch_exists(repo.path(), &sandbox.sandbox_branch));
        // No stale git bookkeeping left behind.
        let prune = std::process::Command::new("git")
            .args(["worktree", "prune", "--dry-run"])
            .current_dir(repo.path())
            .output()
            .unwrap();
        assert!(
            !String::from_utf8_lossy(&prune.stdout).contains(&sandbox.id),
            "stale worktree admin entry remains"
        );
    }

    #[tokio::test]
    async fn teardown_unique_commits_keeps_both_and_records_path() {
        let repo = init_test_repo();
        let repo_path = repo.path().to_string_lossy().to_string();
        let sandbox = create_sandbox(&repo_path, "opencode", "protocol")
            .await
            .expect("create sandbox");
        commit_file(
            &sandbox.worktree_path,
            "work.txt",
            "real work",
            "add real work",
        );

        let (_dbdir, db) = test_db();
        let row = insert_test_row(&db, &sandbox, "loop-2");
        assert_eq!(has_unique_commits(&row), Some(true));

        teardown_sandbox_at_end(&db, &row, "completed").await;

        let row = db.get_sandbox_run(&sandbox.id).unwrap().unwrap();
        assert_eq!(row.status, "kept");
        assert_eq!(row.worktree_path, sandbox.worktree_path.to_string_lossy());
        assert!(sandbox.worktree_path.exists());
        assert!(branch_exists(repo.path(), &sandbox.sandbox_branch));

        remove_sandbox(&sandbox).await.ok();
    }

    #[tokio::test]
    async fn listing_reports_unique_state_both_ways() {
        let repo = init_test_repo();
        let repo_path = repo.path().to_string_lossy().to_string();

        let clean_sb = create_sandbox(&repo_path, "opencode", "protocol")
            .await
            .expect("create clean sandbox");
        let (_dbdir, db) = test_db();
        let clean_row = insert_test_row(&db, &clean_sb, "loop-clean");
        assert_eq!(has_unique_commits(&clean_row), Some(false));
        assert_eq!(behind_count(&clean_row), Some(0));

        let work_sb = create_sandbox(&repo_path, "opencode", "protocol")
            .await
            .expect("create work sandbox");
        commit_file(
            &work_sb.worktree_path,
            "work.txt",
            "real work",
            "add real work",
        );
        let work_row = insert_test_row(&db, &work_sb, "loop-work");
        assert_eq!(has_unique_commits(&work_row), Some(true));
        assert!(behind_count(&work_row).is_some());

        remove_sandbox(&clean_sb).await.ok();
        remove_sandbox(&work_sb).await.ok();
    }

    #[tokio::test]
    async fn land_refuses_on_conflict_and_names_paths() {
        let repo = init_test_repo();
        commit_file(repo.path(), "file.txt", "line1\n", "add file");
        let repo_path = repo.path().to_string_lossy().to_string();

        let sandbox = create_sandbox(&repo_path, "opencode", "protocol")
            .await
            .expect("create sandbox");
        commit_file(
            &sandbox.worktree_path,
            "file.txt",
            "sandbox change\n",
            "sandbox-side change",
        );
        commit_file(repo.path(), "file.txt", "base change\n", "base-side change");

        let (_dbdir, db) = test_db();
        let row = insert_test_row(&db, &sandbox, "loop-land");

        let outcome = land_sandbox(&row).await.expect("land sandbox");
        match outcome {
            LandOutcome::RefusedConflict(paths) => {
                assert!(
                    paths.contains(&"file.txt".to_string()),
                    "conflict paths must name file.txt, got {paths:?}"
                );
            }
            other => panic!("expected RefusedConflict, got {other:?}"),
        }
        // The repo is left exactly as it was: no mid-merge state, branch kept.
        assert!(
            !repo.path().join(".git").join("MERGE_HEAD").exists(),
            "repo left mid-merge"
        );
        assert!(branch_exists(repo.path(), &sandbox.sandbox_branch));

        remove_sandbox(&sandbox).await.ok();
    }

    #[tokio::test]
    async fn discard_refuses_unique_unless_explicit() {
        let repo = init_test_repo();
        let repo_path = repo.path().to_string_lossy().to_string();
        let sandbox = create_sandbox(&repo_path, "opencode", "protocol")
            .await
            .expect("create sandbox");
        commit_file(
            &sandbox.worktree_path,
            "work.txt",
            "real work",
            "add real work",
        );

        let (_dbdir, db) = test_db();
        let row = insert_test_row(&db, &sandbox, "loop-discard");

        let outcome = discard_sandbox(&row, false).await.expect("discard");
        assert_eq!(outcome, DiscardOutcome::RefusedUniqueCommits);
        assert!(sandbox.worktree_path.exists());
        assert!(branch_exists(repo.path(), &sandbox.sandbox_branch));

        let outcome = discard_sandbox(&row, true).await.expect("discard force");
        assert_eq!(outcome, DiscardOutcome::Discarded);
        assert!(!sandbox.worktree_path.exists());
        assert!(!branch_exists(repo.path(), &sandbox.sandbox_branch));
    }

    #[tokio::test]
    async fn failed_cleanup_records_reason() {
        let repo = init_test_repo();
        let repo_path = repo.path().to_string_lossy().to_string();
        let sandbox = create_sandbox(&repo_path, "opencode", "protocol")
            .await
            .expect("create sandbox");
        let worktree_path = sandbox.worktree_path.to_string_lossy().to_string();

        let (_dbdir, db) = test_db();
        let row = insert_test_row(&db, &sandbox, "loop-fail");

        std::fs::remove_dir_all(repo.path()).expect("delete original workdir");

        teardown_sandbox_at_end(&db, &row, "failed: boom").await;

        let row = db.get_sandbox_run(&sandbox.id).unwrap().unwrap();
        assert_eq!(row.status, "cleanup_failed");
        let err = row.cleanup_error.expect("cleanup_error recorded");
        assert!(err.contains(&worktree_path), "error must name path: {err}");
        assert!(
            err.contains("failed: boom"),
            "error must name reason: {err}"
        );
    }
}
