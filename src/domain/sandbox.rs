use std::path::PathBuf;

use anyhow::{bail, Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

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
}
