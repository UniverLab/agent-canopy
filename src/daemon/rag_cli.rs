//! CLI handlers for `canopy rag` subcommands.

use anyhow::Result;
use clap::Subcommand;

use crate::application::ports::StateRepository;
use crate::db::Database;

#[derive(Subcommand, Debug)]
pub(crate) enum RagAction {
    /// Start or stop automatic file indexing.
    AutoIndex {
        #[command(subcommand)]
        action: AutoIndexAction,
    },
    /// Show a detailed per-file RAG indexing report.
    Report,
    /// Delete the entire vector store and reset all RAG state.
    Purge {
        /// Skip the interactive confirmation prompt.
        #[arg(long, short)]
        yes: bool,
    },
}

#[derive(Subcommand, Debug)]
pub(crate) enum AutoIndexAction {
    /// Resume automatic indexing (default state).
    Start,
    /// Pause automatic indexing without losing the queue.
    Stop,
}

pub(crate) async fn handle_rag_action(action: RagAction) -> Result<()> {
    let data_dir = crate::ensure_data_dir()?;
    let db = Database::new(&data_dir.join("background_agents.db"))?;

    match action {
        RagAction::AutoIndex {
            action: AutoIndexAction::Start,
        } => {
            db.set_state("rag_paused", "0")?;
            println!("\x1b[32m✓\x1b[0m  RAG auto-indexing \x1b[1menabled\x1b[0m");
        }
        RagAction::AutoIndex {
            action: AutoIndexAction::Stop,
        } => {
            db.set_state("rag_paused", "1")?;
            println!("\x1b[33m⏸\x1b[0m  RAG auto-indexing \x1b[1mpaused\x1b[0m");
            println!("     Run \x1b[1mcanopy rag auto-index start\x1b[0m to resume.");
        }
        RagAction::Report => handle_rag_report(&data_dir, &db).await?,
        RagAction::Purge { yes } => {
            if !yes {
                eprintln!(
                    "\x1b[33m⚠\x1b[0m  This will \x1b[1mdelete the entire RAG vector store\x1b[0m \
                     and reset all indexing state."
                );
                eprint!("Are you sure? [y/N] ");
                use std::io::Write;
                std::io::stderr().flush()?;
                let mut input = String::new();
                std::io::stdin().read_line(&mut input)?;
                if !input.trim().eq_ignore_ascii_case("y") {
                    println!("Aborted.");
                    return Ok(());
                }
            }
            let lancedb_path = dirs::home_dir()
                .ok_or_else(|| anyhow::anyhow!("Cannot determine home dir"))?
                .join(".canopy/rag/vectors.lancedb");
            let existed = lancedb_path.exists();
            crate::rag::ingestion::wipe_lancedb(&db, "manual purge").await?;
            if existed {
                println!("\x1b[32m✓\x1b[0m  RAG vector store purged and state reset.");
            } else {
                println!("\x1b[32m✓\x1b[0m  RAG state reset (no vector store was present).");
            }
            if crate::daemon::process::read_pid(&data_dir)
                .is_some_and(crate::daemon::process::is_process_running)
            {
                eprintln!(
                    "     Note: the daemon is running and may still hold the old store open. \
                     It will recreate the store on next use."
                );
            }
        }
    }
    Ok(())
}

