//! CLI handlers for `canopy loop` subcommands.
//!
//! `list`/`info`/`export` are read-only: mirror `canopy rag report` (see
//! `rag_cli.rs`) in spirit — a terminal view onto state that previously
//! required querying the database directly. Every other
//! subcommand (`import`/`run`/`pause`/`continue`/`reset`/`autorun`) changes
//! loop state, so it resolves the target loop (or, for `import`, has
//! nothing yet to resolve) against that same local database (exactly as
//! `list`/`info` already do — see [`resolve_loop`]) but then delegates the
//! actual mutation to the daemon's MCP tool of the same name via
//! `daemon::cli_daemon::call_tool`, never touching the database itself.
//! This is the second surface for the operations `loop_import`/`loop_run`/
//! `loop_pause`/`loop_continue`/`loop_reset`/`loop_schedule_autorun` already
//! expose over MCP — for when an MCP client can't reach them but the daemon
//! and its database still can.

use anyhow::{anyhow, Context, Result};
use chrono::{DateTime, Utc};
use clap::Subcommand;

use crate::daemon::cli_daemon::call_tool;
use crate::db::Database;
use crate::domain::db_paths::database_path;
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
    /// Export a loop's design (name, description, nodes, edges, ensembles)
    /// as a portable JSON document, so it can be shared as a file and
    /// recreated elsewhere with `import`.
    Export {
        /// Full loop id, an unambiguous id prefix, or the exact loop name.
        id_or_name: String,
        /// Write the exported JSON to this path instead of stdout.
        #[arg(long)]
        output: Option<String>,
        /// Include each agent node's/ensemble member's platform/model in the
        /// exported file. Omit for a shareable design that never pins the
        /// recipient to a harness or model they may not have.
        #[arg(long = "with-models")]
        with_models: bool,
    },
    /// Create a new loop from an exported document — never overwrites an
    /// existing loop.
    Import {
        /// Path to an exported loop JSON file.
        path: String,
        /// Absolute workdir for the new loop. Defaults to the current
        /// directory.
        #[arg(long)]
        workdir: Option<String>,
        /// Loop name to use instead of the file's own name.
        #[arg(long)]
        name: Option<String>,
    },
    /// Run a loop in the background, spec by spec.
    Run {
        /// Full loop id, an unambiguous id prefix, or the exact loop name.
        id_or_name: String,
        /// Run this queue's pending specs (in queue order) through the
        /// loop's graph instead of the loop's own bound specs.
        #[arg(long)]
        queue: Option<String>,
        /// Absolute workdir override for this run only.
        #[arg(long)]
        workdir: Option<String>,
    },
    /// Pause a running loop after the current node finishes.
    Pause {
        /// Full loop id, an unambiguous id prefix, or the exact loop name.
        id_or_name: String,
    },
    /// Continue a paused loop by retrying the current node or skipping to
    /// the next spec.
    Continue {
        /// Full loop id, an unambiguous id prefix, or the exact loop name.
        id_or_name: String,
        /// Retry the node that was running when the loop paused.
        #[arg(long = "retry-current-node")]
        retry_current_node: bool,
        /// Skip the current spec and move on to the next one.
        #[arg(long = "skip-next-spec")]
        skip_next_spec: bool,
    },
    /// Reset a completed/failed loop back to pending so `run` can relaunch
    /// it.
    Reset {
        /// Full loop id, an unambiguous id prefix, or the exact loop name.
        id_or_name: String,
        /// Reset exactly these spec IDs, even if already completed. Omit to
        /// reset every non-completed spec, leaving completed ones untouched.
        #[arg(long)]
        specs: Vec<String>,
        /// Skip the confirmation prompt.
        #[arg(long)]
        yes: bool,
    },
    /// Schedule a one-shot future resume for a loop, or cancel a pending one.
    Autorun {
        /// Full loop id, an unambiguous id prefix, or the exact loop name.
        id_or_name: String,
        /// ISO 8601 timestamp at which the loop should resume, e.g.
        /// "2026-07-10T09:00:00Z".
        #[arg(long)]
        at: Option<String>,
        /// Raw CLI quota-limit message (e.g. "resets 1pm (America/Bogota)")
        /// to compute the resume instant from, instead of passing `--at`.
        #[arg(long = "quota-reset-message")]
        quota_reset_message: Option<String>,
        /// Cancel any pending autorun instead of scheduling one.
        #[arg(long)]
        cancel: bool,
    },
}

