//! CLI handlers for `canopy loop` subcommands.
//!
//! Read-only: mirrors `canopy rag report` (see `rag_cli.rs`) in spirit — a
//! terminal view onto state that previously required querying
//! `background_agents.db` directly.

use anyhow::{anyhow, Result};
use chrono::{DateTime, Utc};
use clap::Subcommand;

use crate::db::Database;
use crate::domain::loops::{
    Loop, LoopNodeRun, LoopRunStatus, LoopSpec, LoopSpecStatus, LoopStatus,
};

#[derive(Subcommand, Debug)]
pub(crate) enum LoopAction {
    /// List all loops with status and spec progress.
    List {
        /// Only show loops tagged with this workdir.
        #[arg(long)]
        workdir: Option<String>,
    },
    /// Show detailed status for a single loop.
    Info {
        /// Full loop id, an unambiguous id prefix, or the exact loop name.
        id_or_name: String,
    },
}

pub(crate) async fn handle_loop_action(action: LoopAction) -> Result<()> {
    let data_dir = crate::ensure_data_dir()?;
    let db = Database::new(&data_dir.join("background_agents.db"))?;

    match action {
        LoopAction::List { workdir } => handle_loop_list(&db, workdir.as_deref()),
        LoopAction::Info { id_or_name } => handle_loop_info(&db, &id_or_name),
    }
}

fn handle_loop_list(db: &Database, workdir: Option<&str>) -> Result<()> {
    let loops = db.list_loops(workdir)?;

    if loops.is_empty() {
        println!("No loops found.");
        return Ok(());
    }

    println!("\n\x1b[1m── Canopy Loops ───────────────────────────────────────────────\x1b[0m\n");
    for lp in &loops {
        let specs = db.list_loop_specs(&lp.id)?;
        let (done, total) = loop_progress(db, lp, &specs)?;

        let mut line = format!(
            " {} {}  \x1b[90m{}\x1b[0m  {:<9} {done}/{total}",
            status_icon(lp.status),
            lp.name,
            short_id(&lp.id),
            lp.status.as_str(),
        );

        if lp.status == LoopStatus::Running {
            if let Some(name) = current_spec_name(db, lp, &specs)? {
                line.push_str(&format!("  → {name}"));
            }
        }

        if let Some(at) = lp.autorun_at {
            line.push_str(&format!("  \x1b[36m{}\x1b[0m", format_autorun_compact(at)));
        }

        println!("{line}");
    }
    println!();
    Ok(())
}

