//! CLI handler for `canopy clean` (soft cleanup, C1) and
//! `canopy clean --hard` (orphan-project cascade, C2).
//!
//! Gathers facts from the DB and filesystem, hands them to the pure
//! `domain::clean` decision functions to build a [`CleanPlan`], then either
//! prints it (`--dry-run`) or executes it and prints what happened. Soft
//! mode never touches `active`/`resumed` sessions, never deletes projects,
//! and only reports orphaned projects (missing workdir). `--hard` runs the
//! full soft cleanup first, then prints the orphan-project cascade plan
//! and prompts before deleting each orphan (and every row that references
//! it) unless `--yes` or `--dry-run` is set.

use std::path::Path;

use anyhow::Result;

use crate::db::Database;
use crate::domain::canopy_config::CanopyConfig;
use crate::domain::clean::{
    self, CleanPlan, FileCandidate, HardCascadeCandidate, HardCascadePlan, ProjectCandidate,
};

pub async fn handle_clean_action(
    dry_run: bool,
    older_than: Option<u64>,
    hard: bool,
    yes: bool,
    no_reclaim: bool,
) -> Result<()> {
    let data_dir = crate::ensure_data_dir()?;
    let db_path = data_dir.join("background_agents.db");
    let db = Database::new(&db_path)?;
    let config = CanopyConfig::load(&data_dir);
    let retention_days = older_than.unwrap_or(config.clean.retention_days);
    let now_ts = chrono::Utc::now().timestamp();

    let plan = run_clean(&data_dir, &db, dry_run, retention_days, now_ts)?;
    print_summary(&plan, retention_days, dry_run);

    let mut rows_deleted = plan.deleted_row_count() as u64;
    if hard {
        rows_deleted += run_hard_cascade(&db, dry_run, yes)?;
    }

    reclaim_if_warranted(&db, &data_dir, &db_path, dry_run, no_reclaim, rows_deleted);

    Ok(())
}

/// Gather inputs, build the plan, and (unless `dry_run`) execute it.
/// Takes `data_dir`/`db`/`now_ts` as parameters (rather than resolving them
/// itself) so tests can point it at a scratch directory and an injected
/// clock instead of the real `~/.canopy`.
fn run_clean(
    data_dir: &Path,
    db: &Database,
    dry_run: bool,
    retention_days: u64,
    now_ts: i64,
) -> Result<CleanPlan> {
    let cutoff_ts = clean::cutoff_timestamp(now_ts, retention_days);

    let sessions = db.list_cleanable_interactive_sessions()?;
    let session_ids = clean::plan_session_cleanup(&sessions, cutoff_ts);

    let agent_ids = db.list_agent_ids()?;
    let log_files = scan_log_files(data_dir)?;
    let orphan_logs = clean::plan_orphan_file_cleanup(&log_files, &agent_ids, cutoff_ts);

    let terminal_names = db.list_terminal_session_names()?;
    let terminal_dirs = scan_terminal_dirs(data_dir)?;
    let orphan_terminals =
        clean::plan_orphan_file_cleanup(&terminal_dirs, &terminal_names, cutoff_ts);

    let rag_files = scan_rag_residue(data_dir)?;
    let rag_residue = clean::plan_rag_residue_cleanup(&rag_files, cutoff_ts);

    let mut project_candidates = Vec::new();
    for p in db.list_projects()? {
        let workdir_exists = Path::new(&p.path).exists();
        let dependents = db.project_dependent_counts(&p.path)?;
        project_candidates.push(ProjectCandidate {
            hash: p.hash,
            name: p.name,
            path: p.path,
            workdir_exists,
            dependents,
        });
    }
    let orphaned_projects = clean::plan_orphaned_projects(&project_candidates);

    let plan = CleanPlan {
        session_ids,
        log_files: orphan_logs,
        terminal_dirs: orphan_terminals,
        rag_residue_files: rag_residue,
        orphaned_projects,
    };

    if !dry_run {
        execute_plan(db, &plan)?;
    }

    Ok(plan)
}

