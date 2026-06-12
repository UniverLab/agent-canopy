//! CLI handlers for `canopy rag` subcommands.

use anyhow::Result;
use clap::Subcommand;

use crate::application::ports::StateRepository;
use crate::db::Database;

#[derive(Subcommand)]
pub(crate) enum RagAction {
    /// Start or stop automatic file indexing.
    AutoIndex {
        #[command(subcommand)]
        action: AutoIndexAction,
    },
    /// Show a detailed per-file RAG indexing report.
    Report,
}

#[derive(Subcommand)]
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
        if is_paused {
            "\x1b[33m⏸ paused\x1b[0m"
        } else {
            "\x1b[32m● running\x1b[0m"
        }
    );
    println!(
        " Total:  {} indexed file(s), {} chunk(s)",
        chunk_counts.len(),
        total_chunks
    );
    if !queue_items.is_empty() {
        let processing = queue_items
            .iter()
            .filter(|q| q.status == "processing")
            .count();
        let queued = queue_items.iter().filter(|q| q.status == "queued").count();
        println!(" Queue:  {} queued, {} indexing", queued, processing);
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

        // Status icon
        let status = if last_error.is_some() && chunks == 0 {
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
