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
use std::sync::{Arc, Mutex};

use anyhow::Result;

use crate::daemon::process;
use crate::db::Database;
use crate::domain::canopy_config::CanopyConfig;
use crate::domain::clean::{
    self, CleanPlan, FileCandidate, HardCascadeCandidate, HardCascadePlan, ProjectCandidate,
};
use crate::domain::db_paths::database_path;

pub async fn handle_clean_action(
    dry_run: bool,
    older_than: Option<u64>,
    hard: bool,
    yes: bool,
    no_reclaim: bool,
    stop_daemon: bool,
) -> Result<()> {
    let data_dir = crate::ensure_data_dir()?;
    let db_path = database_path(&data_dir);
    let db = Database::new_safe(&db_path, &data_dir)?;
    let config = CanopyConfig::load(&data_dir);
    let retention_days = older_than.unwrap_or(config.clean.retention_days);
    let now_ts = chrono::Utc::now().timestamp();

    // Project (read-only, no execution) what a real run would delete, so we
    // only ever consider the stop-daemon reclaim window when it's actually
    // warranted — stopping the daemon to reclaim a handful of rows would
    // just be downtime for nothing.
    let projected_plan = build_clean_plan(&data_dir, &db, retention_days, now_ts)?;
    let mut projected_rows = projected_plan.deleted_row_count() as u64;
    if hard {
        projected_rows += projected_hard_cascade_rows(&build_hard_cascade_plan(&db)?);
    }
    let reclaim_warranted = !no_reclaim && clean::should_reclaim(projected_rows as usize);

    if !reclaim_warranted {
        return run_clean_without_window(
            &db,
            &data_dir,
            &db_path,
            dry_run,
            hard,
            yes,
            retention_days,
            now_ts,
            no_reclaim,
        );
    }

    if dry_run {
        print_summary(&projected_plan, retention_days, true);
        let mut rows_deleted = projected_plan.deleted_row_count() as u64;
        if hard {
            rows_deleted += run_hard_cascade(&db, true, yes)?;
        }
        print_dry_run_reclaim_window(rows_deleted, &db_path);
        return Ok(());
    }

    let daemon_pid = process::read_pid(&data_dir).filter(|&p| process::is_process_running(p));

    if daemon_pid.is_some() {
        let consent = stop_daemon || confirm_stop_daemon()?;
        if !consent {
            return run_clean_without_window(
                &db,
                &data_dir,
                &db_path,
                false,
                hard,
                yes,
                retention_days,
                now_ts,
                no_reclaim,
            );
        }
    }

    run_reclaim_window_and_report(
        &data_dir,
        &db_path,
        retention_days,
        now_ts,
        hard,
        yes,
        daemon_pid,
    )
    .await
}

/// The pre-B2 flow: build (and unless `dry_run`, execute) the plan, run
/// `--hard` if requested, then let [`reclaim_if_warranted`] decide whether a
/// same-process reclaim is possible (only when the daemon happens to already
/// be down) — used whenever the stop-daemon reclaim window isn't in play:
/// reclaiming wasn't warranted, or it was but the operator didn't consent to
/// stopping the daemon.
#[allow(clippy::too_many_arguments)]
fn run_clean_without_window(
    db: &Database,
    data_dir: &Path,
    db_path: &Path,
    dry_run: bool,
    hard: bool,
    yes: bool,
    retention_days: u64,
    now_ts: i64,
    no_reclaim: bool,
) -> Result<()> {
    let plan = run_clean(data_dir, db, dry_run, retention_days, now_ts)?;
    print_summary(&plan, retention_days, dry_run);

    let mut rows_deleted = plan.deleted_row_count() as u64;
    if hard {
        rows_deleted += run_hard_cascade(db, dry_run, yes)?;
    }

    reclaim_if_warranted(db, data_dir, db_path, dry_run, no_reclaim, rows_deleted);

    Ok(())
}

/// Gather inputs and build the plan, and (unless `dry_run`) execute it.
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
    let plan = build_clean_plan(data_dir, db, retention_days, now_ts)?;
    if !dry_run {
        execute_plan(db, &plan)?;
    }
    Ok(plan)
}

/// Pure planning half of [`run_clean`] — gathers facts and builds the
/// [`CleanPlan`] without executing it. Split out so the reclaim window can
/// project the row count a real run would delete (to decide whether
/// reclaiming is even warranted) without a redundant `execute_plan` call,
/// and so it can later execute that same, already-built plan once inside
/// the window instead of re-scanning.
fn build_clean_plan(
    data_dir: &Path,
    db: &Database,
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

    Ok(CleanPlan {
        session_ids,
        log_files: orphan_logs,
        terminal_dirs: orphan_terminals,
        rag_residue_files: rag_residue,
        orphaned_projects,
    })
}