pub(crate) async fn handle_loop_action(
    action: LoopAction,
    port_override: Option<u16>,
) -> Result<()> {
    let data_dir = crate::ensure_data_dir()?;
    let db = Database::new(&database_path(&data_dir))?;

    match action {
        LoopAction::List { workdir } => handle_loop_list(&db, workdir.as_deref()),
        LoopAction::Info { id_or_name } => handle_loop_info(&db, &id_or_name),
        LoopAction::Export {
            id_or_name,
            output,
            with_models,
        } => handle_loop_export(&db, &id_or_name, output.as_deref(), with_models),
        LoopAction::Import {
            path,
            workdir,
            name,
        } => handle_loop_import(port_override, &path, workdir, name),
        LoopAction::Run {
            id_or_name,
            queue,
            workdir,
        } => handle_loop_run(&db, port_override, &id_or_name, queue, workdir),
        LoopAction::Pause { id_or_name } => handle_loop_pause(&db, port_override, &id_or_name),
        LoopAction::Continue {
            id_or_name,
            retry_current_node,
            skip_next_spec,
        } => handle_loop_continue(
            &db,
            port_override,
            &id_or_name,
            retry_current_node,
            skip_next_spec,
        ),
        LoopAction::Reset {
            id_or_name,
            specs,
            yes,
        } => handle_loop_reset(&db, port_override, &id_or_name, &specs, yes),
        LoopAction::Autorun {
            id_or_name,
            at,
            quota_reset_message,
            cancel,
        } => handle_loop_autorun(
            &db,
            port_override,
            &id_or_name,
            at.as_deref(),
            quota_reset_message.as_deref(),
            cancel,
        ),
    }
}

fn handle_loop_run(
    db: &Database,
    port_override: Option<u16>,
    id_or_name: &str,
    queue: Option<String>,
    workdir: Option<String>,
) -> Result<()> {
    let loops = db.list_loops(None, true)?;
    let lp = resolve_loop(&loops, id_or_name)?;

    let mut args = serde_json::json!({ "loop_id": lp.id });
    if let Some(queue_id) = queue {
        args["queue_id"] = serde_json::json!(queue_id);
    }
    if let Some(workdir) = workdir {
        args["workdir"] = serde_json::json!(workdir);
    }

    println!("{}", call_tool(port_override, "loop_run", &args)?);
    Ok(())
}

fn handle_loop_pause(db: &Database, port_override: Option<u16>, id_or_name: &str) -> Result<()> {
    let loops = db.list_loops(None, true)?;
    let lp = resolve_loop(&loops, id_or_name)?;

    println!(
        "{}",
        call_tool(
            port_override,
            "loop_pause",
            &serde_json::json!({ "loop_id": lp.id }),
        )?
    );
    Ok(())
}

fn handle_loop_continue(
    db: &Database,
    port_override: Option<u16>,
    id_or_name: &str,
    retry_current_node: bool,
    skip_next_spec: bool,
) -> Result<()> {
    let action = match (retry_current_node, skip_next_spec) {
        (true, false) => "retry_current_node",
        (false, true) => "skip_next_spec",
        (false, false) => {
            return Err(anyhow!(
                "Specify one of --retry-current-node or --skip-next-spec."
            ))
        }
        (true, true) => {
            return Err(anyhow!(
                "--retry-current-node and --skip-next-spec are mutually exclusive."
            ))
        }
    };

    let loops = db.list_loops(None, true)?;
    let lp = resolve_loop(&loops, id_or_name)?;

    println!(
        "{}",
        call_tool(
            port_override,
            "loop_continue",
            &serde_json::json!({ "loop_id": lp.id, "action": action }),
        )?
    );
    Ok(())
}

fn handle_loop_reset(
    db: &Database,
    port_override: Option<u16>,
    id_or_name: &str,
    specs: &[String],
    yes: bool,
) -> Result<()> {
    let loops = db.list_loops(None, true)?;
    let lp = resolve_loop(&loops, id_or_name)?;

    if !yes && !confirm_reset(&lp.name)? {
        println!("Aborted.");
        return Ok(());
    }

    let mut args = serde_json::json!({ "loop_id": lp.id });
    if !specs.is_empty() {
        args["specs"] = serde_json::json!(specs);
    }

    println!("{}", call_tool(port_override, "loop_reset", &args)?);
    Ok(())
}

/// Interactive confirmation for `loop reset`, bypassable with `--yes` — the
/// same `inquire::Confirm`-with-default-false pattern `canopy clean --hard`
/// already uses for its own destructive cascade, and the CLI counterpart to
/// the TUI's y/n modal for a loop deletion.
fn confirm_reset(loop_name: &str) -> Result<bool> {
    use inquire::Confirm;
    Confirm::new(&format!(
        "Reset loop '{loop_name}' back to pending? This clears progress on its non-completed specs."
    ))
    .with_default(false)
    .with_help_message("y: reset, n/Esc: abort")
    .prompt()
    .map_err(|err| anyhow!("{err}"))
}