/// `--hard` mode: gather the per-project cascade facts, build the
/// [`HardCascadePlan`], print it, and (unless `dry_run`) prompt and
/// execute. Splits the orphan work from `run_clean` so the soft-mode
/// tests don't pay for the extra DB roundtrips when `--hard` isn't set.
/// Returns the number of database rows the cascade removed (real run) or
/// would remove (`--dry-run`'s projection), so the caller can fold it into
/// the total that decides whether reclaiming space is warranted.
fn run_hard_cascade(db: &Database, dry_run: bool, yes: bool) -> Result<u64> {
    let candidates: Vec<HardCascadeCandidate> = db
        .list_projects()?
        .into_iter()
        .map(|p| {
            let workdir_exists = Path::new(&p.path).exists();
            let counts = db.project_hard_cascade_counts(&p.hash, &p.path)?;
            let skip_reason = db.project_hard_cascade_skip_reason(&p.path)?;
            Ok::<_, anyhow::Error>(HardCascadeCandidate {
                hash: p.hash,
                name: p.name,
                path: p.path,
                workdir_exists,
                counts,
                skip_reason,
            })
        })
        .collect::<Result<Vec<_>>>()?;

    let plan = clean::plan_hard_cascade(&candidates);
    print_hard_cascade_plan(&plan, dry_run);

    if plan.targets.is_empty() {
        return Ok(0);
    }

    if dry_run {
        // Plan-only: no prompt, no deletes (spec: "deletes nothing, no
        // confirmation needed"). Project the row count so `--dry-run`
        // reports the same reclaim figures a real run would.
        let projected = plan
            .targets
            .iter()
            .map(|t| (direct_count(&t.counts) + cascade_count(&t.counts)) as u64)
            .sum();
        return Ok(projected);
    }

    if !yes {
        // Interactive confirmation; refuse on no/eof/non-tty.
        let proceed = match prompt_hard_cascade_confirmation(&plan) {
            Ok(value) => value,
            Err(err) => {
                eprintln!("  {err}\n  Aborting --hard: refusing to run without an explicit yes.");
                return Ok(0);
            }
        };
        if !proceed {
            println!("  Aborted by user — nothing deleted.");
            return Ok(0);
        }
    }

    let mut rows_deleted = 0u64;
    for target in &plan.targets {
        match db.cascade_delete_orphan_project(&target.hash, &target.missing_path) {
            Ok(actual) => {
                rows_deleted += (direct_count(&actual) + cascade_count(&actual)) as u64;
                println!(
                    " \x1b[32m✓\x1b[0m  Removed {} ({}): {} direct + {} cascade rows across the project.",
                    target.name,
                    target.hash,
                    direct_count(&actual),
                    cascade_count(&actual),
                );
            }
            Err(err) => {
                eprintln!(
                    " \x1b[31m✗\x1b[0m  Failed to remove {} ({}): {err}",
                    target.name, target.hash
                );
            }
        }
    }
    Ok(rows_deleted)
}

fn direct_count(c: &clean::HardCascadeCounts) -> i64 {
    c.loops
        + c.interactive_sessions
        + c.terminal_sessions
        + c.last_prompts
        + c.scheduled_sends
        + c.failed_scheduled_sends
        + c.sync_messages
        + c.sync_locks
        + c.intelligence_nodes
}

fn cascade_count(c: &clean::HardCascadeCounts) -> i64 {
    c.loop_specs
        + c.loop_nodes
        + c.loop_edges
        + c.loop_runs
        + c.loop_completion_hook_runs
        + c.ensembles
        + c.ensemble_members
        + c.pool_members
        + c.seed_sessions
        + c.intelligence_edges
}

fn prompt_hard_cascade_confirmation(plan: &HardCascadePlan) -> Result<bool> {
    use inquire::Confirm;
    // Default to no so a stray Enter (or a non-tty env) can't accidentally
    // confirm a destructive cascade. Scripts that want unattended deletes
    // must pass `--yes`.
    let prompt = format!(
        "Delete these {} orphaned project(s) and every row that references them?",
        plan.targets.len()
    );
    Confirm::new(&prompt)
        .with_default(false)
        .with_help_message("y: delete, n/Esc: abort")
        .prompt()
        .map_err(|err| anyhow::anyhow!("{err}"))
}

fn print_hard_cascade_plan(plan: &HardCascadePlan, dry_run: bool) {
    if plan.is_empty() && plan.skips.is_empty() {
        println!("\nNo orphaned projects to clean.");
        return;
    }
    let verb = if dry_run {
        "Would remove"
    } else {
        "Will remove"
    };
    if !plan.targets.is_empty() {
        println!("\n\x1b[1m── canopy clean --hard (orphan-project cascade) ──\x1b[0m");
        println!(" {verb} {} orphaned project(s):", plan.targets.len());
        for t in &plan.targets {
            let c = &t.counts;
            println!(
                "   {} ({})  missing: {}\n     [{} loop(s), {} interactive session(s), {} terminal session(s),\n      {} last prompt(s), {} scheduled send(s), {} failed send(s),\n      {} sync message(s), {} sync lock(s), {} intelligence node(s)]\n     + cascade: [{} loop_spec(s), {} loop_node(s), {} loop_edge(s),\n                 {} loop_run(s), {} completion_hook_run(s),\n                 {} ensemble(s), {} ensemble_member(s), {} pool_member(s),\n                 {} seed_session(s), {} intelligence_edge(s)]",
                t.name,
                t.hash,
                t.missing_path,
                c.loops,
                c.interactive_sessions,
                c.terminal_sessions,
                c.last_prompts,
                c.scheduled_sends,
                c.failed_scheduled_sends,
                c.sync_messages,
                c.sync_locks,
                c.intelligence_nodes,
                c.loop_specs,
                c.loop_nodes,
                c.loop_edges,
                c.loop_runs,
                c.loop_completion_hook_runs,
                c.ensembles,
                c.ensemble_members,
                c.pool_members,
                c.seed_sessions,
                c.intelligence_edges,
            );
        }
    }
    if !plan.skips.is_empty() {
        println!(
            "\n\x1b[33m⚠\x1b[0m  Skipped {} project(s) with in-flight state:",
            plan.skips.len()
        );
        for s in &plan.skips {
            println!(
                "   {} ({})  missing: {}  — {}",
                s.name,
                s.hash,
                s.missing_path,
                s.reason.describe()
            );
        }
    }
}