async fn handle_rag_report(data_dir: &std::path::Path, db: &Database) -> Result<()> {
    let config = crate::domain::canopy_config::CanopyConfig::load(data_dir);

    // ── Vector store chunk counts per file ────────────────────────────
    let chunk_counts = if !config.embeddings_model.trim().is_empty() {
        let dims = crate::rag::embedding_client::model_dimensions(config.embeddings_model.trim())
            .unwrap_or(384);
        match crate::rag::vector_store::VectorStore::open_at(
            &crate::rag::vector_store::VectorStore::default_lancedb_path()?,
            dims,
        )
        .await
        {
            Ok(store) => store.count_chunks_per_file().await.unwrap_or_default(),
            Err(_) => std::collections::HashMap::new(),
        }
    } else {
        std::collections::HashMap::new()
    };

    // ── RAG queue ─────────────────────────────────────────────────────
    let queue_items = db.list_rag_queue(10_000).unwrap_or_default();
    let queued_paths: std::collections::HashSet<&str> =
        queue_items.iter().map(|q| q.source_path.as_str()).collect();

    // ── Events ────────────────────────────────────────────────────────
    let all_events = db.list_rag_events(10_000).unwrap_or_default();

    // Group events by file
    let mut events_by_file: std::collections::HashMap<
        String,
        Vec<&crate::db::project::RagFileEvent>,
    > = std::collections::HashMap::new();
    for ev in &all_events {
        events_by_file
            .entry(ev.file_path.clone())
            .or_default()
            .push(ev);
    }

    // Build a unified set of all known files
    let mut all_files: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    all_files.extend(chunk_counts.keys().cloned());
    all_files.extend(events_by_file.keys().cloned());
    all_files.extend(queued_paths.iter().map(|s| s.to_string()));

    if all_files.is_empty() {
        println!("No RAG data found. Make sure the daemon is running and RAG dirs are configured.");
        return Ok(());
    }

    let is_paused = db.get_state("rag_paused")?.as_deref() == Some("1");
    let total_chunks: usize = chunk_counts.values().sum();
    let processing_items = queue_items
        .iter()
        .filter(|q| q.status == "processing")
        .count();
    let model_status = crate::rag::status::compute_rag_status(
        config.embeddings_model.trim(),
        is_paused,
        crate::rag::status::is_model_loaded(db),
        processing_items as i64,
    );

    // A file's *current* oversize status is whatever its latest event says —
    // if it later shrank and got indexed, the newer "indexed" event wins.
    let is_oversize = |file: &str| -> bool {
        events_by_file
            .get(file)
            .and_then(|events| events.first())
            .is_some_and(|e| e.event_type == "skipped_oversize")
    };
    let oversize_count = all_files.iter().filter(|f| is_oversize(f)).count();

    println!("\n\x1b[1m── Canopy RAG Report ──────────────────────────────────────────\x1b[0m");
    println!(
        " Model:  {}",
        if config.embeddings_model.trim().is_empty() {
            "(not configured)"
        } else {
            config.embeddings_model.trim()
        }
    );
    println!(
        " Status: {}",
        match model_status {
            crate::rag::status::RagModelStatus::Paused => "\x1b[33m⏸ paused\x1b[0m".to_string(),
            crate::rag::status::RagModelStatus::Ready =>
                "\x1b[32m● ready (model loaded)\x1b[0m".to_string(),
            crate::rag::status::RagModelStatus::Sleeping =>
                "\x1b[90m○ sleeping (lazy — loads on demand)\x1b[0m".to_string(),
            crate::rag::status::RagModelStatus::Unavailable(reason) =>
                format!("\x1b[31m✗ unavailable\x1b[0m — {reason}"),
        }
    );
    if model_status == crate::rag::status::RagModelStatus::Ready {
        if let Some(since) = crate::rag::status::model_loaded_since(db) {
            println!("         since: {}", format_ts(since));
        }
    }
    println!(
        " Total:  {} indexed file(s), {} chunk(s)",
        chunk_counts.len(),
        total_chunks
    );
    if !queue_items.is_empty() {
        let queued = queue_items.iter().filter(|q| q.status == "queued").count();
        println!(" Queue:  {} queued, {} indexing", queued, processing_items);
    }
    if oversize_count > 0 {
        let cap_mb = crate::rag::ingestion::FILE_MAX_BYTES as f64 / (1024.0 * 1024.0);
        println!(
            " \x1b[33m⚠\x1b[0m Oversize: {oversize_count} file(s) skipped — exceed the {cap_mb:.0} MB indexing limit (FILE_MAX_BYTES)"
        );
    }

    println!("\n\x1b[1m── Files ──────────────────────────────────────────────────────\x1b[0m");

    for file in &all_files {
        let chunks = chunk_counts.get(file).copied().unwrap_or(0);
        let in_queue = queued_paths.contains(file.as_str());
        let events = events_by_file
            .get(file)
            .map(|v| v.as_slice())
            .unwrap_or(&[]);

        let indexed_count = events.iter().filter(|e| e.event_type == "indexed").count();
        let deleted_count = events.iter().filter(|e| e.event_type == "deleted").count();
        let last_error = events.iter().find(|e| e.event_type == "error");
        let file_is_oversize = is_oversize(file);

        // Status icon
        let status = if file_is_oversize {
            "\x1b[35m⊘\x1b[0m"
        } else if last_error.is_some() && chunks == 0 {
            "\x1b[31m✗\x1b[0m"
        } else if in_queue {
            "\x1b[33m⏳\x1b[0m"
        } else if chunks > 0 {
            "\x1b[32m✓\x1b[0m"
        } else {
            "\x1b[90m○\x1b[0m"
        };

        let short = short_path(file);
        println!("\n  {} {}", status, short);
        if file_is_oversize {
            if let Some(size_bytes) = events.first().and_then(|e| e.detail.as_deref()) {
                if let Ok(bytes) = size_bytes.parse::<u64>() {
                    let cap_mb = crate::rag::ingestion::FILE_MAX_BYTES as f64 / (1024.0 * 1024.0);
                    println!(
                        "     \x1b[35moversize\x1b[0m: {:.1} MB (exceeds {:.0} MB limit — skipped)",
                        bytes as f64 / (1024.0 * 1024.0),
                        cap_mb
                    );
                }
            }
        }
        if chunks > 0 {
            println!("     chunks: {}", chunks);
        }
        if indexed_count > 0 {
            println!("     indexed: {} time(s)", indexed_count);
        }
        if deleted_count > 0 {
            println!("     purged:  {} time(s)", deleted_count);
        }
        if in_queue {
            let item = queue_items.iter().find(|q| q.source_path == *file);
            if let Some(q) = item {
                println!(
                    "     queue:   {} (since {})",
                    q.status,
                    format_ts(q.queued_at)
                );
            }
        }
        if let Some(err) = last_error {
            let detail = err.detail.as_deref().unwrap_or("(no detail)");
            println!(
                "     \x1b[31merror\x1b[0m:   {} — {}",
                format_ts(err.occurred_at),
                crate::tui::truncate_str_keep_tail(detail, 120)
            );
        }
    }

    println!();
    Ok(())
}