/// `--hard` mode: gather the per-project cascade facts, build the
/// [`HardCascadePlan`], print it, and (unless `dry_run`) prompt and
/// execute. Splits the orphan work from `run_clean` so the soft-mode
/// tests don't pay for the extra DB roundtrips when `--hard` isn't set.
/// Returns the number of database rows the cascade removed (real run) or
/// would remove (`--dry-run`'s projection), so the caller can fold it into
/// the total that decides whether reclaiming space is warranted.
/// Pure fact-gathering half of `--hard`'s cascade: builds the
/// [`HardCascadePlan`] without printing or executing anything. Split out so
/// the reclaim window can project the cascade's row count (to decide
/// whether reclaiming is warranted) without the double-printed plan a
/// second `run_hard_cascade` call would otherwise produce.
fn build_hard_cascade_plan(db: &Database) -> Result<HardCascadePlan> {
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

    Ok(clean::plan_hard_cascade(&candidates))
}

/// Projected row count (direct + cascade) the plan's targets would remove,
/// for the reclaim-warranted projection — never executes anything.
fn projected_hard_cascade_rows(plan: &HardCascadePlan) -> u64 {
    plan.targets
        .iter()
        .map(|t| (direct_count(&t.counts) + cascade_count(&t.counts)) as u64)
        .sum()
}