/// Read-only, like `list`/`info` — reads the local database directly rather
/// than round-tripping through the daemon, so exporting a loop never
/// requires the daemon to be running.
fn handle_loop_export(
    db: &Database,
    id_or_name: &str,
    output: Option<&str>,
    with_models: bool,
) -> Result<()> {
    let loops = db.list_loops(None, true)?;
    let lp = resolve_loop(&loops, id_or_name)?;

    let graph_nodes = db.list_loop_nodes_for_loop(&lp.id)?;
    let graph_edges = db.list_loop_edges_for_loop(&lp.id)?;
    let ensembles = db.list_ensembles_for_loop(&lp.id)?;
    let document = crate::domain::loop_transfer::build_export_document(
        lp,
        &graph_nodes,
        &graph_edges,
        &ensembles,
        with_models,
    )
    .map_err(|e| anyhow!(e))?;
    let json = serde_json::to_string_pretty(&document)?;

    match output {
        Some(path) => {
            std::fs::write(path, format!("{json}\n"))
                .with_context(|| format!("could not write '{path}'"))?;
            println!("Exported loop '{}' to {path}.", lp.name);
        }
        None => println!("{json}"),
    }
    Ok(())
}

/// Import creates a new loop, which needs the daemon's own side effects
/// (project path registration, trigger activation) — so unlike `export`
/// this delegates to the daemon's `loop_import` MCP tool rather than
/// writing to the database directly (see module doc: every mutation goes
/// through the daemon).
fn handle_loop_import(
    port_override: Option<u16>,
    path: &str,
    workdir: Option<String>,
    name: Option<String>,
) -> Result<()> {
    let raw = std::fs::read_to_string(path).with_context(|| format!("could not read '{path}'"))?;
    // Validated locally first so a malformed file (bad JSON, missing
    // format_version) fails fast with a clear message instead of a round
    // trip to the daemon — `loop_import` re-validates it anyway (decision
    // 5), so this is a UX nicety, not the source of truth.
    let document = crate::domain::loop_transfer::parse_export_document_str(&raw)
        .map_err(|e| anyhow!("'{path}': {e}"))?;
    let document = serde_json::to_value(&document)?;

    let workdir = match workdir {
        Some(workdir) => workdir,
        None => std::env::current_dir()
            .context("could not determine the current directory")?
            .to_string_lossy()
            .to_string(),
    };

    let mut args = serde_json::json!({ "document": document, "workdir": workdir });
    if let Some(name) = name {
        args["name"] = serde_json::json!(name);
    }

    println!("{}", call_tool(port_override, "loop_import", &args)?);
    Ok(())
}

fn handle_loop_autorun(
    db: &Database,
    port_override: Option<u16>,
    id_or_name: &str,
    at: Option<&str>,
    quota_reset_message: Option<&str>,
    cancel: bool,
) -> Result<()> {
    let set_count = [at.is_some(), quota_reset_message.is_some(), cancel]
        .iter()
        .filter(|set| **set)
        .count();
    if set_count == 0 {
        return Err(anyhow!(
            "Specify one of --at, --quota-reset-message, or --cancel."
        ));
    }
    if set_count > 1 {
        return Err(anyhow!(
            "--at, --quota-reset-message, and --cancel are mutually exclusive."
        ));
    }

    let loops = db.list_loops(None, true)?;
    let lp = resolve_loop(&loops, id_or_name)?;

    let args = if cancel {
        serde_json::json!({ "loop_id": lp.id })
    } else if let Some(at) = at {
        serde_json::json!({ "loop_id": lp.id, "at": at })
    } else {
        serde_json::json!({ "loop_id": lp.id, "quota_reset_message": quota_reset_message })
    };

    println!(
        "{}",
        call_tool(port_override, "loop_schedule_autorun", &args)?
    );
    Ok(())
}