fn handle_loop_info(db: &Database, id_or_name: &str) -> Result<()> {
    let loops = db.list_loops(None)?;
    let lp = resolve_loop(&loops, id_or_name)?;

    println!("\n\x1b[1m── Loop: {} ──\x1b[0m", lp.name);
    println!(" id:      {}", lp.id);
    println!(
        " status:  {} {}",
        status_icon(lp.status),
        lp.status.as_str()
    );
    println!(" workdir: {}", lp.workdir);
    print!(" trigger: {}", lp.trigger_type_label());
    if let Some(expr) = lp.schedule_expr() {
        print!(" ({expr})");
    }
    if let Some(path) = lp.watch_path() {
        print!(" ({path})");
    }
    println!();
    if let Some(at) = lp.autorun_at {
        println!(
            " autorun: {} ({})",
            at.to_rfc3339(),
            format_relative_duration(at - Utc::now())
        );
    }
    if let Some(hook) = &lp.on_completed {
        println!(
            " on_completed: {} ({})",
            hook.platform,
            hook.model.as_deref().unwrap_or("default model")
        );
    }

    let specs = db.list_loop_specs(&lp.id)?;
    let all_runs = db.list_loop_runs_for_loop(&lp.id)?;
    let hook_runs = db.list_loop_completion_hook_runs(&lp.id)?;

    println!("\n\x1b[1m── Specs ──────────────────────────────────────────────────────\x1b[0m");
    if !specs.is_empty() {
        for spec in &specs {
            let admin_tag = if spec.completed_via.as_deref() == Some("admin") {
                " (admin)"
            } else {
                ""
            };
            println!(
                " {} {}{}",
                spec_status_icon(spec.status),
                spec.name,
                admin_tag
            );
        }
    } else if !all_runs.is_empty() {
        // No specs are bound to this loop directly — it's draining a pool
        // (pool members never set `loop_specs.loop_id`, see `LoopEngine::
        // run_loop`), so there's no fixed queue to show. Reconstruct what
        // ran so far from `loop_runs`, which always records the real
        // `loop_id` regardless of pool membership.
        println!(" (queue-driven — showing specs worked so far, not the full queue)");
        for spec_id in distinct_spec_ids_in_order(&all_runs) {
            if let Some(spec) = db.get_loop_spec(spec_id)? {
                let admin_tag = if spec.completed_via.as_deref() == Some("admin") {
                    " (admin)"
                } else {
                    ""
                };
                println!(
                    " {} {}{}",
                    spec_status_icon(spec.status),
                    spec.name,
                    admin_tag
                );
            }
        }
    } else {
        println!(" (no specs queued)");
    }

    // A `running`-status run row can outlive its loop (e.g. a run left over
    // from before `reconcile_orphaned_loops` existed to clean these up), so
    // only trust it as "current" while the loop itself is actually running —
    // otherwise a completed/failed loop could misreport an old node as still
    // in flight.
    if lp.status == LoopStatus::Running {
        if let Some(run) = current_running_run(&all_runs) {
            let node = db.get_loop_node(&run.node_id)?;
            let node_name = node.as_ref().map_or(run.node_id.as_str(), |n| &n.name);
            let node_kind = node.as_ref().map_or("?", |n| n.kind.display_str());
            let elapsed = format_elapsed(Utc::now() - run.started_at);
            println!(
                "\n\x1b[1m── Current Node ───────────────────────────────────────────────\x1b[0m"
            );
            println!(
                " {} ({})  running {}  iteration {}",
                node_name, node_kind, elapsed, run.iteration
            );
            // Surfaces the exact baseline `{{spec_start_head}}` resolved to
            // for this spec's current attempt (B10) — the one number every
            // "why did this check pass/fail" debugging session needs.
            if let Some(spec) = db.get_loop_spec(&run.spec_id)? {
                match spec.spec_start_head {
                    Some(head) => println!(" spec_start_head: {head}"),
                    None => println!(" spec_start_head: (not a git workdir)"),
                }
            }
        }
    }

    println!("\n\x1b[1m── Recent Node Runs ───────────────────────────────────────────\x1b[0m");
    if all_runs.is_empty() {
        println!(" (no runs yet)");
    }
    for run in all_runs.iter().rev().take(5) {
        let node_name = db
            .get_loop_node(&run.node_id)?
            .map_or_else(|| run.node_id.clone(), |n| n.name);
        let sid = run
            .session_id
            .as_deref()
            .map(|sid| format!("  sid {}", sid.chars().take(8).collect::<String>()))
            .unwrap_or_default();
        // RS3: flag a run that continued a context group's warm session (its
        // session id was first captured by a grouped sibling on this node).
        let group_note = match (lp.active_run_pool_id.as_deref(), run.session_id.as_deref()) {
            (Some(pool_id), Some(session_id)) => db
                .group_resume_source(pool_id, &run.spec_id, &run.node_id, session_id)?
                .map(|group| format!("  \x1b[36m(resumed group {group})\x1b[0m"))
                .unwrap_or_default(),
            _ => String::new(),
        };
        // B37: a node that moved git HEAD without `commit_rights` was failed
        // by the engine, not by its own verdict — say so, otherwise the run
        // reads as an ordinary fail and the operator hunts the wrong cause.
        let commit_note = commit_rights_note(run.output.as_ref());
        // B36: a run the daemon marked `fail` only because it was cut short
        // by a restart/kill must never read as "the node failed" — flag it
        // with its own icon and note, including how to recover any
        // quarantined worktree changes.
        let interrupted_note = interrupted_note(run.output.as_ref());
        println!(
            " {} {}  {}{}{}{}{}",
            run_status_icon(run.status, run.output.as_ref()),
            node_name,
            format_dt(run.started_at),
            sid,
            group_note,
            commit_note,
            interrupted_note
        );
    }

    if !hook_runs.is_empty() {
        println!("\n\x1b[1m── on_completed Hook Runs ─────────────────────────────────────\x1b[0m");
        for run in hook_runs.iter().rev().take(5) {
            println!(
                " {} {}",
                run_status_icon(run.status, run.output.as_ref()),
                format_dt(run.started_at)
            );
        }
    }
    println!();
    Ok(())
}

/// The `loop info` annotation for a run the engine failed on a commit-rights
/// violation (B37), or an empty string for every other run. Reads the marker
/// the engine writes into the run's output rather than re-running git, so it
/// stays true long after the fact.
fn commit_rights_note(output: Option<&serde_json::Value>) -> String {
    let Some(violation) = output.and_then(|o| o.get("commit_rights_violation")) else {
        return String::new();
    };
    let head_after = violation
        .get("head_after")
        .and_then(|v| v.as_str())
        .unwrap_or("?");
    format!(
        "  \x1b[31m(no commit rights — committed {})\x1b[0m",
        head_after.chars().take(8).collect::<String>()
    )
}

/// The `loop info` annotation for a run cut short by a daemon restart/kill
/// (B36), or an empty string for every other run — pairs with the distinct
/// icon `run_status_icon` already gives it. Also surfaces the recovery hint
/// when uncommitted changes were quarantined via `git stash`, so a `fail`
/// here never reads as the node's own doing.
fn interrupted_note(output: Option<&serde_json::Value>) -> String {
    if !is_interrupted(output) {
        return String::new();
    }
    match output.and_then(|o| o.get("quarantine")) {
        Some(q) if q.get("stashed").and_then(|v| v.as_bool()) == Some(true) => {
            "  \x1b[33m(interrupted — worktree changes quarantined, recover with `git stash pop`)\x1b[0m".to_string()
        }
        _ => "  \x1b[33m(interrupted — not a node failure)\x1b[0m".to_string(),
    }
}