fn run_hard_cascade(db: &Database, dry_run: bool, yes: bool) -> Result<u64> {
    let plan = build_hard_cascade_plan(db)?;
    print_hard_cascade_plan(&plan, dry_run);

    if plan.targets.is_empty() {
        return Ok(0);
    }

    if dry_run {
        // Plan-only: no prompt, no deletes (spec: "deletes nothing, no
        // confirmation needed"). Project the row count so `--dry-run`
        // reports the same reclaim figures a real run would.
        return Ok(projected_hard_cascade_rows(&plan));
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
        + c.operational_sessions
}

fn cascade_count(c: &clean::HardCascadeCounts) -> i64 {
    c.loop_specs
        + c.loop_nodes
        + c.loop_edges
        + c.loop_runs
        + c.loop_completion_hook_runs
        + c.ensembles
        + c.ensemble_members
        + c.queue_members
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
                "   {} ({})  missing: {}\n     [{} loop(s), {} interactive session(s), {} terminal session(s),\n      {} last prompt(s), {} scheduled send(s), {} failed send(s),\n      {} sync message(s), {} sync lock(s), {} intelligence node(s), {} operational session(s)]\n     + cascade: [{} loop_spec(s), {} loop_node(s), {} loop_edge(s),\n                 {} loop_run(s), {} completion_hook_run(s),\n                 {} ensemble(s), {} ensemble_member(s), {} queue_member(s),\n                 {} seed_session(s), {} intelligence_edge(s)]",
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
                c.operational_sessions,
                c.loop_specs,
                c.loop_nodes,
                c.loop_edges,
                c.loop_runs,
                c.loop_completion_hook_runs,
                c.ensembles,
                c.ensemble_members,
                c.queue_members,
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

// ── B2: the stop-daemon reclaim window ───────────────────────────────────
//
// `reclaim_if_warranted` above only ever fires when the daemon happens to
// already be down — normally it never is, so that path is effectively
// dead. The functions below let `canopy clean` create the exclusive window
// itself: refuse if busy, stop the daemon, `quick_check` gate, run the
// existing cleanup, `VACUUM` + WAL checkpoint, and restore the daemon on
// every exit path — including a panic or Ctrl-C.

/// Interactive consent to stop the daemon, mirroring the `--hard`/`--yes`
/// precedent in this same command: refuses (rather than hangs or
/// accidentally confirms) on a non-tty or a stray Enter.
fn confirm_stop_daemon() -> Result<bool> {
    use inquire::Confirm;
    Confirm::new(
        "Reclaiming this much space requires stopping the canopy daemon. Stop it, reclaim, then restart it?",
    )
    .with_default(false)
    .with_help_message("y: stop/reclaim/restart, n/Esc: skip reclamation")
    .prompt()
    .or(Ok(false))
}

fn print_dry_run_reclaim_window(rows_deleted: u64, db_path: &Path) {
    let size_before = std::fs::metadata(db_path).ok().map(|m| m.len());
    println!(
        "\n Database file: {} — would reclaim space ({rows_deleted} row(s) deleted, ≥ {} threshold).",
        size_before.map(format_bytes).unwrap_or_default(),
        clean::RECLAIM_ROW_THRESHOLD,
    );
    println!(
        " Would stop the canopy daemon (if running), run `PRAGMA quick_check`, reclaim space \
         (VACUUM + WAL checkpoint), then restart the daemon exactly as it was running."
    );
}

/// Who currently owns the running daemon process — decides how it must be
/// stopped and restored (B-decision 4). `Managed` carries the service
/// manager's name (`"systemd"` on Linux, `"launchd"` on macOS) purely for
/// logging/reporting.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DaemonOwnerKind {
    Managed(&'static str),
    Detached,
}

impl DaemonOwnerKind {
    fn describe(&self) -> &'static str {
        match self {
            DaemonOwnerKind::Managed("systemd") => "systemd",
            DaemonOwnerKind::Managed("launchd") => "launchd",
            DaemonOwnerKind::Managed(_) => "the service manager",
            DaemonOwnerKind::Detached => "a detached process (no owning service unit)",
        }
    }
}

/// Reuses [`process::service_manager_facts`] (systemd/launchd cgroup or
/// `launchctl` fact-checking already built for `daemon status`) rather than
/// inventing a second ownership check: the daemon is service-manager-owned
/// only if that manager's own live-PID fact names exactly this PID.
fn detect_daemon_owner(daemon_pid: u32) -> DaemonOwnerKind {
    match process::service_manager_facts() {
        Some(facts) if facts.pid == Some(daemon_pid) => DaemonOwnerKind::Managed(facts.name),
        _ => DaemonOwnerKind::Detached,
    }
}

/// B-decision 5: detect *before* stopping anything whether restoration is
/// even possible. A missing/disabled systemd unit must not be discovered
/// only after the daemon is already down.
fn restoration_feasible(owner: DaemonOwnerKind) -> std::result::Result<(), String> {
    let unit_installed = process::service_unit_installed();
    #[cfg(target_os = "linux")]
    let (systemd_available, service_enabled) = (
        process::is_systemd_available(),
        process::is_service_enabled(),
    );
    #[cfg(not(target_os = "linux"))]
    let (systemd_available, service_enabled) = (true, true);
    restoration_feasible_from_facts(owner, unit_installed, systemd_available, service_enabled)
}

/// Pure decision half of [`restoration_feasible`] (B-decision 5), fed
/// booleans instead of shelling out itself — `systemctl`/the unit file are
/// absent in some CI containers (see [`process::is_systemd_available`]),
/// so a test asserting a specific refusal must not depend on the machine
/// it happens to run on actually having (or lacking) a real canopy unit.
fn restoration_feasible_from_facts(
    owner: DaemonOwnerKind,
    unit_installed: bool,
    systemd_available: bool,
    service_enabled: bool,
) -> std::result::Result<(), String> {
    match owner {
        DaemonOwnerKind::Managed(name) => {
            if !unit_installed {
                return Err(format!(
                    "{name} unit is not installed — restoring through {name} is impossible"
                ));
            }
            if name == "systemd" {
                if !systemd_available {
                    return Err("the systemd user session is unavailable".to_string());
                }
                if !service_enabled {
                    return Err(format!(
                        "the {name} unit is disabled — a restart cannot be trusted to bring the daemon back"
                    ));
                }
            }
            Ok(())
        }
        DaemonOwnerKind::Detached => std::env::current_exe()
            .map(|_| ())
            .map_err(|err| format!("cannot resolve the canopy executable to relaunch it: {err}")),
    }
}

/// The stop/restart mechanics, behind a trait so [`run_reclaim_window`]'s
/// decision logic — the part the spec requires tests for — is exercisable
/// with a fake instead of a real subprocess/systemctl dance.
trait DaemonOps {
    fn stop(&self, owner: DaemonOwnerKind) -> Result<()>;
    fn restart(&self, owner: DaemonOwnerKind) -> Result<()>;
}

struct RealDaemonOps {
    data_dir: std::path::PathBuf,
    port: u16,
}

impl DaemonOps for RealDaemonOps {
    fn stop(&self, owner: DaemonOwnerKind) -> Result<()> {
        match owner {
            // Never a bare SIGTERM here: the installed unit has
            // `Restart=on-failure`, so signalling the PID directly reads to
            // the manager as an unclean exit and it respawns the daemon out
            // from under the exclusive VACUUM this stop is for. Going
            // through the manager's own stop verb is a stop it won't
            // immediately undo.
            DaemonOwnerKind::Managed(name) => {
                if !process::service_manager_stop() {
                    anyhow::bail!("failed to stop the {name}-managed daemon");
                }
                Ok(())
            }
            DaemonOwnerKind::Detached => {
                let pid = process::read_pid(&self.data_dir)
                    .or_else(|| process::resolve_port_pid(self.port))
                    .filter(|&p| process::is_process_running(p));
                if let Some(pid) = pid {
                    process::send_signal(pid);
                    for _ in 0..20 {
                        std::thread::sleep(std::time::Duration::from_millis(250));
                        if !process::is_process_running(pid) {
                            break;
                        }
                    }
                }
                process::remove_pid_file(&self.data_dir);
                Ok(())
            }
        }
    }

    fn restart(&self, owner: DaemonOwnerKind) -> Result<()> {
        match owner {
            DaemonOwnerKind::Managed(name) => {
                if !process::service_manager_start() {
                    anyhow::bail!("failed to restart the {name}-managed daemon");
                }
                Ok(())
            }
            DaemonOwnerKind::Detached => spawn_detached_daemon(&self.data_dir, self.port),
        }
    }
}

/// Relaunch a detached `canopy serve`, mirroring `canopy daemon start`'s
/// spawn (setsid, logs appended, stdin null) but deliberately without its
/// `install_service_if_needed` side effect — restoring a daemon that wasn't
/// service-manager-owned must not silently switch it to being one.
fn spawn_detached_daemon(data_dir: &Path, port: u16) -> Result<()> {
    let exe = std::env::current_exe()?;
    let mut cmd = std::process::Command::new(&exe);
    cmd.arg("serve").arg("--port").arg(port.to_string());

    let log_path = data_dir.join("daemon.log");
    let log_file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)?;
    let log_file_err = log_file.try_clone()?;
    cmd.stdout(log_file)
        .stderr(log_file_err)
        .stdin(std::process::Stdio::null());

    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        unsafe {
            cmd.pre_exec(|| {
                libc::setsid();
                Ok(())
            });
        }
    }

    let child = cmd.spawn()?;
    let child_pid = child.id();
    std::thread::sleep(std::time::Duration::from_millis(500));
    if !process::is_process_running(child_pid) {
        anyhow::bail!("daemon process exited immediately after restart");
    }
    Ok(())
}