fn execute_plan(db: &Database, plan: &CleanPlan) -> Result<()> {
    if !plan.session_ids.is_empty() {
        db.delete_interactive_sessions(&plan.session_ids)?;
    }
    for f in &plan.log_files {
        let _ = std::fs::remove_file(&f.path);
    }
    for d in &plan.terminal_dirs {
        let _ = std::fs::remove_file(d.path.join("history.toml"));
        let _ = std::fs::remove_dir(&d.path);
    }
    for f in &plan.rag_residue_files {
        let _ = std::fs::remove_file(&f.path);
    }
    Ok(())
}

fn scan_log_files(data_dir: &Path) -> Result<Vec<FileCandidate>> {
    let dir = data_dir.join("logs");
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return Ok(out);
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("log") {
            continue;
        }
        let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
            continue;
        };
        let Ok(meta) = entry.metadata() else {
            continue;
        };
        out.push(FileCandidate {
            key: stem.to_string(),
            path,
            mtime: mtime_unix(&meta),
            size_bytes: meta.len(),
        });
    }
    Ok(out)
}

/// Terminal history lives at `terminals/<session_name>/history.toml`
/// (`tui::terminal_history`); `terminals/global_catalog.toml` is a shared
/// file, not a per-session dir, and is skipped by the `is_dir()` check.
fn scan_terminal_dirs(data_dir: &Path) -> Result<Vec<FileCandidate>> {
    let dir = data_dir.join("terminals");
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return Ok(out);
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let Some(name) = path.file_name().and_then(|s| s.to_str()) else {
            continue;
        };
        let hist_file = path.join("history.toml");
        let Ok(hist_meta) = std::fs::metadata(&hist_file) else {
            continue;
        };
        out.push(FileCandidate {
            key: name.to_string(),
            path,
            mtime: mtime_unix(&hist_meta),
            size_bytes: hist_meta.len(),
        });
    }
    Ok(out)
}

/// Only the top level of `rag/` is scanned, and never descended into —
/// `vectors.lancedb/` is LanceDB's own on-disk store and must never be
/// touched by name-based heuristics.
fn scan_rag_residue(data_dir: &Path) -> Result<Vec<FileCandidate>> {
    let dir = data_dir.join("rag");
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return Ok(out);
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            continue;
        }
        let Some(name) = path.file_name().and_then(|s| s.to_str()) else {
            continue;
        };
        let Ok(meta) = entry.metadata() else {
            continue;
        };
        out.push(FileCandidate {
            key: name.to_string(),
            path,
            mtime: mtime_unix(&meta),
            size_bytes: meta.len(),
        });
    }
    Ok(out)
}