fn short_path(path: &str) -> String {
    if let Some(home) = dirs::home_dir() {
        let home_str = home.to_string_lossy();
        if let Some(rest) = path.strip_prefix(home_str.as_ref()) {
            return format!("~{rest}");
        }
    }
    path.to_string()
}

fn format_ts(ts: i64) -> String {
    use std::time::{Duration, UNIX_EPOCH};
    let d = UNIX_EPOCH + Duration::from_secs(ts as u64);
    let dt: chrono::DateTime<chrono::Local> = d.into();
    dt.format("%Y-%m-%d %H:%M").to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::{CommandFactory, Parser};

    #[derive(Parser)]
    struct TestCli {
        #[command(subcommand)]
        action: RagAction,
    }

    #[test]
    fn purge_yes_flag_parsed() {
        let cli =
            TestCli::try_parse_from(["test", "purge", "--yes"]).expect("purge --yes should parse");
        match cli.action {
            RagAction::Purge { yes } => assert!(yes),
            other => panic!("expected Purge, got {other:?}"),
        }
    }

    #[test]
    fn purge_without_yes_defaults_false() {
        let cli =
            TestCli::try_parse_from(["test", "purge"]).expect("purge should parse without --yes");
        match cli.action {
            RagAction::Purge { yes } => assert!(!yes),
            other => panic!("expected Purge, got {other:?}"),
        }
    }

    #[test]
    fn purge_appears_in_help_with_description() {
        let mut cmd = <TestCli as CommandFactory>::command();
        let help = cmd.render_help().to_string();
        assert!(help.contains("purge"), "help should mention purge:\n{help}");
        assert!(
            help.contains("Delete the entire vector store"),
            "purge should have description:\n{help}"
        );
    }

    /// Integration-style check of the exact mapping `handle_rag_report` performs:
    /// live daemon state read from the DB (the CLI's only transport to the
    /// daemon today) must flow through `compute_rag_model_status` to the
    /// truthful three-valued status, including the paused-wins and
    /// active-processing-implies-ready rules.
    #[test]
    fn rag_report_status_mapping_reads_live_daemon_state() {
        let dir = tempfile::tempdir().unwrap();
        let db = Database::new(&dir.path().join("test.db")).unwrap();

        // Fresh daemon, nothing loaded yet: sleeping.
        assert_eq!(
            crate::rag::status::compute_rag_model_status(
                false,
                crate::rag::status::is_model_loaded(&db),
                0,
            ),
            crate::rag::status::RagModelStatus::Sleeping
        );

        // Daemon loads the model (as `IngestionManager::get_or_load_client` persists).
        db.set_state(crate::rag::status::RAG_MODEL_LOADED_KEY, "1")
            .unwrap();
        assert_eq!(
            crate::rag::status::compute_rag_model_status(
                false,
                crate::rag::status::is_model_loaded(&db),
                0,
            ),
            crate::rag::status::RagModelStatus::Ready
        );

        // Paused wins even while the model is still loaded.
        db.set_state("rag_paused", "1").unwrap();
        let is_paused = db.get_state("rag_paused").unwrap().as_deref() == Some("1");
        assert_eq!(
            crate::rag::status::compute_rag_model_status(
                is_paused,
                crate::rag::status::is_model_loaded(&db),
                0,
            ),
            crate::rag::status::RagModelStatus::Paused
        );

        // Unpaused, model unloaded, but a queue item is actively "processing":
        // must never read as sleeping while chunks are being embedded.
        db.set_state("rag_paused", "0").unwrap();
        db.set_state(crate::rag::status::RAG_MODEL_LOADED_KEY, "0")
            .unwrap();
        let is_paused = db.get_state("rag_paused").unwrap().as_deref() == Some("1");
        assert_eq!(
            crate::rag::status::compute_rag_model_status(
                is_paused,
                crate::rag::status::is_model_loaded(&db),
                1,
            ),
            crate::rag::status::RagModelStatus::Ready
        );
    }

    #[tokio::test]
    async fn wipe_lancedb_resets_db_state() {
        let dir = tempfile::tempdir().unwrap();
        let db = Database::new(&dir.path().join("test.db")).unwrap();
        db.set_state("rag_total_chunks", "42").unwrap();
        db.set_state("rag_indexed_files", "5").unwrap();
        // Point at a scratch LanceDB directory instead of the real
        // home-directory-rooted store: the real path may be owned by an
        // actual running canopy daemon, so touching it here would be both
        // unsafe and racy.
        let scratch_lancedb = dir.path().join("vectors.lancedb");
        let _ = crate::rag::ingestion::wipe_lancedb_at(&scratch_lancedb, &db, "test").await;
        assert_eq!(db.get_state("rag_total_chunks").unwrap(), Some("0".into()));
        assert_eq!(db.get_state("rag_indexed_files").unwrap(), Some("0".into()));
    }

    #[test]
    fn short_path_home_dir() {
        if let Some(home) = dirs::home_dir() {
            let home_str = home.to_string_lossy();
            let full = format!("{home_str}/Documents/file.txt");
            assert_eq!(short_path(&full), "~/Documents/file.txt");
        }
    }

    #[test]
    fn short_path_not_home() {
        assert_eq!(short_path("/tmp/something"), "/tmp/something");
    }

    #[test]
    fn short_path_exact_home() {
        if let Some(home) = dirs::home_dir() {
            let home_str = home.to_string_lossy();
            assert_eq!(short_path(&home_str), "~");
        }
    }

    #[test]
    fn format_ts_epoch_zero() {
        let result = format_ts(0);
        // UTC epoch zero, depends on local timezone
        assert!(!result.is_empty());
        assert!(result.contains(":"));
    }

    #[test]
    fn format_ts_recent() {
        let result = format_ts(1700000000);
        assert!(result.contains("2023"));
    }

    #[test]
    fn format_ts_has_time() {
        let result = format_ts(1700000000);
        assert!(result.contains(":"));
    }
}