/// Count of specs that have reached a final `completed` state, alongside the
/// total — used to render `done/total` progress in `loop list`/`loop info`.
fn spec_progress(specs: &[LoopSpec]) -> (usize, usize) {
    let done = specs
        .iter()
        .filter(|s| s.status == LoopSpecStatus::Completed)
        .count();
    (done, specs.len())
}

/// `done/total` progress for a loop's `loop list` row. A pool-driven run binds
/// no specs of its own (`loop_specs.loop_id` stays null for pool members), so
/// counting `bound_specs` renders a misleading `0/0`. When the loop's row
/// carries an `active_run_pool_id`, count that pool's members instead —
/// mirroring `LoopEngine::spec_progress`, which the running engine uses for the
/// same loop. Falls back to the bound specs for ordinary (non-pool) loops.
fn loop_progress(db: &Database, lp: &Loop, bound_specs: &[LoopSpec]) -> Result<(usize, usize)> {
    let Some(pool_id) = lp.active_run_pool_id.as_deref() else {
        return Ok(spec_progress(bound_specs));
    };
    let ids = db.list_pool_member_spec_ids(pool_id)?;
    let mut done = 0;
    for id in &ids {
        if let Some(spec) = db.get_loop_spec(id)? {
            if spec.status == LoopSpecStatus::Completed {
                done += 1;
            }
        }
    }
    Ok((done, ids.len()))
}

/// The spec a loop is actively working through: the one currently `running`,
/// or else the next `pending` one in position order. Mirrors
/// `build_loop_summary_json` in `daemon/handler.rs` so the CLI and MCP report
/// the same "current spec" for a given loop.
fn current_spec(specs: &[LoopSpec]) -> Option<&LoopSpec> {
    specs
        .iter()
        .find(|s| s.status == LoopSpecStatus::Running)
        .or_else(|| specs.iter().find(|s| s.status == LoopSpecStatus::Pending))
}

/// Name of the spec a loop is actively working through, for both bound-spec
/// loops (via [`current_spec`]) and pool-driven loops, which never bind a
/// spec to `loop_specs.loop_id` and so must fall back to `loop_runs` (see
/// [`Database::list_loop_runs_for_loop`]) to find what's currently running.
fn current_spec_name(db: &Database, lp: &Loop, bound_specs: &[LoopSpec]) -> Result<Option<String>> {
    if let Some(spec) = current_spec(bound_specs) {
        return Ok(Some(spec.name.clone()));
    }
    let runs = db.list_loop_runs_for_loop(&lp.id)?;
    let Some(run) = current_running_run(&runs).or_else(|| runs.last()) else {
        return Ok(None);
    };
    Ok(db.get_loop_spec(&run.spec_id)?.map(|s| s.name))
}

/// The run currently in flight, if any — assumes `runs` is ordered by
/// `started_at` ascending (as every `list_loop_runs_for_*` query returns it),
/// so the last matching entry is the most recent.
fn current_running_run(runs: &[LoopNodeRun]) -> Option<&LoopNodeRun> {
    runs.iter()
        .rev()
        .find(|r| r.status == LoopRunStatus::Running)
}

/// Spec ids referenced by `runs`, in first-seen (chronological) order with
/// duplicates dropped — used to approximate a pool-driven loop's queue from
/// its run history, since the queue itself isn't persisted per loop.
fn distinct_spec_ids_in_order(runs: &[LoopNodeRun]) -> Vec<&str> {
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    for run in runs {
        if seen.insert(run.spec_id.as_str()) {
            out.push(run.spec_id.as_str());
        }
    }
    out
}

/// Resolve a user-supplied loop reference against the full loop set: an exact
/// id match wins first, then an exact (necessarily unique) name match, then
/// an unambiguous id prefix. Anything else is an actionable error listing the
/// candidates the caller could have meant.
fn resolve_loop<'a>(loops: &'a [Loop], query: &str) -> Result<&'a Loop> {
    let query = query.trim();
    if query.is_empty() {
        return Err(anyhow!("Loop id or name must not be empty."));
    }

    if let Some(lp) = loops.iter().find(|l| l.id == query) {
        return Ok(lp);
    }

    let name_matches: Vec<&Loop> = loops.iter().filter(|l| l.name == query).collect();
    match name_matches.len() {
        1 => return Ok(name_matches[0]),
        n if n > 1 => return Err(ambiguous_error(query, &name_matches)),
        _ => {}
    }

    let prefix_matches: Vec<&Loop> = loops.iter().filter(|l| l.id.starts_with(query)).collect();
    match prefix_matches.len() {
        1 => Ok(prefix_matches[0]),
        0 => Err(not_found_error(query, loops)),
        _ => Err(ambiguous_error(query, &prefix_matches)),
    }
}

fn not_found_error(query: &str, all: &[Loop]) -> anyhow::Error {
    if all.is_empty() {
        return anyhow!("No loop matches '{query}' — no loops exist yet.");
    }
    anyhow!(
        "No loop matches '{query}'. Available loops:\n{}",
        candidate_list(all.iter())
    )
}