/// What happened inside the reclaim window — every branch reports exactly
/// what the spec requires (busy names what's busy; a `quick_check` failure
/// says nothing was modified; the happy path reports every step
/// distinctly).
enum ReclaimWindowOutcome {
    /// Refused before touching anything: either busy (names what) or
    /// restoration was judged infeasible (names why).
    Refused(String),
    /// `quick_check` found a problem — VACUUM was never attempted and the
    /// existing cleanup never ran, so nothing was modified.
    QuickCheckFailed {
        verdict: String,
        daemon_restored: bool,
    },
    Completed(ReclaimWindowReport),
}

struct ReclaimWindowReport {
    daemon_was_running: bool,
    daemon_owner: Option<DaemonOwnerKind>,
    quick_check: String,
    plan: CleanPlan,
    retention_days: u64,
    hard_rows: u64,
    size_before: Option<u64>,
    size_after: Option<u64>,
    daemon_restored: bool,
}

/// Restores the daemon exactly once, no matter how many callers race to
/// call it (the sync happy/error paths below, and — in the real CLI glue —
/// a concurrent Ctrl-C handler). `restored` only latches `true` on an
/// actual success so a failed attempt can still be retried by whichever
/// caller asks next (idempotent restoration per the spec).
fn restore_daemon_once(
    restored: &Mutex<bool>,
    ops: &dyn DaemonOps,
    owner: Option<DaemonOwnerKind>,
) -> bool {
    let mut done = restored.lock().unwrap_or_else(|e| e.into_inner());
    if *done {
        return true;
    }
    let ok = match owner {
        None => true,
        Some(owner) => ops.restart(owner).is_ok(),
    };
    *done = ok;
    ok
}

/// The orchestrated window itself (B-decision 1-6, functional
/// requirements): refuse-if-busy, stop, `quick_check`, existing cleanup,
/// `VACUUM` + WAL checkpoint, restore — in that order, with the daemon
/// restored on every exit path via `restored`/[`restore_daemon_once`].
///
/// Deliberately synchronous and injected with `ops` rather than calling the
/// real subprocess/systemctl mechanics directly: this is the part the spec
/// requires tests for (busy refusal, `quick_check`-gates-VACUUM,
/// restore-on-failure, infeasible-restoration refusal), and none of that
/// needs a real daemon process to exercise.
#[allow(clippy::too_many_arguments)]
fn run_reclaim_window(
    db: &Database,
    data_dir: &Path,
    db_path: &Path,
    retention_days: u64,
    now_ts: i64,
    hard: bool,
    yes: bool,
    daemon_pid: Option<u32>,
    ops: &dyn DaemonOps,
    restored: &Mutex<bool>,
) -> Result<ReclaimWindowOutcome> {
    let busy = db.busy_reasons()?;
    if !busy.is_empty() {
        return Ok(ReclaimWindowOutcome::Refused(format!(
            "refusing to stop the daemon — busy: {}",
            busy.join(", ")
        )));
    }

    let owner = daemon_pid.map(detect_daemon_owner);
    if let Some(owner) = owner {
        if let Err(reason) = restoration_feasible(owner) {
            return Ok(ReclaimWindowOutcome::Refused(format!(
                "refusing to stop the daemon — cannot guarantee it can be restored afterward: {reason}"
            )));
        }
    }

    // Daemon wasn't running at all: nothing to stop, so `restored` starts
    // already-true and every subsequent restore attempt is a no-op.
    if owner.is_none() {
        *restored.lock().unwrap_or_else(|e| e.into_inner()) = true;
    }

    /// Panic safety net: if anything below unwinds, restore on drop rather
    /// than leaving the daemon down. A no-op once `restore_daemon_once` has
    /// already succeeded from the normal control flow.
    struct PanicGuard<'a> {
        ops: &'a dyn DaemonOps,
        owner: Option<DaemonOwnerKind>,
        restored: &'a Mutex<bool>,
    }
    impl Drop for PanicGuard<'_> {
        fn drop(&mut self) {
            restore_daemon_once(self.restored, self.ops, self.owner);
        }
    }
    let _panic_guard = PanicGuard {
        ops,
        owner,
        restored,
    };

    if let Some(owner) = owner {
        ops.stop(owner)?;
    }

    let quick_check = db.quick_check()?;
    if quick_check != "ok" {
        let daemon_restored = restore_daemon_once(restored, ops, owner);
        return Ok(ReclaimWindowOutcome::QuickCheckFailed {
            verdict: quick_check,
            daemon_restored,
        });
    }

    let plan = build_clean_plan(data_dir, db, retention_days, now_ts)?;
    execute_plan(db, &plan)?;
    let hard_rows = if hard {
        run_hard_cascade(db, false, yes)?
    } else {
        0
    };

    let size_before = std::fs::metadata(db_path).ok().map(|m| m.len());
    db.reclaim_space()?;
    let size_after = std::fs::metadata(db_path).ok().map(|m| m.len());

    let daemon_restored = restore_daemon_once(restored, ops, owner);

    Ok(ReclaimWindowOutcome::Completed(ReclaimWindowReport {
        daemon_was_running: daemon_pid.is_some(),
        daemon_owner: owner,
        quick_check,
        plan,
        retention_days,
        hard_rows,
        size_before,
        size_after,
        daemon_restored,
    }))
}

