//! CLI handler for `canopy project remap` — repoint a project whose workdir
//! was renamed or moved at its new location, keeping its sessions and
//! history instead of orphaning them (see `domain::clean`'s orphan report,
//! which hints at this command).

use std::path::PathBuf;

use anyhow::Result;
use clap::Subcommand;

use crate::db::project::{resolve_remap_path, RemapOutcome};
use crate::db::Database;
use crate::domain::db_paths::database_path;
use crate::domain::project::RemapKind;

#[derive(Subcommand)]
pub enum ProjectAction {
    /// Point a project at a new workdir after its directory was renamed or
    /// moved, keeping its sessions and history instead of orphaning them.
    Remap {
        /// The project's current `workdir_hash` (see `canopy clean`'s
        /// orphan report or `project_search`).
        project_hash: String,
        /// The project's new location on disk.
        new_path: PathBuf,
        /// Preview which rows would move without changing anything.
        #[arg(long = "dry-run")]
        dry_run: bool,
        /// Remap even if `new_path` doesn't exist on disk yet.
        #[arg(long)]
        force: bool,
    },
}

pub async fn handle_project_action(action: ProjectAction) -> Result<()> {
    match action {
        ProjectAction::Remap {
            project_hash,
            new_path,
            dry_run,
            force,
        } => handle_remap(&project_hash, &new_path, dry_run, force).await,
    }
}

async fn handle_remap(
    project_hash: &str,
    new_path: &std::path::Path,
    dry_run: bool,
    force: bool,
) -> Result<()> {
    let data_dir = crate::ensure_data_dir()?;
    let db = Database::new(&database_path(&data_dir))?;

    let resolved = resolve_remap_path(new_path, force)?;

    let outcome = if dry_run {
        db.remap_preview(project_hash, &resolved)?
    } else {
        db.remap_project(project_hash, &resolved)?
    };

    print_remap_outcome(&outcome, dry_run);
    Ok(())
}

fn print_remap_outcome(outcome: &RemapOutcome, dry_run: bool) {
    let would_be = if dry_run { "would be " } else { "" };
    match outcome.kind {
        RemapKind::Move => {
            println!(
                "MOVE: project {} {would_be}re-pointed at {} (new hash {}); {} row(s) {would_be}re-keyed.",
                outcome.old_hash,
                outcome.new_path,
                outcome.new_hash,
                outcome.counts.total(),
            );
        }
        RemapKind::Merge => {
            println!(
                "MERGE: project {} {would_be}folded into the existing project at {} (hash {}); \
                 {} row(s) {would_be}reassigned, stale project row {} {would_be}removed.",
                outcome.old_hash,
                outcome.new_path,
                outcome.new_hash,
                outcome.counts.total(),
                outcome.old_hash,
            );
        }
    }
    print_counts(&outcome.counts, dry_run);
}

fn print_counts(counts: &crate::domain::project::RemapCounts, dry_run: bool) {
    let verb = if dry_run { "Would move" } else { "Moved" };
    let rows: &[(&str, i64)] = &[
        ("interactive session(s)", counts.interactive_sessions),
        ("terminal session(s)", counts.terminal_sessions),
        ("loop(s)", counts.loops),
        ("standalone spec(s)", counts.loop_specs),
        ("sync message(s)", counts.sync_messages),
        ("sync lock(s)", counts.sync_locks),
        ("prompt history row(s)", counts.last_prompts),
        ("scheduled send(s)", counts.scheduled_sends),
        ("failed scheduled send(s)", counts.failed_scheduled_sends),
        ("background agent(s)", counts.agents),
        ("intelligence node(s)", counts.intelligence_nodes),
    ];
    for (label, count) in rows {
        if *count > 0 {
            println!("   {verb} {count} {label}");
        }
    }
}