fn ambiguous_error(query: &str, matches: &[&Loop]) -> anyhow::Error {
    anyhow!(
        "'{query}' matches multiple loops:\n{}\nUse the full id to disambiguate.",
        candidate_list(matches.iter().copied())
    )
}

fn candidate_list<'a>(loops: impl Iterator<Item = &'a Loop>) -> String {
    loops
        .map(|l| format!("  {} ({})", l.name, short_id(&l.id)))
        .collect::<Vec<_>>()
        .join("\n")
}

fn short_id(id: &str) -> String {
    id.chars().take(8).collect()
}

fn status_icon(status: LoopStatus) -> &'static str {
    match status {
        LoopStatus::Running => "\x1b[36m▶\x1b[0m",
        LoopStatus::Paused => "\x1b[33m⏸\x1b[0m",
        LoopStatus::Completed => "\x1b[32m✓\x1b[0m",
        LoopStatus::Failed => "\x1b[31m✗\x1b[0m",
        LoopStatus::Draft => "\x1b[90m●\x1b[0m",
    }
}

fn spec_status_icon(status: LoopSpecStatus) -> &'static str {
    match status {
        LoopSpecStatus::Running => "\x1b[36m▶\x1b[0m",
        LoopSpecStatus::Completed => "\x1b[32m✓\x1b[0m",
        LoopSpecStatus::Failed => "\x1b[31m✗\x1b[0m",
        LoopSpecStatus::Skipped => "\x1b[90m⊘\x1b[0m",
        LoopSpecStatus::Pending => "\x1b[90m●\x1b[0m",
    }
}

/// B36: a run cut short by a daemon restart/kill is recorded as `fail` (see
/// `reconcile_orphaned_loops` / `reconcile_stranded_pool_specs`), but it must
/// never render like an ordinary node failure — its own icon, distinct from
/// both a genuine fail and a healthy pass.
fn run_status_icon(status: LoopRunStatus, output: Option<&serde_json::Value>) -> &'static str {
    if is_interrupted(output) {
        return "\x1b[33m⚑\x1b[0m";
    }
    match status {
        LoopRunStatus::Running => "\x1b[36m▶\x1b[0m",
        LoopRunStatus::Pass => "\x1b[32m✓\x1b[0m",
        LoopRunStatus::Fail => "\x1b[31m✗\x1b[0m",
    }
}

fn is_interrupted(output: Option<&serde_json::Value>) -> bool {
    output
        .and_then(|o| o.get("interrupted"))
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
}

fn format_dt(dt: DateTime<Utc>) -> String {
    dt.with_timezone(&chrono::Local)
        .format("%Y-%m-%d %H:%M")
        .to_string()
}

/// Compact `⏲ HH:MMZ` badge for a scheduled autorun, used in `loop list` rows.
fn format_autorun_compact(at: DateTime<Utc>) -> String {
    format!("⏲ {}", at.format("%H:%MZ"))
}

/// Human relative hint for a scheduled autorun, e.g. "in 2h 14m", used
/// alongside the ISO8601 timestamp in `loop info`.
fn format_relative_duration(d: chrono::Duration) -> String {
    let secs = d.num_seconds();
    if secs <= 0 {
        "due now".to_string()
    } else if secs < 60 {
        format!("in {secs}s")
    } else if secs < 3600 {
        format!("in {}m", secs / 60)
    } else if secs < 86400 {
        let hours = secs / 3600;
        let mins = (secs % 3600) / 60;
        if mins == 0 {
            format!("in {hours}h")
        } else {
            format!("in {hours}h {mins}m")
        }
    } else {
        format!("in {}d", secs / 86400)
    }
}