fn print_reclaim_window_outcome(outcome: &ReclaimWindowOutcome) {
    match outcome {
        ReclaimWindowOutcome::Refused(reason) => {
            println!("\n \x1b[33m⚠\x1b[0m  {reason}");
            println!(
                "   Skipped space reclamation and left the daemon exactly as it was — retry once that clears."
            );
        }
        ReclaimWindowOutcome::QuickCheckFailed {
            verdict,
            daemon_restored,
        } => {
            println!(
                "\n \x1b[31m✗\x1b[0m  PRAGMA quick_check reported a problem with the database file: {verdict}"
            );
            println!(
                "   No reclamation was attempted and nothing was modified — the file was left exactly as found."
            );
            println!(
                "   Daemon restored: {}",
                if *daemon_restored {
                    "yes"
                } else {
                    "\x1b[31mno — check `canopy daemon status`\x1b[0m"
                }
            );
        }
        ReclaimWindowOutcome::Completed(report) => {
            print_summary(&report.plan, report.retention_days, false);
            println!(
                " Daemon stopped: {}",
                if report.daemon_was_running {
                    format!(
                        "yes ({})",
                        report
                            .daemon_owner
                            .map(|o| o.describe())
                            .unwrap_or("unknown")
                    )
                } else {
                    "no (was not running)".to_string()
                }
            );
            println!(" quick_check: {}", report.quick_check);
            match (report.size_before, report.size_after) {
                (Some(before), Some(after)) => println!(
                    " Database file: {} -> {} ({} row(s) reclaimed via VACUUM + WAL checkpoint)",
                    format_bytes(before),
                    format_bytes(after),
                    report.plan.deleted_row_count() as u64 + report.hard_rows,
                ),
                _ => println!(
                    " Reclaimed database space ({} row(s) via VACUUM + WAL checkpoint).",
                    report.plan.deleted_row_count() as u64 + report.hard_rows
                ),
            }
            println!(
                " Daemon restored: {}",
                if report.daemon_restored {
                    "yes"
                } else {
                    "\x1b[31mno — check `canopy daemon status`\x1b[0m"
                }
            );
        }
    }
}