fn handle_loop_list(db: &Database, workdir: Option<&str>) -> Result<()> {
    let loops = db.list_loops(workdir, false)?;

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
    // Resolves by id/name (not a browsing list), so an archived loop must
    // still be found here.
    let loops = db.list_loops(None, true)?;
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
        // No specs are bound to this loop directly — it's draining a queue
        // (queue members never set `loop_specs.loop_id`, see `LoopEngine::
        // run_loop`), so there's no fixed queue to show. Reconstruct what
        // ran so far from `loop_runs`, which always records the real
        // `loop_id` regardless of queue membership.
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
        let group_note = match (lp.active_run_queue_id.as_deref(), run.session_id.as_deref()) {
            (Some(queue_id), Some(session_id)) => db
                .group_resume_source(queue_id, &run.spec_id, &run.node_id, session_id)?
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
/// icon `run_status_icon` already gives it, so a `fail` here never reads as
/// the node's own doing. The engine never touches git on this path, so
/// there's nothing to point the operator at recovering — the spec's own
/// `interrupted` status (visible via `loop_info`'s spec listing) is what
/// says the worktree still holds the interrupted attempt's partial work.
fn interrupted_note(output: Option<&serde_json::Value>) -> String {
    if !is_interrupted(output) {
        return String::new();
    }
    "  \x1b[33m(interrupted — not a node failure)\x1b[0m".to_string()
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

/// `done/total` progress for a loop's `loop list` row. A queue-driven run binds
/// no specs of its own (`loop_specs.loop_id` stays null for queue members), so
/// counting `bound_specs` renders a misleading `0/0`. When the loop's row
/// carries an `active_run_queue_id`, count that queue's members instead —
/// mirroring `LoopEngine::spec_progress`, which the running engine uses for the
/// same loop. Falls back to the bound specs for ordinary (non-queue) loops.
fn loop_progress(db: &Database, lp: &Loop, bound_specs: &[LoopSpec]) -> Result<(usize, usize)> {
    let Some(queue_id) = lp.active_run_queue_id.as_deref() else {
        return Ok(spec_progress(bound_specs));
    };
    let ids = db.list_queue_member_spec_ids(queue_id)?;
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
/// or else the next `pending`/`interrupted` one (equally runnable — see
/// `queue_next_pending_spec_id`) in position order. Mirrors
/// `build_loop_summary_json` in `daemon/handler.rs` so the CLI and MCP report
/// the same "current spec" for a given loop.
fn current_spec(specs: &[LoopSpec]) -> Option<&LoopSpec> {
    specs
        .iter()
        .find(|s| s.status == LoopSpecStatus::Running)
        .or_else(|| {
            specs.iter().find(|s| {
                matches!(
                    s.status,
                    LoopSpecStatus::Pending | LoopSpecStatus::Interrupted
                )
            })
        })
}

/// Name of the spec a loop is actively working through, for both bound-spec
/// loops (via [`current_spec`]) and queue-driven loops, which never bind a
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
/// duplicates dropped — used to approximate a queue-driven loop's queue from
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
        LoopSpecStatus::Interrupted => "\x1b[33m⚑\x1b[0m",
    }
}

/// B36: a run cut short by a daemon restart/kill is recorded as `fail` (see
/// `reconcile_orphaned_loops` / `reconcile_stranded_queue_specs`), but it must
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

    #[test]
    fn run_parses_queue_and_workdir() {
        let cli = TestCli::try_parse_from(["test", "run", "my-loop"]).expect("should parse");
        match cli.action {
            LoopAction::Run {
                id_or_name,
                queue,
                workdir,
            } => {
                assert_eq!(id_or_name, "my-loop");
                assert!(queue.is_none());
                assert!(workdir.is_none());
            }
            other => panic!("expected Run, got {other:?}"),
        }

        let cli = TestCli::try_parse_from([
            "test",
            "run",
            "my-loop",
            "--queue",
            "q1",
            "--workdir",
            "/tmp/proj",
        ])
        .expect("should parse");
        match cli.action {
            LoopAction::Run {
                id_or_name,
                queue,
                workdir,
            } => {
                assert_eq!(id_or_name, "my-loop");
                assert_eq!(queue.as_deref(), Some("q1"));
                assert_eq!(workdir.as_deref(), Some("/tmp/proj"));
            }
            other => panic!("expected Run, got {other:?}"),
        }
    }

    #[test]
    fn pause_requires_id_or_name() {
        assert!(TestCli::try_parse_from(["test", "pause"]).is_err());
        let cli = TestCli::try_parse_from(["test", "pause", "my-loop"]).expect("should parse");
        match cli.action {
            LoopAction::Pause { id_or_name } => assert_eq!(id_or_name, "my-loop"),
            other => panic!("expected Pause, got {other:?}"),
        }
    }

    #[test]
    fn export_parses_output_and_with_models() {
        let cli = TestCli::try_parse_from(["test", "export", "my-loop"]).expect("should parse");
        match cli.action {
            LoopAction::Export {
                id_or_name,
                output,
                with_models,
            } => {
                assert_eq!(id_or_name, "my-loop");
                assert!(output.is_none());
                assert!(!with_models);
            }
            other => panic!("expected Export, got {other:?}"),
        }

        let cli = TestCli::try_parse_from([
            "test",
            "export",
            "my-loop",
            "--output",
            "loop.json",
            "--with-models",
        ])
        .expect("should parse");
        match cli.action {
            LoopAction::Export {
                id_or_name,
                output,
                with_models,
            } => {
                assert_eq!(id_or_name, "my-loop");
                assert_eq!(output.as_deref(), Some("loop.json"));
                assert!(with_models);
            }
            other => panic!("expected Export, got {other:?}"),
        }
    }

    #[test]
    fn import_parses_workdir_and_name() {
        let cli = TestCli::try_parse_from(["test", "import", "loop.json"]).expect("should parse");
        match cli.action {
            LoopAction::Import {
                path,
                workdir,
                name,
            } => {
                assert_eq!(path, "loop.json");
                assert!(workdir.is_none());
                assert!(name.is_none());
            }
            other => panic!("expected Import, got {other:?}"),
        }

        let cli = TestCli::try_parse_from([
            "test",
            "import",
            "loop.json",
            "--workdir",
            "/tmp/proj",
            "--name",
            "New Name",
        ])
        .expect("should parse");
        match cli.action {
            LoopAction::Import {
                path,
                workdir,
                name,
            } => {
                assert_eq!(path, "loop.json");
                assert_eq!(workdir.as_deref(), Some("/tmp/proj"));
                assert_eq!(name.as_deref(), Some("New Name"));
            }
            other => panic!("expected Import, got {other:?}"),
        }
    }

    #[test]
    fn continue_parses_retry_and_skip_flags() {
        let cli = TestCli::try_parse_from(["test", "continue", "my-loop", "--retry-current-node"])
            .expect("should parse");
        match cli.action {
            LoopAction::Continue {
                id_or_name,
                retry_current_node,
                skip_next_spec,
            } => {
                assert_eq!(id_or_name, "my-loop");
                assert!(retry_current_node);
                assert!(!skip_next_spec);
            }
            other => panic!("expected Continue, got {other:?}"),
        }

        let cli = TestCli::try_parse_from(["test", "continue", "my-loop", "--skip-next-spec"])
            .expect("should parse");
        match cli.action {
            LoopAction::Continue { skip_next_spec, .. } => assert!(skip_next_spec),
            other => panic!("expected Continue, got {other:?}"),
        }
    }

    #[test]
    fn reset_parses_repeated_specs_and_yes() {
        let cli = TestCli::try_parse_from([
            "test", "reset", "my-loop", "--specs", "a", "--specs", "b", "--yes",
        ])
        .expect("should parse");
        match cli.action {
            LoopAction::Reset {
                id_or_name,
                specs,
                yes,
            } => {
                assert_eq!(id_or_name, "my-loop");
                assert_eq!(specs, vec!["a".to_string(), "b".to_string()]);
                assert!(yes);
            }
            other => panic!("expected Reset, got {other:?}"),
        }
    }

    #[test]
    fn autorun_parses_at_quota_message_and_cancel() {
        let cli =
            TestCli::try_parse_from(["test", "autorun", "my-loop", "--at", "2026-07-10T09:00:00Z"])
                .expect("should parse");
        match cli.action {
            LoopAction::Autorun { at, .. } => {
                assert_eq!(at.as_deref(), Some("2026-07-10T09:00:00Z"));
            }
            other => panic!("expected Autorun, got {other:?}"),
        }

        let cli = TestCli::try_parse_from(["test", "autorun", "my-loop", "--cancel"])
            .expect("should parse");
        match cli.action {
            LoopAction::Autorun { cancel, .. } => assert!(cancel),
            other => panic!("expected Autorun, got {other:?}"),
        }
    }

    fn make_loop(id: &str, name: &str, status: LoopStatus) -> Loop {
        Loop {
            archived: false,
            paused_by_reconciliation: false,
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
            active_run_queue_id: None,
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
    fn loop_progress_counts_queue_members_when_active_run_queue_id_set() {
        use crate::domain::queues::Queue;

        let dir = tempfile::tempdir().unwrap();
        let db = Database::new(&dir.path().join("t.db")).unwrap();

        // A queue-driven loop binds no specs of its own, so counting bound
        // specs would render a misleading 0/0.
        let mut lp = make_loop("loop-1", "queue-loop", LoopStatus::Running);
        lp.active_run_queue_id = Some("queue-1".to_string());
        db.insert_loop(&lp).unwrap();
        db.insert_queue(&Queue {
            id: "queue-1".to_string(),
            name: "P".to_string(),
            created_at: Utc::now(),
        })
        .unwrap();

        // Two standalone queue members (loop_id: None, like real ones): one
        // completed, one pending.
        for (id, status) in [
            ("spec-a", LoopSpecStatus::Completed),
            ("spec-b", LoopSpecStatus::Pending),
        ] {
            let mut spec = make_spec("queue", id, 0, status);
            spec.id = id.to_string();
            spec.loop_id = None;
            db.insert_loop_spec(&spec).unwrap();
            db.append_queue_member("queue-1", id, None).unwrap();
        }

        // Bound specs empty; queue progress is 1/2.
        assert_eq!(loop_progress(&db, &lp, &[]).unwrap(), (1, 2));
    }

    #[test]
    fn loop_progress_shows_n_of_n_for_completed_queue_loop() {
        use crate::domain::queues::Queue;

        let dir = tempfile::tempdir().unwrap();
        let db = Database::new(&dir.path().join("t.db")).unwrap();

        // B31: a finished queue-driven loop keeps its `active_run_queue_id`, so
        // even in a terminal status it must still render its real n/n queue
        // progress rather than the 0/0 a bound-spec count would produce.
        let mut lp = make_loop("loop-done", "queue-loop", LoopStatus::Completed);
        lp.active_run_queue_id = Some("queue-1".to_string());
        db.insert_loop(&lp).unwrap();
        db.insert_queue(&Queue {
            id: "queue-1".to_string(),
            name: "P".to_string(),
            created_at: Utc::now(),
        })
        .unwrap();

        for id in ["spec-a", "spec-b"] {
            let mut spec = make_spec("queue", id, 0, LoopSpecStatus::Completed);
            spec.id = id.to_string();
            spec.loop_id = None;
            db.insert_loop_spec(&spec).unwrap();
            db.append_queue_member("queue-1", id, None).unwrap();
        }

        assert_eq!(loop_progress(&db, &lp, &[]).unwrap(), (2, 2));
    }

    #[test]
    fn loop_progress_falls_back_to_bound_specs_without_queue() {
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
            LoopSpecStatus::Interrupted,
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
    fn interrupted_note_renders_for_interrupted_run() {
        let output = Some(serde_json::json!({"interrupted": true}));
        let note = interrupted_note(output.as_ref());
        assert!(note.contains("interrupted"));
        assert!(!note.contains("git stash"));
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

    // ── state-changing handlers: daemon delegation ──────────────────

    use crate::tui::mcp_client::test_support::{spawn_fake_daemon, unused_port};

    fn db_with_loop(id: &str, name: &str, status: LoopStatus) -> (tempfile::TempDir, Database) {
        let dir = tempfile::tempdir().unwrap();
        let db = Database::new(&dir.path().join("t.db")).unwrap();
        db.insert_loop(&make_loop(id, name, status)).unwrap();
        (dir, db)
    }

    #[test]
    fn handle_loop_run_calls_loop_run_tool_with_resolved_id() {
        let (_dir, db) = db_with_loop("loop-1", "my-loop", LoopStatus::Draft);
        let fake = spawn_fake_daemon(serde_json::json!({
            "content": [{"type": "text", "text": "Loop 'loop-1' launched in background."}],
            "isError": false
        }));
        let port: u16 = fake.port.parse().unwrap();

        handle_loop_run(&db, Some(port), "my-loop", Some("q1".to_string()), None)
            .expect("run should succeed");

        let calls = fake.recorded_calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0]["name"], "loop_run");
        assert_eq!(calls[0]["arguments"]["loop_id"], "loop-1");
        assert_eq!(calls[0]["arguments"]["queue_id"], "q1");
    }

    #[test]
    fn handle_loop_run_rejects_unknown_loop_before_touching_daemon() {
        let (_dir, db) = db_with_loop("loop-1", "my-loop", LoopStatus::Draft);
        // No fake daemon is spawned — a request that reaches the network at
        // all would fail differently (connection refused) than the
        // not-found error resolution must produce locally.
        let err = handle_loop_run(&db, Some(65535), "missing", None, None).unwrap_err();
        assert!(err.to_string().contains("No loop matches"));
    }

    #[test]
    fn handle_loop_pause_calls_loop_pause_tool() {
        let (_dir, db) = db_with_loop("loop-1", "my-loop", LoopStatus::Running);
        let fake = spawn_fake_daemon(serde_json::json!({
            "content": [{"type": "text", "text": "Loop 'loop-1' marked to pause."}],
            "isError": false
        }));
        let port: u16 = fake.port.parse().unwrap();

        handle_loop_pause(&db, Some(port), "loop-1").expect("pause should succeed");

        let calls = fake.recorded_calls();
        assert_eq!(calls[0]["name"], "loop_pause");
        assert_eq!(calls[0]["arguments"]["loop_id"], "loop-1");
    }

    #[test]
    fn handle_loop_continue_sends_retry_current_node_action() {
        let (_dir, db) = db_with_loop("loop-1", "my-loop", LoopStatus::Paused);
        let fake = spawn_fake_daemon(serde_json::json!({
            "content": [{"type": "text", "text": "resumed"}],
            "isError": false
        }));
        let port: u16 = fake.port.parse().unwrap();

        handle_loop_continue(&db, Some(port), "loop-1", true, false).expect("should succeed");

        let calls = fake.recorded_calls();
        assert_eq!(calls[0]["arguments"]["action"], "retry_current_node");
    }

    #[test]
    fn handle_loop_continue_sends_skip_next_spec_action() {
        let (_dir, db) = db_with_loop("loop-1", "my-loop", LoopStatus::Paused);
        let fake = spawn_fake_daemon(serde_json::json!({
            "content": [{"type": "text", "text": "resumed"}],
            "isError": false
        }));
        let port: u16 = fake.port.parse().unwrap();

        handle_loop_continue(&db, Some(port), "loop-1", false, true).expect("should succeed");

        let calls = fake.recorded_calls();
        assert_eq!(calls[0]["arguments"]["action"], "skip_next_spec");
    }

    #[test]
    fn handle_loop_continue_rejects_when_neither_flag_set() {
        let (_dir, db) = db_with_loop("loop-1", "my-loop", LoopStatus::Paused);
        let err = handle_loop_continue(&db, Some(65535), "loop-1", false, false).unwrap_err();
        assert!(err.to_string().contains("Specify one of"));
    }

    #[test]
    fn handle_loop_continue_rejects_when_both_flags_set() {
        let (_dir, db) = db_with_loop("loop-1", "my-loop", LoopStatus::Paused);
        let err = handle_loop_continue(&db, Some(65535), "loop-1", true, true).unwrap_err();
        assert!(err.to_string().contains("mutually exclusive"));
    }

    #[test]
    fn handle_loop_reset_with_yes_sends_explicit_specs() {
        let (_dir, db) = db_with_loop("loop-1", "my-loop", LoopStatus::Failed);
        // `yes: true` bypasses the interactive confirmation prompt (which
        // would otherwise block on a non-tty test run), exercising the
        // confirmed path deterministically.
        let fake = spawn_fake_daemon(serde_json::json!({
            "content": [{"type": "text", "text": "Loop 'loop-1' reset."}],
            "isError": false
        }));
        let port: u16 = fake.port.parse().unwrap();

        handle_loop_reset(
            &db,
            Some(port),
            "loop-1",
            &["spec-a".to_string(), "spec-b".to_string()],
            true,
        )
        .expect("reset should succeed");

        let calls = fake.recorded_calls();
        assert_eq!(calls[0]["name"], "loop_reset");
        assert_eq!(calls[0]["arguments"]["loop_id"], "loop-1");
        assert_eq!(
            calls[0]["arguments"]["specs"],
            serde_json::json!(["spec-a", "spec-b"])
        );
    }

    #[test]
    fn handle_loop_reset_omits_specs_field_when_none_given() {
        let (_dir, db) = db_with_loop("loop-1", "my-loop", LoopStatus::Failed);
        let fake = spawn_fake_daemon(serde_json::json!({
            "content": [{"type": "text", "text": "Loop 'loop-1' reset."}],
            "isError": false
        }));
        let port: u16 = fake.port.parse().unwrap();

        handle_loop_reset(&db, Some(port), "loop-1", &[], true).expect("reset should succeed");

        let calls = fake.recorded_calls();
        assert!(calls[0]["arguments"].get("specs").is_none());
    }

    /// Export is read-only and must work without any daemon listening —
    /// mirroring `list`/`info`, it reads the local database directly.
    #[test]
    fn handle_loop_export_writes_document_to_output_path_without_a_daemon() {
        let (dir, db) = db_with_loop("loop-1", "my-loop", LoopStatus::Draft);
        db.insert_loop_node(&crate::domain::loops::LoopNode {
            id: "n1".to_string(),
            spec_id: None,
            loop_id: Some("loop-1".to_string()),
            name: "implementer".to_string(),
            kind: crate::domain::loops::LoopNodeKind::Agent,
            config: serde_json::json!({"platform": "claude", "prompt_template": "go"}),
            position: 1,
            created_at: Utc::now(),
        })
        .unwrap();

        let out_path = dir.path().join("export.json");
        handle_loop_export(&db, "loop-1", Some(out_path.to_str().unwrap()), false)
            .expect("export should succeed");

        let raw = std::fs::read_to_string(&out_path).unwrap();
        let doc: serde_json::Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(doc["format_version"], 1);
        assert_eq!(doc["name"], "my-loop");
        assert!(doc["nodes"][0]["config"].get("platform").is_none());
    }

    #[test]
    fn handle_loop_export_with_models_keeps_platform() {
        let (dir, db) = db_with_loop("loop-1", "my-loop", LoopStatus::Draft);
        db.insert_loop_node(&crate::domain::loops::LoopNode {
            id: "n1".to_string(),
            spec_id: None,
            loop_id: Some("loop-1".to_string()),
            name: "implementer".to_string(),
            kind: crate::domain::loops::LoopNodeKind::Agent,
            config: serde_json::json!({"platform": "claude", "prompt_template": "go"}),
            position: 1,
            created_at: Utc::now(),
        })
        .unwrap();

        let out_path = dir.path().join("export.json");
        handle_loop_export(&db, "loop-1", Some(out_path.to_str().unwrap()), true)
            .expect("export should succeed");

        let raw = std::fs::read_to_string(&out_path).unwrap();
        let doc: serde_json::Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(doc["nodes"][0]["config"]["platform"], "claude");
    }

    #[test]
    fn handle_loop_export_rejects_unknown_loop_before_touching_disk() {
        let (_dir, db) = db_with_loop("loop-1", "my-loop", LoopStatus::Draft);
        let err = handle_loop_export(&db, "missing", None, false).unwrap_err();
        assert!(err.to_string().contains("No loop matches"));
    }

    /// Import delegates to the daemon's `loop_import` MCP tool — unlike
    /// export, it has state-changing side effects (project registration,
    /// trigger activation) that only the running daemon can perform.
    #[test]
    fn handle_loop_import_sends_document_workdir_and_name_to_daemon() {
        let dir = tempfile::tempdir().unwrap();
        let file_path = dir.path().join("loop.json");
        std::fs::write(
            &file_path,
            serde_json::json!({
                "format_version": 1,
                "name": "Shared Loop",
                "nodes": [],
                "edges": [],
                "ensembles": []
            })
            .to_string(),
        )
        .unwrap();

        let fake = spawn_fake_daemon(serde_json::json!({
            "content": [{"type": "text", "text": "{\"loop_id\": \"new-1\", \"name\": \"Shared Loop\", \"nodes_missing_platform\": []}"}],
            "isError": false
        }));
        let port: u16 = fake.port.parse().unwrap();

        handle_loop_import(
            Some(port),
            file_path.to_str().unwrap(),
            Some("/tmp/target".to_string()),
            Some("Custom Name".to_string()),
        )
        .expect("import should succeed");

        let calls = fake.recorded_calls();
        assert_eq!(calls[0]["name"], "loop_import");
        assert_eq!(calls[0]["arguments"]["workdir"], "/tmp/target");
        assert_eq!(calls[0]["arguments"]["name"], "Custom Name");
        assert_eq!(calls[0]["arguments"]["document"]["name"], "Shared Loop");
    }

    #[test]
    fn handle_loop_import_rejects_missing_format_version_before_touching_daemon() {
        let dir = tempfile::tempdir().unwrap();
        let file_path = dir.path().join("loop.json");
        std::fs::write(
            &file_path,
            serde_json::json!({"name": "x", "nodes": [], "edges": [], "ensembles": []}).to_string(),
        )
        .unwrap();

        // No fake daemon spawned — a request that reaches the network at
        // all would fail differently (connection refused).
        let err =
            handle_loop_import(Some(65535), file_path.to_str().unwrap(), None, None).unwrap_err();
        assert!(err.to_string().contains("format_version"));
    }

    #[test]
    fn handle_loop_import_rejects_unreadable_path() {
        let err =
            handle_loop_import(Some(65535), "/nonexistent/loop.json", None, None).unwrap_err();
        assert!(err.to_string().contains("could not read"));
    }

    #[test]
    fn handle_loop_autorun_sends_at() {
        let (_dir, db) = db_with_loop("loop-1", "my-loop", LoopStatus::Draft);
        let fake = spawn_fake_daemon(serde_json::json!({
            "content": [{"type": "text", "text": "scheduled"}],
            "isError": false
        }));
        let port: u16 = fake.port.parse().unwrap();

        handle_loop_autorun(
            &db,
            Some(port),
            "loop-1",
            Some("2026-07-10T09:00:00Z"),
            None,
            false,
        )
        .expect("autorun should succeed");

        let calls = fake.recorded_calls();
        assert_eq!(calls[0]["arguments"]["at"], "2026-07-10T09:00:00Z");
    }

    #[test]
    fn handle_loop_autorun_cancel_omits_at_and_message() {
        let (_dir, db) = db_with_loop("loop-1", "my-loop", LoopStatus::Draft);
        let fake = spawn_fake_daemon(serde_json::json!({
            "content": [{"type": "text", "text": "cancelled"}],
            "isError": false
        }));
        let port: u16 = fake.port.parse().unwrap();

        handle_loop_autorun(&db, Some(port), "loop-1", None, None, true)
            .expect("autorun cancel should succeed");

        let calls = fake.recorded_calls();
        assert!(calls[0]["arguments"].get("at").is_none());
        assert!(calls[0]["arguments"].get("quota_reset_message").is_none());
    }

    #[test]
    fn handle_loop_autorun_rejects_no_flags() {
        let (_dir, db) = db_with_loop("loop-1", "my-loop", LoopStatus::Draft);
        let err = handle_loop_autorun(&db, Some(65535), "loop-1", None, None, false).unwrap_err();
        assert!(err.to_string().contains("Specify one of"));
    }

    #[test]
    fn handle_loop_autorun_rejects_multiple_flags() {
        let (_dir, db) = db_with_loop("loop-1", "my-loop", LoopStatus::Draft);
        let err = handle_loop_autorun(
            &db,
            Some(65535),
            "loop-1",
            Some("2026-07-10T09:00:00Z"),
            None,
            true,
        )
        .unwrap_err();
        assert!(err.to_string().contains("mutually exclusive"));
    }

    /// The case this spec exists for: the daemon is unreachable on the
    /// resolved port, and the failure must read as "daemon unreachable",
    /// never as "the loop does not exist" (the loop resolves fine locally
    /// against the same database `loop list`/`loop info` already read).
    #[test]
    fn handle_loop_pause_reports_daemon_unreachable_distinctly_from_not_found() {
        let (_dir, db) = db_with_loop("loop-1", "my-loop", LoopStatus::Running);

        for _ in 0..5 {
            let port: u16 = unused_port().parse().unwrap();
            match handle_loop_pause(&db, Some(port), "loop-1") {
                Err(err) => {
                    let chain: Vec<String> = err.chain().map(ToString::to_string).collect();
                    assert!(chain
                        .iter()
                        .any(|m| m.contains("could not reach the daemon")));
                    assert!(!chain.iter().any(|m| m.contains("No loop matches")));
                    return;
                }
                Ok(()) => continue,
            }
        }
        panic!("port kept getting claimed by another test after 5 attempts");
    }
}