fn format_elapsed(d: chrono::Duration) -> String {
    let secs = d.num_seconds().max(0);
    if secs < 60 {
        format!("{secs}s")
    } else if secs < 3600 {
        format!("{}m{}s", secs / 60, secs % 60)
    } else {
        format!("{}h{}m", secs / 3600, (secs % 3600) / 60)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[derive(Parser)]
    struct TestCli {
        #[command(subcommand)]
        action: LoopAction,
    }

    #[test]
    fn list_parses_with_and_without_workdir() {
        let cli = TestCli::try_parse_from(["test", "list"]).expect("list should parse");
        assert!(matches!(cli.action, LoopAction::List { workdir: None }));

        let cli = TestCli::try_parse_from(["test", "list", "--workdir", "/tmp/proj"])
            .expect("list --workdir should parse");
        match cli.action {
            LoopAction::List { workdir } => assert_eq!(workdir.as_deref(), Some("/tmp/proj")),
            other => panic!("expected List, got {other:?}"),
        }
    }

    #[test]
    fn info_requires_id_or_name() {
        assert!(TestCli::try_parse_from(["test", "info"]).is_err());
        let cli = TestCli::try_parse_from(["test", "info", "my-loop"]).expect("should parse");
        match cli.action {
            LoopAction::Info { id_or_name } => assert_eq!(id_or_name, "my-loop"),
            other => panic!("expected Info, got {other:?}"),
        }
    }

    fn make_loop(id: &str, name: &str, status: LoopStatus) -> Loop {
        Loop {
            id: id.to_string(),
            name: name.to_string(),
            description: None,
            workdir: "/tmp".to_string(),
            status,
            trigger: None,
            created_at: Utc::now(),
            started_at: None,
            completed_at: None,
            autorun_at: None,
            auto_continue_at: None,
            auto_continue_action: None,
            active_run_pool_id: None,
            on_completed: None,
        }
    }

    fn make_spec(loop_id: &str, name: &str, position: i64, status: LoopSpecStatus) -> LoopSpec {
        LoopSpec {
            id: format!("{loop_id}-{position}"),
            loop_id: Some(loop_id.to_string()),
            name: name.to_string(),
            description: None,
            position,
            parallelizable: false,
            status,
            started_at: None,
            completed_at: None,
            spec_start_head: None,
            workdir: None,
            completed_via: None,
            completed_via_reason: None,
            completed_via_at: None,
        }
    }

    #[test]
    fn spec_progress_counts_only_completed_as_done() {
        let specs = vec![
            make_spec("l1", "a", 0, LoopSpecStatus::Completed),
            make_spec("l1", "b", 1, LoopSpecStatus::Running),
            make_spec("l1", "c", 2, LoopSpecStatus::Pending),
            make_spec("l1", "d", 3, LoopSpecStatus::Skipped),
        ];
        assert_eq!(spec_progress(&specs), (1, 4));
    }

    #[test]
    fn spec_progress_empty_is_zero_of_zero() {
        assert_eq!(spec_progress(&[]), (0, 0));
    }

    #[test]
    fn loop_progress_counts_pool_members_when_active_run_pool_id_set() {
        use crate::domain::pools::Pool;

        let dir = tempfile::tempdir().unwrap();
        let db = Database::new(&dir.path().join("t.db")).unwrap();

        // A pool-driven loop binds no specs of its own, so counting bound
        // specs would render a misleading 0/0.
        let mut lp = make_loop("loop-1", "pool-loop", LoopStatus::Running);
        lp.active_run_pool_id = Some("pool-1".to_string());
        db.insert_loop(&lp).unwrap();
        db.insert_pool(&Pool {
            id: "pool-1".to_string(),
            name: "P".to_string(),
            created_at: Utc::now(),
        })
        .unwrap();

        // Two standalone pool members (loop_id: None, like real ones): one
        // completed, one pending.
        for (id, status) in [
            ("spec-a", LoopSpecStatus::Completed),
            ("spec-b", LoopSpecStatus::Pending),
        ] {
            let mut spec = make_spec("pool", id, 0, status);
            spec.id = id.to_string();
            spec.loop_id = None;
            db.insert_loop_spec(&spec).unwrap();
            db.append_pool_member("pool-1", id, None).unwrap();
        }

        // Bound specs empty; pool progress is 1/2.
        assert_eq!(loop_progress(&db, &lp, &[]).unwrap(), (1, 2));
    }

    #[test]
    fn loop_progress_shows_n_of_n_for_completed_pool_loop() {
        use crate::domain::pools::Pool;

        let dir = tempfile::tempdir().unwrap();
        let db = Database::new(&dir.path().join("t.db")).unwrap();

        // B31: a finished pool-driven loop keeps its `active_run_pool_id`, so
        // even in a terminal status it must still render its real n/n queue
        // progress rather than the 0/0 a bound-spec count would produce.
        let mut lp = make_loop("loop-done", "pool-loop", LoopStatus::Completed);
        lp.active_run_pool_id = Some("pool-1".to_string());
        db.insert_loop(&lp).unwrap();
        db.insert_pool(&Pool {
            id: "pool-1".to_string(),
            name: "P".to_string(),
            created_at: Utc::now(),
        })
        .unwrap();

        for id in ["spec-a", "spec-b"] {
            let mut spec = make_spec("pool", id, 0, LoopSpecStatus::Completed);
            spec.id = id.to_string();
            spec.loop_id = None;
            db.insert_loop_spec(&spec).unwrap();
            db.append_pool_member("pool-1", id, None).unwrap();
        }

        assert_eq!(loop_progress(&db, &lp, &[]).unwrap(), (2, 2));
    }

    #[test]
    fn loop_progress_falls_back_to_bound_specs_without_pool() {
        let dir = tempfile::tempdir().unwrap();
        let db = Database::new(&dir.path().join("t.db")).unwrap();
        let lp = make_loop("loop-2", "bound", LoopStatus::Running);
        let specs = vec![
            make_spec("loop-2", "a", 0, LoopSpecStatus::Completed),
            make_spec("loop-2", "b", 1, LoopSpecStatus::Pending),
        ];
        assert_eq!(loop_progress(&db, &lp, &specs).unwrap(), (1, 2));
    }

    #[test]
    fn current_spec_prefers_running_over_pending() {
        let specs = vec![
            make_spec("l1", "a", 0, LoopSpecStatus::Completed),
            make_spec("l1", "b", 1, LoopSpecStatus::Running),
            make_spec("l1", "c", 2, LoopSpecStatus::Pending),
        ];
        assert_eq!(current_spec(&specs).map(|s| s.name.as_str()), Some("b"));
    }

    #[test]
    fn current_spec_falls_back_to_next_pending() {
        let specs = vec![
            make_spec("l1", "a", 0, LoopSpecStatus::Completed),
            make_spec("l1", "c", 2, LoopSpecStatus::Pending),
        ];
        assert_eq!(current_spec(&specs).map(|s| s.name.as_str()), Some("c"));
    }

    #[test]
    fn current_spec_none_when_all_terminal() {
        let specs = vec![
            make_spec("l1", "a", 0, LoopSpecStatus::Completed),
            make_spec("l1", "b", 1, LoopSpecStatus::Failed),
        ];
        assert!(current_spec(&specs).is_none());
    }

    fn make_run(
        spec_id: &str,
        node_id: &str,
        status: LoopRunStatus,
        started_at_secs: i64,
    ) -> LoopNodeRun {
        LoopNodeRun {
            id: format!("{spec_id}-{node_id}-{started_at_secs}"),
            loop_id: "l1".to_string(),
            spec_id: spec_id.to_string(),
            node_id: node_id.to_string(),
            status,
            input: None,
            output: None,
            started_at: DateTime::<Utc>::from_timestamp(started_at_secs, 0).unwrap(),
            completed_at: None,
            iteration: 1,
            pid: None,
            boot_id: None,
            session_id: None,
        }
    }

    #[test]
    fn current_running_run_picks_the_in_flight_one() {
        let runs = vec![
            make_run("s1", "n1", LoopRunStatus::Pass, 1),
            make_run("s2", "n2", LoopRunStatus::Running, 2),
        ];
        assert_eq!(
            current_running_run(&runs).map(|r| r.node_id.as_str()),
            Some("n2")
        );
    }

    #[test]
    fn current_running_run_none_when_all_terminal() {
        let runs = vec![
            make_run("s1", "n1", LoopRunStatus::Pass, 1),
            make_run("s2", "n2", LoopRunStatus::Fail, 2),
        ];
        assert!(current_running_run(&runs).is_none());
    }

    #[test]
    fn distinct_spec_ids_in_order_dedupes_preserving_first_seen() {
        let runs = vec![
            make_run("s1", "n1", LoopRunStatus::Pass, 1),
            make_run("s2", "n1", LoopRunStatus::Pass, 2),
            make_run("s1", "n2", LoopRunStatus::Pass, 3),
        ];
        assert_eq!(distinct_spec_ids_in_order(&runs), vec!["s1", "s2"]);
    }

    #[test]
    fn distinct_spec_ids_in_order_empty() {
        assert!(distinct_spec_ids_in_order(&[]).is_empty());
    }

    #[test]
    fn resolve_loop_by_exact_id() {
        let loops = vec![
            make_loop("abc123", "one", LoopStatus::Draft),
            make_loop("def456", "two", LoopStatus::Draft),
        ];
        let resolved = resolve_loop(&loops, "def456").unwrap();
        assert_eq!(resolved.name, "two");
    }

    #[test]
    fn resolve_loop_by_exact_name() {
        let loops = vec![
            make_loop("abc123", "one", LoopStatus::Draft),
            make_loop("def456", "two", LoopStatus::Draft),
        ];
        let resolved = resolve_loop(&loops, "two").unwrap();
        assert_eq!(resolved.id, "def456");
    }

    #[test]
    fn resolve_loop_by_unambiguous_prefix() {
        let loops = vec![
            make_loop("abc123", "one", LoopStatus::Draft),
            make_loop("def456", "two", LoopStatus::Draft),
        ];
        let resolved = resolve_loop(&loops, "abc").unwrap();
        assert_eq!(resolved.name, "one");
    }

    #[test]
    fn resolve_loop_ambiguous_prefix_lists_candidates() {
        let loops = vec![
            make_loop("abc123", "one", LoopStatus::Draft),
            make_loop("abc789", "two", LoopStatus::Draft),
        ];
        let err = resolve_loop(&loops, "abc").unwrap_err().to_string();
        assert!(err.contains("multiple loops"));
        assert!(err.contains("one"));
        assert!(err.contains("two"));
    }

    #[test]
    fn resolve_loop_ambiguous_name_lists_candidates() {
        let loops = vec![
            make_loop("abc123", "dup", LoopStatus::Draft),
            make_loop("def456", "dup", LoopStatus::Draft),
        ];
        let err = resolve_loop(&loops, "dup").unwrap_err().to_string();
        assert!(err.contains("multiple loops"));
        assert!(err.contains("abc123"));
        assert!(err.contains("def456"));
    }

    #[test]
    fn resolve_loop_not_found_lists_all_candidates() {
        let loops = vec![make_loop("abc123", "one", LoopStatus::Draft)];
        let err = resolve_loop(&loops, "missing").unwrap_err().to_string();
        assert!(err.contains("No loop matches 'missing'"));
        assert!(err.contains("one"));
    }

    #[test]
    fn resolve_loop_not_found_on_empty_set() {
        let err = resolve_loop(&[], "anything").unwrap_err().to_string();
        assert!(err.contains("no loops exist yet"));
    }

    #[test]
    fn resolve_loop_rejects_empty_query() {
        let loops = vec![make_loop("abc123", "one", LoopStatus::Draft)];
        assert!(resolve_loop(&loops, "").is_err());
        assert!(resolve_loop(&loops, "   ").is_err());
    }

    #[test]
    fn status_icons_are_distinct_per_status() {
        let statuses = [
            LoopStatus::Draft,
            LoopStatus::Running,
            LoopStatus::Paused,
            LoopStatus::Completed,
            LoopStatus::Failed,
        ];
        let icons: std::collections::HashSet<&str> =
            statuses.iter().map(|s| status_icon(*s)).collect();
        assert_eq!(icons.len(), statuses.len());
    }

    #[test]
    fn format_elapsed_buckets() {
        assert_eq!(format_elapsed(chrono::Duration::seconds(5)), "5s");
        assert_eq!(format_elapsed(chrono::Duration::seconds(65)), "1m5s");
        assert_eq!(format_elapsed(chrono::Duration::seconds(3661)), "1h1m");
    }

    #[test]
    fn format_relative_duration_buckets() {
        assert_eq!(
            format_relative_duration(chrono::Duration::seconds(30)),
            "in 30s"
        );
        assert_eq!(
            format_relative_duration(chrono::Duration::minutes(1)),
            "in 1m"
        );
        assert_eq!(
            format_relative_duration(chrono::Duration::minutes(134)),
            "in 2h 14m"
        );
        assert_eq!(
            format_relative_duration(chrono::Duration::hours(3)),
            "in 3h"
        );
        assert_eq!(
            format_relative_duration(chrono::Duration::hours(30)),
            "in 1d"
        );
    }

    #[test]
    fn format_relative_duration_past_or_now_reads_due_now() {
        assert_eq!(
            format_relative_duration(chrono::Duration::seconds(0)),
            "due now"
        );
        assert_eq!(
            format_relative_duration(chrono::Duration::seconds(-5)),
            "due now"
        );
    }

    #[test]
    fn format_autorun_compact_renders_utc_badge() {
        let at = DateTime::<Utc>::from_timestamp(5 * 3600, 0).unwrap(); // 05:00:00 UTC
        assert_eq!(format_autorun_compact(at), "⏲ 05:00Z");
    }

    #[test]
    fn short_id_truncates_to_eight_chars() {
        assert_eq!(short_id("abcdef1234567890"), "abcdef12");
        assert_eq!(short_id("short"), "short");
        assert_eq!(short_id(""), "");
    }

    #[test]
    fn spec_status_icons_are_distinct_per_status() {
        let statuses = [
            LoopSpecStatus::Running,
            LoopSpecStatus::Completed,
            LoopSpecStatus::Failed,
            LoopSpecStatus::Skipped,
            LoopSpecStatus::Pending,
        ];
        let icons: std::collections::HashSet<&str> =
            statuses.iter().map(|s| spec_status_icon(*s)).collect();
        assert_eq!(icons.len(), statuses.len());
    }

    #[test]
    fn run_status_icon_shows_flag_for_interrupted_runs() {
        let output = Some(serde_json::json!({"interrupted": true}));
        assert!(run_status_icon(LoopRunStatus::Fail, output.as_ref()).contains("⚑"));
        assert!(run_status_icon(LoopRunStatus::Pass, output.as_ref()).contains("⚑"));
        assert!(run_status_icon(LoopRunStatus::Running, output.as_ref()).contains("⚑"));
    }

    #[test]
    fn run_status_icon_shows_normal_icons_when_not_interrupted() {
        let output = Some(serde_json::json!({"interrupted": false}));
        assert!(run_status_icon(LoopRunStatus::Pass, output.as_ref()).contains("✓"));
        assert!(run_status_icon(LoopRunStatus::Fail, output.as_ref()).contains("✗"));
        assert!(run_status_icon(LoopRunStatus::Running, output.as_ref()).contains("▶"));
    }

    #[test]
    fn run_status_icon_handles_none_output() {
        assert!(run_status_icon(LoopRunStatus::Pass, None).contains("✓"));
        assert!(run_status_icon(LoopRunStatus::Fail, None).contains("✗"));
        assert!(run_status_icon(LoopRunStatus::Running, None).contains("▶"));
    }

    #[test]
    fn is_interrupted_detects_flag() {
        assert!(is_interrupted(Some(
            &serde_json::json!({"interrupted": true})
        )));
        assert!(!is_interrupted(Some(
            &serde_json::json!({"interrupted": false})
        )));
        assert!(!is_interrupted(Some(
            &serde_json::json!({"other": "field"})
        )));
        assert!(!is_interrupted(None));
    }

    #[test]
    fn commit_rights_note_renders_violation() {
        let output = Some(serde_json::json!({
            "commit_rights_violation": {"head_after": "abcdef1234567890"}
        }));
        let note = commit_rights_note(output.as_ref());
        assert!(note.contains("no commit rights"));
        assert!(note.contains("abcdef12"));
    }

    #[test]
    fn commit_rights_note_empty_when_no_violation() {
        assert!(commit_rights_note(None).is_empty());
        assert!(commit_rights_note(Some(&serde_json::json!({"other": "field"}))).is_empty());
    }

    #[test]
    fn interrupted_note_renders_with_quarantine() {
        let output = Some(serde_json::json!({
            "interrupted": true,
            "quarantine": {"stashed": true}
        }));
        let note = interrupted_note(output.as_ref());
        assert!(note.contains("interrupted"));
        assert!(note.contains("quarantined"));
        assert!(note.contains("git stash pop"));
    }

    #[test]
    fn interrupted_note_renders_without_quarantine() {
        let output = Some(serde_json::json!({"interrupted": true}));
        let note = interrupted_note(output.as_ref());
        assert!(note.contains("interrupted"));
        assert!(!note.contains("quarantined"));
    }

    #[test]
    fn interrupted_note_empty_when_not_interrupted() {
        assert!(interrupted_note(None).is_empty());
        assert!(interrupted_note(Some(&serde_json::json!({"interrupted": false}))).is_empty());
    }

    #[test]
    fn format_dt_renders_local_time() {
        let dt = DateTime::<Utc>::from_timestamp(0, 0).unwrap();
        let formatted = format_dt(dt);
        // The function converts to local time, so we just check it's non-empty and has a colon
        assert!(!formatted.is_empty());
        assert!(formatted.contains(":"));
    }

    #[test]
    fn candidate_list_renders_loop_names() {
        let loops = [
            make_loop("abc123", "loop-one", LoopStatus::Draft),
            make_loop("def456", "loop-two", LoopStatus::Running),
        ];
        let list = candidate_list(loops.iter());
        assert!(list.contains("loop-one"));
        assert!(list.contains("loop-two"));
        assert!(list.contains("abc123"));
        assert!(list.contains("def456"));
    }

    #[test]
    fn not_found_error_mentions_query() {
        let loops = [make_loop("abc123", "one", LoopStatus::Draft)];
        let err = not_found_error("missing", &loops);
        assert!(err.to_string().contains("missing"));
    }

    #[test]
    fn ambiguous_error_mentions_all_candidates() {
        let loops = [
            make_loop("abc123", "dup", LoopStatus::Draft),
            make_loop("def456", "dup", LoopStatus::Draft),
        ];
        let matches: Vec<&Loop> = loops.iter().collect();
        let err = ambiguous_error("dup", &matches);
        let msg = err.to_string();
        assert!(msg.contains("abc123"));
        assert!(msg.contains("def456"));
    }

    #[test]
    fn current_spec_name_returns_none_for_empty_specs() {
        let dir = tempfile::tempdir().unwrap();
        let db = Database::new(&dir.path().join("test.db")).unwrap();
        let lp = make_loop("loop1", "test-loop", LoopStatus::Running);
        let result = current_spec_name(&db, &lp, &[]).unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn current_spec_name_returns_name_for_running_spec() {
        let dir = tempfile::tempdir().unwrap();
        let db = Database::new(&dir.path().join("test.db")).unwrap();
        let lp = make_loop("loop1", "test-loop", LoopStatus::Running);
        let specs = vec![
            make_spec("loop1", "spec-a", 0, LoopSpecStatus::Completed),
            make_spec("loop1", "spec-b", 1, LoopSpecStatus::Running),
        ];
        let result = current_spec_name(&db, &lp, &specs).unwrap();
        assert_eq!(result, Some("spec-b".to_string()));
    }

    #[test]
    fn format_elapsed_zero_seconds() {
        assert_eq!(format_elapsed(chrono::Duration::seconds(0)), "0s");
    }

    #[test]
    fn format_elapsed_exactly_one_minute() {
        assert_eq!(format_elapsed(chrono::Duration::seconds(60)), "1m0s");
    }

    #[test]
    fn format_elapsed_exactly_one_hour() {
        assert_eq!(format_elapsed(chrono::Duration::seconds(3600)), "1h0m");
    }

    #[test]
    fn format_relative_duration_exactly_zero() {
        assert_eq!(
            format_relative_duration(chrono::Duration::seconds(0)),
            "due now"
        );
    }

    #[test]
    fn format_relative_duration_large_future() {
        let result = format_relative_duration(chrono::Duration::days(365));
        assert!(result.contains("in"));
    }

    #[test]
    fn format_autorun_compact_midnight() {
        let at = DateTime::<Utc>::from_timestamp(0, 0).unwrap();
        let result = format_autorun_compact(at);
        assert!(result.contains("00:00Z"));
    }

    #[test]
    fn format_autorun_compact_end_of_day() {
        let at = DateTime::<Utc>::from_timestamp(23 * 3600 + 59 * 60, 0).unwrap();
        let result = format_autorun_compact(at);
        assert!(result.contains("23:59Z"));
    }
}