/// Real-CLI glue around [`run_reclaim_window`]: runs it on a blocking
/// thread (it does real subprocess/flock/VACUUM work) while a concurrent
/// task watches for Ctrl-C. Both sides restore through the same
/// `restored` flag, so whichever notices first — the window finishing on
/// its own, or the operator interrupting — is the one that actually
/// restarts the daemon; the other is a no-op.
async fn run_reclaim_window_and_report(
    data_dir: &Path,
    db_path: &Path,
    retention_days: u64,
    now_ts: i64,
    hard: bool,
    yes: bool,
    daemon_pid: Option<u32>,
) -> Result<()> {
    let port = crate::daemon::cli::configured_port(data_dir);
    let owner = daemon_pid.map(detect_daemon_owner);
    let ops = Arc::new(RealDaemonOps {
        data_dir: data_dir.to_path_buf(),
        port,
    });
    let restored = Arc::new(Mutex::new(daemon_pid.is_none()));

    let watcher_ops = ops.clone();
    let watcher_restored = restored.clone();
    let watcher = tokio::spawn(async move {
        if tokio::signal::ctrl_c().await.is_ok() {
            eprintln!("\n\x1b[33m⚠\x1b[0m  Interrupted — restoring the daemon before exiting...");
            let ok = restore_daemon_once(&watcher_restored, watcher_ops.as_ref(), owner);
            eprintln!(
                "{}",
                if ok {
                    "Daemon restored."
                } else {
                    "Failed to restore the daemon — check `canopy daemon status`."
                }
            );
            std::process::exit(130);
        }
    });

    let db_owned = db_path.to_path_buf();
    let db_dir_owned = data_dir.to_path_buf();
    let blocking_ops = ops.clone();
    let blocking_restored = restored.clone();
    let outcome = tokio::task::spawn_blocking({
        let db = Database::new_safe(&db_owned, &db_dir_owned)?;
        move || {
            run_reclaim_window(
                &db,
                &db_dir_owned,
                &db_owned,
                retention_days,
                now_ts,
                hard,
                yes,
                daemon_pid,
                blocking_ops.as_ref(),
                &blocking_restored,
            )
        }
    })
    .await;

    watcher.abort();

    let outcome = outcome.map_err(|e| anyhow::anyhow!("reclaim window task panicked: {e}"))??;
    print_reclaim_window_outcome(&outcome);
    Ok(())
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
            effort: None,
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
            archived: false,
            paused_by_reconciliation: false,
            infra_node_id: None,
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
            active_run_queue_id: None,
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

    // ── B2: the stop-daemon reclaim window ─────────────────────────────

    /// A PID that is never going to equal a real `systemd`/`launchd`-managed
    /// canopy PID on any machine this test runs on, so `detect_daemon_owner`
    /// deterministically resolves to `Detached` here — including on a dev
    /// box (like this one) that has a real `canopy.service` installed and
    /// running.
    const FAKE_DAEMON_PID: u32 = u32::MAX;

    struct FakeDaemonOps {
        calls: std::cell::RefCell<Vec<&'static str>>,
        stop_ok: bool,
        restart_ok: bool,
        stop_panics: bool,
    }

    impl FakeDaemonOps {
        fn new() -> Self {
            Self {
                calls: std::cell::RefCell::new(Vec::new()),
                stop_ok: true,
                restart_ok: true,
                stop_panics: false,
            }
        }
    }

    impl DaemonOps for FakeDaemonOps {
        fn stop(&self, _owner: DaemonOwnerKind) -> Result<()> {
            self.calls.borrow_mut().push("stop");
            if self.stop_panics {
                panic!("simulated stop failure");
            }
            if self.stop_ok {
                Ok(())
            } else {
                anyhow::bail!("simulated stop failure")
            }
        }

        fn restart(&self, _owner: DaemonOwnerKind) -> Result<()> {
            self.calls.borrow_mut().push("restart");
            if self.restart_ok {
                Ok(())
            } else {
                anyhow::bail!("simulated restart failure")
            }
        }
    }

    #[test]
    fn reclaim_window_refuses_when_a_loop_is_running() {
        let dir = tempdir().unwrap();
        let data_dir = dir.path();
        let db_path = data_dir.join("test.db");
        let db = Database::new(&db_path).unwrap();
        db.insert_loop(&make_loop("loop-1", "/tmp", LoopStatus::Running))
            .unwrap();

        let ops = FakeDaemonOps::new();
        let restored = Mutex::new(false);
        let outcome = run_reclaim_window(
            &db,
            data_dir,
            &db_path,
            7,
            chrono::Utc::now().timestamp(),
            false,
            true,
            Some(FAKE_DAEMON_PID),
            &ops,
            &restored,
        )
        .unwrap();

        match outcome {
            ReclaimWindowOutcome::Refused(reason) => {
                assert!(reason.contains("running"), "{reason}");
            }
            _ => panic!("expected Refused"),
        }
        assert!(
            ops.calls.borrow().is_empty(),
            "must not touch the daemon while busy"
        );
    }

    #[test]
    fn reclaim_window_refuses_when_a_tui_session_is_attached() {
        let dir = tempdir().unwrap();
        let data_dir = dir.path();
        let db_path = data_dir.join("test.db");
        let db = Database::new(&db_path).unwrap();
        db.insert_interactive_session(
            "s-live",
            "s-live",
            "opencode",
            "/tmp",
            None,
            None,
            "interactive",
            None,
        )
        .unwrap();

        let ops = FakeDaemonOps::new();
        let restored = Mutex::new(false);
        let outcome = run_reclaim_window(
            &db,
            data_dir,
            &db_path,
            7,
            chrono::Utc::now().timestamp(),
            false,
            true,
            Some(FAKE_DAEMON_PID),
            &ops,
            &restored,
        )
        .unwrap();

        match outcome {
            ReclaimWindowOutcome::Refused(reason) => {
                assert!(reason.contains("TUI"), "{reason}");
            }
            _ => panic!("expected Refused"),
        }
        assert!(ops.calls.borrow().is_empty());
    }

    #[test]
    fn reclaim_window_happy_path_stops_cleans_vacuums_and_restarts() {
        let dir = tempdir().unwrap();
        let data_dir = dir.path();
        let db_path = data_dir.join("test.db");
        let db = Database::new(&db_path).unwrap();
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

        let ops = FakeDaemonOps::new();
        let restored = Mutex::new(false);
        let now_ts = chrono::Utc::now().timestamp() + 30 * 86_400;
        let outcome = run_reclaim_window(
            &db,
            data_dir,
            &db_path,
            7,
            now_ts,
            false,
            true,
            Some(FAKE_DAEMON_PID),
            &ops,
            &restored,
        )
        .unwrap();

        match outcome {
            ReclaimWindowOutcome::Completed(report) => {
                assert!(report.daemon_was_running);
                assert_eq!(report.daemon_owner, Some(DaemonOwnerKind::Detached));
                assert_eq!(report.quick_check, "ok");
                assert_eq!(report.plan.session_ids, vec!["s-old".to_string()]);
                assert!(report.daemon_restored);
            }
            _ => panic!("expected Completed"),
        }
        assert_eq!(db.count_interactive_sessions().unwrap(), 0);
        assert_eq!(*ops.calls.borrow(), vec!["stop", "restart"]);
        assert!(*restored.lock().unwrap());
    }

    #[test]
    fn reclaim_window_skips_stop_and_restart_when_daemon_was_not_running() {
        let dir = tempdir().unwrap();
        let data_dir = dir.path();
        let db_path = data_dir.join("test.db");
        let db = Database::new(&db_path).unwrap();

        let ops = FakeDaemonOps::new();
        let restored = Mutex::new(false);
        let outcome = run_reclaim_window(
            &db,
            data_dir,
            &db_path,
            7,
            chrono::Utc::now().timestamp(),
            false,
            true,
            None,
            &ops,
            &restored,
        )
        .unwrap();

        match outcome {
            ReclaimWindowOutcome::Completed(report) => {
                assert!(!report.daemon_was_running);
                assert_eq!(report.daemon_owner, None);
                assert!(report.daemon_restored);
            }
            _ => panic!("expected Completed"),
        }
        assert!(
            ops.calls.borrow().is_empty(),
            "nothing to stop or restart when the daemon wasn't running"
        );
    }

    #[test]
    fn reclaim_window_aborts_on_quick_check_failure_and_still_restores() {
        let dir = tempdir().unwrap();
        let data_dir = dir.path();
        let db_path = data_dir.join("test.db");
        {
            let db = Database::new(&db_path).unwrap();
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
        }
        // Same corruption technique as `db::clean`'s `quick_check` test:
        // stomp on page data after every handle is dropped so the file has
        // a valid SQLite header but fails `quick_check`.
        let mut bytes = std::fs::read(&db_path).unwrap();
        // CM5: a fixed offset in page 3 rather than bytes.len()/2 — the
        // subagent_runs table shifted the file so the midpoint no longer
        // lands in a page quick_check validates.
        let start = 8192;
        let end = (start + 100).min(bytes.len());
        for b in &mut bytes[start..end] {
            *b ^= 0xFF;
        }
        std::fs::write(&db_path, &bytes).unwrap();

        let db = Database::new(&db_path).unwrap();
        let ops = FakeDaemonOps::new();
        let restored = Mutex::new(false);
        let now_ts = chrono::Utc::now().timestamp() + 30 * 86_400;
        let outcome = run_reclaim_window(
            &db,
            data_dir,
            &db_path,
            7,
            now_ts,
            false,
            true,
            Some(FAKE_DAEMON_PID),
            &ops,
            &restored,
        )
        .unwrap();

        match outcome {
            ReclaimWindowOutcome::QuickCheckFailed {
                verdict,
                daemon_restored,
            } => {
                assert_ne!(verdict, "ok");
                assert!(daemon_restored);
            }
            _ => panic!("expected QuickCheckFailed"),
        }
        // Nothing was modified: the existing cleanup never ran.
        assert_eq!(db.count_interactive_sessions().unwrap(), 1);
        assert_eq!(*ops.calls.borrow(), vec!["stop", "restart"]);
    }

    #[test]
    fn reclaim_window_restores_the_daemon_even_when_stop_panics() {
        let dir = tempdir().unwrap();
        let data_dir = dir.path();
        let db_path = data_dir.join("test.db");
        let db = Database::new(&db_path).unwrap();

        let ops = FakeDaemonOps {
            stop_panics: true,
            ..FakeDaemonOps::new()
        };
        let restored = Mutex::new(false);
        let now_ts = chrono::Utc::now().timestamp();

        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            run_reclaim_window(
                &db,
                data_dir,
                &db_path,
                7,
                now_ts,
                false,
                true,
                Some(FAKE_DAEMON_PID),
                &ops,
                &restored,
            )
        }));

        assert!(
            result.is_err(),
            "expected the simulated stop failure to panic"
        );
        assert!(
            *restored.lock().unwrap(),
            "the panic-safety guard must still restore the daemon"
        );
        assert_eq!(*ops.calls.borrow(), vec!["stop", "restart"]);
    }

    #[test]
    fn reclaim_window_reports_restore_failure_without_losing_the_quick_check_verdict() {
        let dir = tempdir().unwrap();
        let data_dir = dir.path();
        let db_path = data_dir.join("test.db");
        let mut bytes;
        {
            let db = Database::new(&db_path).unwrap();
            db.insert_terminal_session("t1", "t1", "bash", "/tmp")
                .unwrap();
            drop(db);
            bytes = std::fs::read(&db_path).unwrap();
        }
        // CM5: a fixed offset in page 3 rather than bytes.len()/2 — the
        // subagent_runs table shifted the file so the midpoint no longer
        // lands in a page quick_check validates.
        let start = 8192;
        let end = (start + 100).min(bytes.len());
        for b in &mut bytes[start..end] {
            *b ^= 0xFF;
        }
        std::fs::write(&db_path, &bytes).unwrap();

        let db = Database::new(&db_path).unwrap();
        let ops = FakeDaemonOps {
            restart_ok: false,
            ..FakeDaemonOps::new()
        };
        let restored = Mutex::new(false);
        let outcome = run_reclaim_window(
            &db,
            data_dir,
            &db_path,
            7,
            chrono::Utc::now().timestamp(),
            false,
            true,
            Some(FAKE_DAEMON_PID),
            &ops,
            &restored,
        )
        .unwrap();

        match outcome {
            ReclaimWindowOutcome::QuickCheckFailed {
                daemon_restored, ..
            } => {
                assert!(
                    !daemon_restored,
                    "a failed restart must be reported, not papered over"
                );
            }
            _ => panic!("expected QuickCheckFailed"),
        }
        assert!(
            !*restored.lock().unwrap(),
            "a failed restart leaves the shared flag false so a later attempt can retry"
        );
    }

    // ── restoration_feasible_from_facts (B-decision 5) ──────────────────
    //
    // Fed booleans directly rather than exercised through the real
    // `systemctl`/unit-file checks: those are absent on some CI containers
    // and present (and enabled) on this very dev box, so a test asserting a
    // specific refusal must not depend on which machine it runs on.

    #[test]
    fn restoration_feasible_ok_for_a_detached_owner() {
        assert!(
            restoration_feasible_from_facts(DaemonOwnerKind::Detached, false, false, false).is_ok()
        );
    }

    #[test]
    fn restoration_feasible_refuses_when_the_unit_is_missing() {
        let err =
            restoration_feasible_from_facts(DaemonOwnerKind::Managed("systemd"), false, true, true)
                .unwrap_err();
        assert!(err.contains("not installed"), "{err}");
    }

    #[test]
    fn restoration_feasible_refuses_when_the_systemd_session_is_unavailable() {
        let err =
            restoration_feasible_from_facts(DaemonOwnerKind::Managed("systemd"), true, false, true)
                .unwrap_err();
        assert!(err.contains("unavailable"), "{err}");
    }

    #[test]
    fn restoration_feasible_refuses_when_the_unit_is_disabled() {
        let err =
            restoration_feasible_from_facts(DaemonOwnerKind::Managed("systemd"), true, true, false)
                .unwrap_err();
        assert!(err.contains("disabled"), "{err}");
    }

    #[test]
    fn restoration_feasible_ok_when_the_unit_is_installed_available_and_enabled() {
        assert!(restoration_feasible_from_facts(
            DaemonOwnerKind::Managed("systemd"),
            true,
            true,
            true
        )
        .is_ok());
    }

    // ── no-consent / dry-run: the legacy path stays intact ──────────────

    #[test]
    fn no_consent_path_still_cleans_but_leaves_reclaim_to_the_legacy_gate() {
        let dir = tempdir().unwrap();
        let data_dir = dir.path();
        let db_path = data_dir.join("test.db");
        let db = Database::new(&db_path).unwrap();

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
        // Same trick as `reclaim_skipped_while_daemon_is_running`: name the
        // live test process so the daemon reads as running.
        std::fs::write(data_dir.join("daemon.pid"), std::process::id().to_string()).unwrap();

        let now_ts = chrono::Utc::now().timestamp() + 30 * 86_400;
        run_clean_without_window(
            &db, data_dir, &db_path, false, false, true, 7, now_ts, false,
        )
        .unwrap();

        // The existing cleanup still ran even though the (simulated) daemon
        // is up and no stop-daemon window was ever entered.
        assert_eq!(db.count_interactive_sessions().unwrap(), 0);
    }

    #[test]
    fn dry_run_projection_leaves_soft_and_hard_candidates_untouched() {
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

        let workdir = "/definitely/does/not/exist/for/projection";
        db.upsert_project(&make_project("hash-x", workdir)).unwrap();
        db.insert_loop(&make_loop("loop-x", workdir, LoopStatus::Completed))
            .unwrap();

        let now_ts = chrono::Utc::now().timestamp() + 30 * 86_400;

        let projected_plan = build_clean_plan(data_dir, &db, 7, now_ts).unwrap();
        let projected_hard = projected_hard_cascade_rows(&build_hard_cascade_plan(&db).unwrap());
        assert_eq!(projected_plan.session_ids.len(), 1);
        assert!(projected_hard > 0);

        // A dry run of the soft plan must not touch anything, regardless of
        // whether the projected total would warrant a reclaim window.
        let dry_plan = run_clean(data_dir, &db, true, 7, now_ts).unwrap();
        assert_eq!(dry_plan.session_ids, projected_plan.session_ids);
        assert_eq!(db.count_interactive_sessions().unwrap(), 1);
        assert!(
            db.get_project("hash-x").unwrap().is_some(),
            "projecting the hard cascade's row count must never execute it"
        );
    }
}