fn mtime_unix(meta: &std::fs::Metadata) -> i64 {
    meta.modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn format_bytes(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["B", "KB", "MB", "GB"];
    let mut size = bytes as f64;
    let mut unit = 0;
    while size >= 1024.0 && unit < UNITS.len() - 1 {
        size /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{size:.1} {}", UNITS[unit])
    }
}

/// After a clean run, reclaim the database space its own row deletions
/// freed — but only when that's actually warranted. Skips automatically
/// (spec: "skipped automatically when the deletion was trivial") unless
/// `rows_deleted` clears [`clean::RECLAIM_ROW_THRESHOLD`], skips entirely
/// when `no_reclaim` opts out for a fast run, and never touches the file
/// under `--dry-run` — it only prints what a real run would do.
///
/// Reclaiming (`VACUUM` + WAL checkpoint) takes an exclusive lock on the
/// database, so it must not run while the daemon could be mid-write. The
/// daemon's own singleton lock (`daemon::process::acquire_daemon_lock`) is
/// a `daemon.pid`-backed flock; this reuses the same pid file (rather than
/// re-acquiring the flock, which would race the daemon's own re-acquire on
/// restart) to decide whether a daemon is up before ever calling `VACUUM`.
fn reclaim_if_warranted(
    db: &Database,
    data_dir: &Path,
    db_path: &Path,
    dry_run: bool,
    no_reclaim: bool,
    rows_deleted: u64,
) {
    if no_reclaim || !clean::should_reclaim(rows_deleted as usize) {
        return;
    }

    let size_before = std::fs::metadata(db_path).ok().map(|m| m.len());

    if dry_run {
        if let Some(before) = size_before {
            println!(
                "\n Database file: {} — would reclaim space ({rows_deleted} row(s) deleted, ≥ {} threshold; run without --dry-run to apply).",
                format_bytes(before),
                clean::RECLAIM_ROW_THRESHOLD,
            );
        }
        return;
    }

    let daemon_running = crate::daemon::process::read_pid(data_dir)
        .map(crate::daemon::process::is_process_running)
        .unwrap_or(false);
    if daemon_running {
        println!(
            "\n \x1b[33m⚠\x1b[0m  Skipped space reclamation: the canopy daemon is running and holds a write connection that a VACUUM's exclusive lock would conflict with. Stop it (`canopy daemon stop`) and re-run `canopy clean` to shrink the database file."
        );
        return;
    }

    match db.reclaim_space() {
        Ok(()) => {
            let size_after = std::fs::metadata(db_path).ok().map(|m| m.len());
            match (size_before, size_after) {
                (Some(before), Some(after)) => println!(
                    "\n Database file: {} -> {} ({rows_deleted} row(s) reclaimed via VACUUM + WAL checkpoint)",
                    format_bytes(before),
                    format_bytes(after),
                ),
                _ => println!(
                    "\n Reclaimed database space ({rows_deleted} row(s) via VACUUM + WAL checkpoint)."
                ),
            }
        }
        Err(err) => {
            eprintln!("\n \x1b[33m⚠\x1b[0m  Could not reclaim database space: {err}");
        }
    }
}

fn print_summary(plan: &CleanPlan, retention_days: u64, dry_run: bool) {
    let verb = if dry_run { "Would remove" } else { "Removed" };
    println!(
        "\n\x1b[1m── canopy clean (retention: {retention_days}d{}) ──\x1b[0m",
        if dry_run { ", dry run" } else { "" }
    );
    println!(
        " {verb} {} stale interactive session(s) (database rows)",
        plan.session_ids.len()
    );
    println!(" {verb} {} orphaned log file(s)", plan.log_files.len());
    println!(
        " {verb} {} orphaned terminal history dir(s)",
        plan.terminal_dirs.len()
    );
    println!(
        " {verb} {} leftover RAG residue file(s)",
        plan.rag_residue_files.len()
    );
    // Deliberately two separate lines: a row count is not a byte count, and
    // the row deletions above contribute nothing to this figure — it's
    // filesystem bytes from the log/terminal/RAG files only. Freed database
    // space is reported (if warranted) by `reclaim_if_warranted` below.
    println!(
        " Filesystem bytes {}: {}",
        if dry_run { "would be freed" } else { "freed" },
        format_bytes(plan.reclaimed_bytes())
    );

    if !plan.orphaned_projects.is_empty() {
        println!(
            "\n\x1b[33m⚠\x1b[0m  {} orphaned project(s) — workdir missing, reported only:",
            plan.orphaned_projects.len()
        );
        for p in &plan.orphaned_projects {
            println!(
                "   {} ({})  missing: {}  [{} loop(s), {} interactive session(s), {} terminal session(s)]",
                p.name,
                p.hash,
                p.missing_path,
                p.dependents.loops,
                p.dependents.interactive_sessions,
                p.dependents.terminal_sessions
            );
            println!(
                "     Hint: if this directory was renamed or moved, `canopy project remap {} <new-path>` \
                 keeps its history instead of deleting it.",
                p.hash
            );
        }
        println!(
            "   Hint: `canopy clean --hard` removes orphaned projects (and their dependents) with confirmation — \
             only if the directory is truly gone, not just moved."
        );
    }

    if plan.is_empty() && plan.orphaned_projects.is_empty() {
        println!("\nNothing to clean.");
    }
    println!();
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::loops::LoopStatus;
    use tempfile::tempdir;

    fn test_db(dir: &Path) -> Database {
        Database::new(&dir.join("test.db")).unwrap()
    }

    fn touch(path: &Path, contents: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, contents).unwrap();
    }

    #[test]
    fn dry_run_reports_but_deletes_nothing() {
        let dir = tempdir().unwrap();
        let data_dir = dir.path();
        let db = test_db(data_dir);

        db.insert_interactive_session(
            "s-old",
            "s-old",
            "opencode",
            "/tmp",
            None,
            None,
            "interactive",
            None,
        )
        .unwrap();
        db.finish_interactive_session("s-old", 0).unwrap();

        touch(&data_dir.join("logs/agent-gone.log"), "log contents");

        // Push the clock far enough forward that everything looks old
        // without needing to backdate real file mtimes.
        let now_ts = chrono::Utc::now().timestamp() + 30 * 86_400;
        let plan = run_clean(data_dir, &db, true, 7, now_ts).unwrap();

        assert_eq!(plan.session_ids, vec!["s-old".to_string()]);
        assert_eq!(plan.log_files.len(), 1);

        // Nothing was actually touched.
        assert_eq!(db.count_interactive_sessions().unwrap(), 1);
        assert!(data_dir.join("logs/agent-gone.log").exists());
    }

    #[test]
    fn real_run_deletes_orphaned_session_and_log_file() {
        let dir = tempdir().unwrap();
        let data_dir = dir.path();
        let db = test_db(data_dir);

        db.insert_interactive_session(
            "s-old",
            "s-old",
            "opencode",
            "/tmp",
            None,
            None,
            "interactive",
            None,
        )
        .unwrap();
        db.finish_interactive_session("s-old", 0).unwrap();

        touch(&data_dir.join("logs/agent-gone.log"), "log contents");

        let now_ts = chrono::Utc::now().timestamp() + 30 * 86_400;
        let plan = run_clean(data_dir, &db, false, 7, now_ts).unwrap();

        assert_eq!(plan.session_ids.len(), 1);
        assert_eq!(db.count_interactive_sessions().unwrap(), 0);
        assert!(!data_dir.join("logs/agent-gone.log").exists());
    }

    #[test]
    fn active_session_survives_a_real_run_even_when_ancient() {
        let dir = tempdir().unwrap();
        let data_dir = dir.path();
        let db = test_db(data_dir);

        db.insert_interactive_session(
            "s-active",
            "s-active",
            "opencode",
            "/tmp",
            None,
            None,
            "interactive",
            None,
        )
        .unwrap();

        let now_ts = chrono::Utc::now().timestamp() + 3650 * 86_400;
        run_clean(data_dir, &db, false, 7, now_ts).unwrap();

        assert_eq!(
            db.get_interactive_session_status("s-active").unwrap(),
            Some("active".to_string())
        );
    }

    #[test]
    fn known_agent_log_file_is_never_deleted() {
        use crate::application::ports::AgentRepository;

        let dir = tempdir().unwrap();
        let data_dir = dir.path();
        let db = test_db(data_dir);

        db.upsert_agent(&crate::domain::models::Agent {
            id: "agent-keep".to_string(),
            prompt: "do stuff".to_string(),
            trigger: None,
            cli: crate::domain::models::Cli::new("opencode"),
            model: None,
            working_dir: None,
            enabled: true,
            enable_at: None,
            created_at: chrono::Utc::now(),
            log_path: data_dir
                .join("logs/agent-keep.log")
                .to_string_lossy()
                .to_string(),
            timeout_minutes: 15,
            expires_at: None,
            last_run_at: None,
            last_run_ok: None,
            last_triggered_at: None,
            trigger_count: 0,
        })
        .unwrap();

        touch(&data_dir.join("logs/agent-keep.log"), "keep me");

        let now_ts = chrono::Utc::now().timestamp() + 30 * 86_400;
        run_clean(data_dir, &db, false, 7, now_ts).unwrap();

        assert!(data_dir.join("logs/agent-keep.log").exists());
    }

    #[test]
    fn orphaned_terminal_history_dir_detected_and_removed() {
        let dir = tempdir().unwrap();
        let data_dir = dir.path();
        let db = test_db(data_dir);

        touch(
            &data_dir.join("terminals/stray-term/history.toml"),
            "commands = []",
        );

        let now_ts = chrono::Utc::now().timestamp() + 30 * 86_400;
        let plan = run_clean(data_dir, &db, false, 7, now_ts).unwrap();

        assert_eq!(plan.terminal_dirs.len(), 1);
        assert!(!data_dir.join("terminals/stray-term").exists());
    }

    #[test]
    fn known_terminal_session_dir_survives() {
        let dir = tempdir().unwrap();
        let data_dir = dir.path();
        let db = test_db(data_dir);

        db.insert_terminal_session("t1", "kept-term", "bash", "/tmp")
            .unwrap();
        touch(
            &data_dir.join("terminals/kept-term/history.toml"),
            "commands = []",
        );

        let now_ts = chrono::Utc::now().timestamp() + 30 * 86_400;
        run_clean(data_dir, &db, false, 7, now_ts).unwrap();

        assert!(data_dir.join("terminals/kept-term").exists());
    }

    #[test]
    fn orphaned_project_with_missing_workdir_is_reported_not_deleted() {
        let dir = tempdir().unwrap();
        let data_dir = dir.path();
        let db = test_db(data_dir);

        let existing_workdir = dir.path().join("still-here");
        std::fs::create_dir_all(&existing_workdir).unwrap();

        db.upsert_project(&crate::domain::project::Project {
            hash: "hash-exists".to_string(),
            path: existing_workdir.to_string_lossy().to_string(),
            name: "exists".to_string(),
            description: None,
            tags: None,
            indexed_at: None,
            created_at: chrono::Utc::now().timestamp(),
        })
        .unwrap();
        db.upsert_project(&crate::domain::project::Project {
            hash: "hash-missing".to_string(),
            path: "/definitely/does/not/exist/anywhere".to_string(),
            name: "missing".to_string(),
            description: None,
            tags: None,
            indexed_at: None,
            created_at: chrono::Utc::now().timestamp(),
        })
        .unwrap();

        let now_ts = chrono::Utc::now().timestamp();
        let plan = run_clean(data_dir, &db, false, 7, now_ts).unwrap();

        assert_eq!(plan.orphaned_projects.len(), 1);
        assert_eq!(plan.orphaned_projects[0].hash, "hash-missing");
        // Soft mode never deletes the project row itself.
        assert_eq!(db.list_projects().unwrap().len(), 2);
    }

    #[test]
    fn rag_residue_tmp_file_is_removed_and_lancedb_dir_untouched() {
        let dir = tempdir().unwrap();
        let data_dir = dir.path();
        let db = test_db(data_dir);

        touch(&data_dir.join("rag/leftover.tmp"), "partial ingest");
        touch(
            &data_dir.join("rag/vectors.lancedb/manifest.json"),
            "not a residue file",
        );

        let now_ts = chrono::Utc::now().timestamp() + 30 * 86_400;
        let plan = run_clean(data_dir, &db, false, 7, now_ts).unwrap();

        assert_eq!(plan.rag_residue_files.len(), 1);
        assert!(!data_dir.join("rag/leftover.tmp").exists());
        assert!(data_dir.join("rag/vectors.lancedb/manifest.json").exists());
    }

    #[test]
    fn format_bytes_renders_human_units() {
        assert_eq!(format_bytes(0), "0 B");
        assert_eq!(format_bytes(512), "512 B");
        assert_eq!(format_bytes(2048), "2.0 KB");
    }

    fn make_project(hash: &str, path: &str) -> crate::domain::project::Project {
        crate::domain::project::Project {
            hash: hash.to_string(),
            path: path.to_string(),
            name: path.rsplit('/').next().unwrap_or(path).to_string(),
            description: None,
            tags: None,
            indexed_at: None,
            created_at: 1_700_000_000,
        }
    }

    fn make_loop(
        id: &str,
        workdir: &str,
        status: crate::domain::loops::LoopStatus,
    ) -> crate::domain::loops::Loop {
        crate::domain::loops::Loop {
            id: id.to_string(),
            name: format!("loop-{id}"),
            description: None,
            workdir: workdir.to_string(),
            status,
            trigger: None,
            created_at: chrono::Utc::now(),
            started_at: None,
            completed_at: None,
            autorun_at: None,
            auto_continue_at: None,
            auto_continue_action: None,
            active_run_pool_id: None,
            on_completed: None,
        }
    }

    #[test]
    fn hard_cascade_with_yes_deletes_orphan_and_its_dependents() {
        let dir = tempdir().unwrap();
        let data_dir = dir.path();
        let db = test_db(data_dir);
        let workdir = "/definitely/does/not/exist";
        let hash = "hash-orphan";
        db.upsert_project(&make_project(hash, workdir)).unwrap();
        db.insert_loop(&make_loop("loop-1", workdir, LoopStatus::Completed))
            .unwrap();
        db.insert_interactive_session(
            "s-old",
            "s-old",
            "opencode",
            workdir,
            None,
            None,
            "interactive",
            None,
        )
        .unwrap();
        db.finish_interactive_session("s-old", 0).unwrap();
        db.insert_terminal_session("t-1", "t-1", "bash", workdir)
            .unwrap();

        // --hard --yes: confirm the cascade executes without prompting.
        run_hard_cascade(&db, false, true).unwrap();

        assert!(db.get_project(hash).unwrap().is_none());
        assert_eq!(db.project_dependent_counts(workdir).unwrap().loops, 0);
        assert_eq!(
            db.project_dependent_counts(workdir)
                .unwrap()
                .interactive_sessions,
            0
        );
        assert_eq!(
            db.project_dependent_counts(workdir)
                .unwrap()
                .terminal_sessions,
            0
        );
    }

    #[test]
    fn hard_cascade_dry_run_deletes_nothing() {
        let dir = tempdir().unwrap();
        let data_dir = dir.path();
        let db = test_db(data_dir);
        let workdir = "/definitely/does/not/exist";
        let hash = "hash-orphan-dry";
        db.upsert_project(&make_project(hash, workdir)).unwrap();
        db.insert_loop(&make_loop("loop-1", workdir, LoopStatus::Completed))
            .unwrap();
        db.insert_interactive_session(
            "s-old",
            "s-old",
            "opencode",
            workdir,
            None,
            None,
            "interactive",
            None,
        )
        .unwrap();
        db.finish_interactive_session("s-old", 0).unwrap();

        // --hard --dry-run: no prompt, no deletes, plan still printed.
        run_hard_cascade(&db, true, false).unwrap();

        assert!(db.get_project(hash).unwrap().is_some());
        assert_eq!(db.project_dependent_counts(workdir).unwrap().loops, 1);
        assert_eq!(
            db.project_dependent_counts(workdir)
                .unwrap()
                .interactive_sessions,
            1
        );
    }

    #[test]
    fn hard_cascade_keeps_project_with_existing_workdir_intact() {
        let dir = tempdir().unwrap();
        let data_dir = dir.path();
        let db = test_db(data_dir);
        let workdir = dir.path().join("real-workdir");
        std::fs::create_dir_all(&workdir).unwrap();
        let workdir_str = workdir.to_string_lossy().to_string();
        let hash = "hash-keep";
        db.upsert_project(&make_project(hash, &workdir_str))
            .unwrap();
        db.insert_loop(&make_loop("loop-1", &workdir_str, LoopStatus::Completed))
            .unwrap();
        db.insert_interactive_session(
            "s-1",
            "s-1",
            "opencode",
            &workdir_str,
            None,
            None,
            "interactive",
            None,
        )
        .unwrap();
        db.finish_interactive_session("s-1", 0).unwrap();

        run_hard_cascade(&db, false, true).unwrap();

        // Project with an existing workdir is NEVER a target of --hard.
        assert!(db.get_project(hash).unwrap().is_some());
        assert_eq!(db.project_dependent_counts(&workdir_str).unwrap().loops, 1);
    }

    #[test]
    fn hard_cascade_skips_orphan_with_running_loop() {
        let dir = tempdir().unwrap();
        let data_dir = dir.path();
        let db = test_db(data_dir);
        let workdir = "/orphan/with/running/loop";
        let hash = "hash-running";
        db.upsert_project(&make_project(hash, workdir)).unwrap();
        db.insert_loop(&make_loop("loop-r", workdir, LoopStatus::Running))
            .unwrap();

        run_hard_cascade(&db, false, true).unwrap();

        // Project survives because its loop is still running.
        assert!(db.get_project(hash).unwrap().is_some());
        assert!(db.get_loop("loop-r").unwrap().is_some());
    }

    #[test]
    fn hard_cascade_skips_orphan_with_active_session() {
        let dir = tempdir().unwrap();
        let data_dir = dir.path();
        let db = test_db(data_dir);
        let workdir = "/orphan/with/active/session";
        let hash = "hash-active";
        db.upsert_project(&make_project(hash, workdir)).unwrap();
        db.insert_interactive_session(
            "s-live",
            "s-live",
            "opencode",
            workdir,
            None,
            None,
            "interactive",
            None,
        )
        .unwrap();

        run_hard_cascade(&db, false, true).unwrap();

        assert!(db.get_project(hash).unwrap().is_some());
        assert_eq!(
            db.project_dependent_counts(workdir)
                .unwrap()
                .interactive_sessions,
            1
        );
    }

    #[test]
    fn hard_cascade_processes_targets_and_skips_in_one_call() {
        // Mixed: one deletable orphan, one skipped (running loop), one with
        // a real workdir. --hard should delete only the first.
        let dir = tempdir().unwrap();
        let data_dir = dir.path();
        let db = test_db(data_dir);

        let real_workdir = dir.path().join("real");
        std::fs::create_dir_all(&real_workdir).unwrap();
        let real_str = real_workdir.to_string_lossy().to_string();

        db.upsert_project(&make_project("hash-real", &real_str))
            .unwrap();
        db.upsert_project(&make_project("hash-doomed", "/orphan/doomed"))
            .unwrap();
        db.upsert_project(&make_project("hash-skipped", "/orphan/skipped"))
            .unwrap();

        db.insert_loop(&make_loop("loop-real", &real_str, LoopStatus::Completed))
            .unwrap();
        db.insert_loop(&make_loop(
            "loop-doomed",
            "/orphan/doomed",
            LoopStatus::Completed,
        ))
        .unwrap();
        db.insert_loop(&make_loop(
            "loop-skipped",
            "/orphan/skipped",
            LoopStatus::Running,
        ))
        .unwrap();

        run_hard_cascade(&db, false, true).unwrap();

        assert!(db.get_project("hash-real").unwrap().is_some());
        assert!(db.get_project("hash-doomed").unwrap().is_none());
        assert!(db.get_project("hash-skipped").unwrap().is_some());
    }

    // ── reclaim_if_warranted ────────────────────────────────────────────

    /// Combined on-disk footprint (main file + WAL) so shrinkage is
    /// detectable regardless of whether data happened to already be
    /// checkpointed out of the WAL at the moment of measurement.
    fn total_db_size(db_path: &Path) -> u64 {
        let main = std::fs::metadata(db_path).map(|m| m.len()).unwrap_or(0);
        let wal_path = std::path::PathBuf::from(format!("{}-wal", db_path.to_string_lossy()));
        let wal = std::fs::metadata(&wal_path).map(|m| m.len()).unwrap_or(0);
        main + wal
    }

    /// Inserts `count` padded, immediately-deleted sessions so the database
    /// has freed-but-unreturned pages worth reclaiming.
    fn bulk_insert_and_delete_sessions(db: &Database, count: usize) {
        let padding = "x".repeat(4096);
        let mut ids = Vec::new();
        for i in 0..count {
            let id = format!("s-{i}");
            db.insert_interactive_session(
                &id,
                &id,
                "opencode",
                "/tmp",
                Some(&padding),
                None,
                "interactive",
                None,
            )
            .unwrap();
            db.finish_interactive_session(&id, 0).unwrap();
            ids.push(id);
        }
        db.delete_interactive_sessions(&ids).unwrap();
    }

    #[test]
    fn reclaim_shrinks_file_when_threshold_met_and_daemon_not_running() {
        let dir = tempdir().unwrap();
        let data_dir = dir.path();
        let db_path = data_dir.join("test.db");
        let db = Database::new(&db_path).unwrap();

        // Above RECLAIM_ROW_THRESHOLD (50).
        bulk_insert_and_delete_sessions(&db, 60);

        let size_before = total_db_size(&db_path);
        reclaim_if_warranted(&db, data_dir, &db_path, false, false, 60);
        let size_after = total_db_size(&db_path);

        assert!(
            size_after < size_before,
            "expected reclaim to shrink the file: {size_before} -> {size_after}"
        );
    }

    #[test]
    fn reclaim_skipped_when_deletion_is_trivial() {
        let dir = tempdir().unwrap();
        let data_dir = dir.path();
        let db_path = data_dir.join("test.db");
        let db = Database::new(&db_path).unwrap();

        bulk_insert_and_delete_sessions(&db, 5);

        let size_before = total_db_size(&db_path);
        // Below RECLAIM_ROW_THRESHOLD (50): must not touch the file.
        reclaim_if_warranted(&db, data_dir, &db_path, false, false, 5);
        let size_after = total_db_size(&db_path);

        assert_eq!(size_before, size_after);
    }

    #[test]
    fn reclaim_skipped_when_no_reclaim_flag_set() {
        let dir = tempdir().unwrap();
        let data_dir = dir.path();
        let db_path = data_dir.join("test.db");
        let db = Database::new(&db_path).unwrap();

        bulk_insert_and_delete_sessions(&db, 60);

        let size_before = total_db_size(&db_path);
        // Above threshold, but --no-reclaim opts out.
        reclaim_if_warranted(&db, data_dir, &db_path, false, true, 60);
        let size_after = total_db_size(&db_path);

        assert_eq!(size_before, size_after);
    }

    #[test]
    fn reclaim_skipped_and_untouched_under_dry_run() {
        let dir = tempdir().unwrap();
        let data_dir = dir.path();
        let db_path = data_dir.join("test.db");
        let db = Database::new(&db_path).unwrap();

        bulk_insert_and_delete_sessions(&db, 60);

        let size_before = total_db_size(&db_path);
        // Above threshold, but --dry-run must project only, never touch.
        reclaim_if_warranted(&db, data_dir, &db_path, true, false, 60);
        let size_after = total_db_size(&db_path);

        assert_eq!(size_before, size_after);
    }

    #[test]
    fn reclaim_skipped_while_daemon_is_running() {
        let dir = tempdir().unwrap();
        let data_dir = dir.path();
        let db_path = data_dir.join("test.db");
        let db = Database::new(&db_path).unwrap();

        bulk_insert_and_delete_sessions(&db, 60);

        // Current test process is guaranteed alive, so `daemon.pid`
        // naming it makes `is_process_running` report true, exactly as it
        // would for a live `canopy serve`.
        std::fs::write(data_dir.join("daemon.pid"), std::process::id().to_string()).unwrap();

        let size_before = total_db_size(&db_path);
        reclaim_if_warranted(&db, data_dir, &db_path, false, false, 60);
        let size_after = total_db_size(&db_path);

        assert_eq!(
            size_before, size_after,
            "must not VACUUM while the daemon holds a write connection"
        );
    }
}
