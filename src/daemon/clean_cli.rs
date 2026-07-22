//! CLI handler for `canopy clean` (soft cleanup, C1).
//!
//! Gathers facts from the DB and filesystem, hands them to the pure
//! `domain::clean` decision functions to build a [`CleanPlan`], then either
//! prints it (`--dry-run`) or executes it and prints what happened. Soft
//! mode never touches `active`/`resumed` sessions, never deletes projects,
//! and only reports orphaned projects (missing workdir) with a hint that
//! `canopy clean --hard` — a separate, not-yet-implemented spec — removes
//! them.

use std::path::Path;

use anyhow::Result;

use crate::db::Database;
use crate::domain::canopy_config::CanopyConfig;
use crate::domain::clean::{self, CleanPlan, FileCandidate, ProjectCandidate};

pub async fn handle_clean_action(dry_run: bool, older_than: Option<u64>) -> Result<()> {
    let data_dir = crate::ensure_data_dir()?;
    let db = Database::new(&data_dir.join("background_agents.db"))?;
    let config = CanopyConfig::load(&data_dir);
    let retention_days = older_than.unwrap_or(config.clean.retention_days);
    let now_ts = chrono::Utc::now().timestamp();

    let plan = run_clean(&data_dir, &db, dry_run, retention_days, now_ts)?;
    print_summary(&plan, retention_days, dry_run);
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

fn print_summary(plan: &CleanPlan, retention_days: u64, dry_run: bool) {
    let verb = if dry_run { "Would remove" } else { "Removed" };
    println!(
        "\n\x1b[1m── canopy clean (retention: {retention_days}d{}) ──\x1b[0m",
        if dry_run { ", dry run" } else { "" }
    );
    println!(
        " {verb} {} stale interactive session(s)",
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
    println!(" Reclaimed: {}", format_bytes(plan.reclaimed_bytes()));

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
        }
        println!(
            "   Hint: `canopy clean --hard` removes orphaned projects (separate spec, not yet available)."
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
}
