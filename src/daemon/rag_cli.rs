//! CLI handlers for `canopy rag` subcommands.

use anyhow::Result;
use clap::Subcommand;

use crate::application::ports::StateRepository;
use crate::db::Database;
use crate::domain::db_paths::database_path;

#[derive(Subcommand, Debug)]
pub(crate) enum RagAction {
    /// Start or stop automatic file indexing.
    AutoIndex {
        #[command(subcommand)]
        action: AutoIndexAction,
    },
    /// Show a detailed per-file RAG indexing report.
    Report,
    /// Index all configured files not yet in the vector store.
    Backfill,
    /// Delete the entire vector store and reset all RAG state.
    Purge {
        /// Skip the interactive confirmation prompt.
        #[arg(long, short)]
        yes: bool,
    },
    /// Manage the configured embedding model's download/prepare state.
    Model {
        #[command(subcommand)]
        action: ModelAction,
    },
}

#[derive(Subcommand, Debug)]
pub(crate) enum AutoIndexAction {
    /// Resume automatic indexing (default state).
    Start,
    /// Pause automatic indexing without losing the queue.
    Stop,
}

#[derive(Subcommand, Debug)]
pub(crate) enum ModelAction {
    /// Clear a failed local-model download/prepare attempt so the running
    /// daemon's background acquisition loop retries it (within its normal
    /// poll interval), without re-running the whole setup wizard.
    Retry,
}

pub(crate) async fn handle_rag_action(action: RagAction) -> Result<()> {
    let data_dir = crate::ensure_data_dir()?;
    let db = Database::new_safe(&database_path(&data_dir), &data_dir)?;

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
        RagAction::Backfill => handle_rag_backfill(&data_dir, &db).await?,
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
        RagAction::Model {
            action: ModelAction::Retry,
        } => handle_model_retry(&data_dir, &db)?,
    }
    Ok(())
}

fn handle_model_retry(data_dir: &std::path::Path, db: &Database) -> Result<()> {
    let config = crate::domain::canopy_config::CanopyConfig::load(data_dir);
    let model_id = config.embeddings_model.trim();

    if model_id.is_empty() {
        println!("\x1b[33m⚠\x1b[0m  No embeddings model configured — nothing to retry.");
        return Ok(());
    }

    match crate::rag::status::read_acquisition_state(db, model_id) {
        Some(crate::rag::status::AcquisitionState::Failed { reason }) => {
            crate::rag::status::clear_acquisition(db, model_id);
            println!("\x1b[32m✓\x1b[0m  Cleared failed download for '{model_id}' (was: {reason}).");
            println!(
                "     The running daemon will retry it automatically within ~15s. \
                 Start the daemon first if it isn't running."
            );
        }
        Some(crate::rag::status::AcquisitionState::Downloading { .. }) => {
            println!(
                "\x1b[33m⚠\x1b[0m  '{model_id}' is already downloading — nothing to retry yet."
            );
        }
        Some(crate::rag::status::AcquisitionState::Preparing { .. }) => {
            println!(
                "\x1b[33m⚠\x1b[0m  '{model_id}' is already being prepared — nothing to retry yet."
            );
        }
        None => {
            println!("\x1b[32m✓\x1b[0m  '{model_id}' has no failed download to retry.");
        }
    }
    Ok(())
}

/// Outcome of a backfill scan: what was found on disk, what was already
/// stored, and what was newly enqueued. Pure data — see [`run_backfill`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct BackfillOutcome {
    /// Indexable (supported, non-ignored, under the size limit) files on disk.
    pub indexable_on_disk: usize,
    /// Supported, non-ignored files over the size limit (skipped, with reason).
    pub oversize_on_disk: usize,
    /// Indexable files already present in the vector store.
    pub already_indexed: usize,
    /// Files newly enqueued for indexing by this run.
    pub enqueued: usize,
}

/// Pure core of `canopy rag backfill`: diff the authoritative filesystem
/// scan against the set of already-indexed paths and enqueue exactly the
/// remainder. Takes the indexed set as a plain value so tests can exercise
/// the remainder/idempotency logic without opening a real vector store.
///
/// `enqueue_rag_item` is an idempotent upsert, so re-running (or
/// interrupting and resuming) never loses finished work and never re-embeds
/// what is already stored — the second run finds nothing left to enqueue.
pub(crate) fn run_backfill(
    db: &Database,
    indexable_files: &[std::path::PathBuf],
    indexed_paths: &std::collections::HashSet<String>,
    now: i64,
) -> BackfillOutcome {
    let to_index: Vec<&std::path::PathBuf> = indexable_files
        .iter()
        .filter(|p| !indexed_paths.contains(&p.to_string_lossy().to_string()))
        .collect();
    let already_indexed = indexable_files.len().saturating_sub(to_index.len());
    let mut enqueued = 0usize;
    for path in &to_index {
        if db.enqueue_rag_item(&path.to_string_lossy(), now).is_ok() {
            enqueued += 1;
        }
    }
    BackfillOutcome {
        indexable_on_disk: indexable_files.len(),
        oversize_on_disk: 0,
        already_indexed,
        enqueued,
    }
}

async fn handle_rag_backfill(data_dir: &std::path::Path, db: &Database) -> Result<()> {
    let config = crate::domain::canopy_config::CanopyConfig::load(data_dir);

    if config.rag_personal_dirs.is_empty() {
        println!("\x1b[33m⚠\x1b[0m  No personal RAG directories configured.");
        return Ok(());
    }

    let max_bytes = config.rag_max_file_bytes();
    let cap_mb = max_bytes as f64 / (1024.0 * 1024.0);

    // 1. Authoritative filesystem scan of the configured roots.
    let scan = crate::rag::size_report::scan(data_dir, &config.rag_personal_dirs, max_bytes)?;
    let oversize_on_disk = scan.oversize_files.len();

    // 2. Already-indexed paths from the vector store (single source of truth).
    let indexed_paths: std::collections::HashSet<String> =
        if !config.embeddings_model.trim().is_empty() {
            let dims =
                crate::rag::embedding_client::model_dimensions(config.embeddings_model.trim())
                    .unwrap_or(384);
            match crate::rag::vector_store::VectorStore::open_at(
                &crate::rag::vector_store::VectorStore::default_lancedb_path()?,
                dims,
                Some(config.rag_vector_cache_entries),
            )
            .await
            {
                Ok(store) => store
                    .list_unique_paths()
                    .await
                    .unwrap_or_default()
                    .into_iter()
                    .collect(),
                Err(_) => std::collections::HashSet::new(),
            }
        } else {
            std::collections::HashSet::new()
        };

    // 3-4. Diff and enqueue (resumable, incremental — see `run_backfill`).
    let now = chrono::Utc::now().timestamp();
    let mut outcome = run_backfill(db, &scan.indexable_files, &indexed_paths, now);
    outcome.oversize_on_disk = oversize_on_disk;

    // 5. Report: found / indexed / skipped-with-reason.
    println!("\n\x1b[1m── RAG Backfill ─────────────────────────────────────────────\x1b[0m");
    println!(
        " Found on disk:     {} indexable file(s), {} oversize (skipped: exceed the {:.0} MB limit)",
        outcome.indexable_on_disk, outcome.oversize_on_disk, cap_mb
    );
    println!(" Already indexed:   {}", outcome.already_indexed);
    println!(" Enqueued to index: {}", outcome.enqueued);
    println!();
    if outcome.enqueued > 0 {
        println!(" The daemon picks these up automatically; nothing else to run.");
    } else {
        println!(" \x1b[32m✓\x1b[0m Everything is already indexed.");
    }

    Ok(())
}

/// Every configured file in exactly one state, derived from a single
/// authoritative query each (the vector store's chunk map, the fresh
/// metadata scan, the DB queue). Priority is indexed > oversize > queued >
/// not-yet-indexed, so no file is double-counted and none is lost.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct RagFileCategories {
    /// Has chunks in the vector store.
    pub indexed: std::collections::BTreeSet<String>,
    /// Over the size limit and not indexed (skipped-for-size).
    pub oversize: std::collections::BTreeSet<String>,
    /// In the DB queue and not indexed/oversize (awaiting indexing).
    pub queued: std::collections::BTreeSet<String>,
    /// Indexable on disk but in none of the above (never indexed).
    pub not_yet_indexed: std::collections::BTreeSet<String>,
}

pub(crate) fn categorize_rag_files(
    chunk_paths: impl IntoIterator<Item = String>,
    oversize_files: &[std::path::PathBuf],
    queued_paths: &[String],
    indexable_files: &[std::path::PathBuf],
) -> RagFileCategories {
    let mut cats = RagFileCategories::default();
    cats.indexed.extend(chunk_paths);
    for p in oversize_files {
        let s = p.to_string_lossy().to_string();
        if !cats.indexed.contains(&s) {
            cats.oversize.insert(s);
        }
    }
    for p in queued_paths {
        if !cats.indexed.contains(p) && !cats.oversize.contains(p) {
            cats.queued.insert(p.clone());
        }
    }
    for p in indexable_files {
        let s = p.to_string_lossy().to_string();
        if !cats.indexed.contains(&s) && !cats.oversize.contains(&s) && !cats.queued.contains(&s) {
            cats.not_yet_indexed.insert(s);
        }
    }
    cats
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
            Some(config.rag_vector_cache_entries),
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

    // ── Size-based exclusions ────────────────────────────────────────
    // Recomputed from filesystem metadata (never file contents), not read
    // from the ledger: a large file that was never ingested has no event,
    // and the ledger goes stale after a config/path/ragignore change. This
    // is the authoritative *current* count for the report.
    let max_bytes = config.rag_max_file_bytes();
    let size_scan = crate::rag::size_report::scan(data_dir, &config.rag_personal_dirs, max_bytes)?;

    // A file's *current* oversize status: the fresh metadata scan is
    // authoritative, and a stale "skipped_oversize" event still flags a file
    // the scan could not see (e.g. a root later unconfigured). If it shrank
    // and got re-indexed, it has chunks and the indexed state wins, so it is
    // not flagged.
    let mut oversize_paths: Vec<std::path::PathBuf> = size_scan.oversize_files.clone();
    for (file, events) in &events_by_file {
        if events
            .first()
            .is_some_and(|e| e.event_type == "skipped_oversize")
            && !chunk_counts.contains_key(file)
            && !oversize_paths.iter().any(|p| p.to_string_lossy() == *file)
        {
            oversize_paths.push(std::path::PathBuf::from(file));
        }
    }

    // Every configured file in exactly one state, derived from one query
    // each — the vector store's chunk map is the authoritative "indexed"
    // count, so the header below cannot disagree with the listing.
    let queued_list: Vec<String> = queue_items.iter().map(|q| q.source_path.clone()).collect();
    let cats = categorize_rag_files(
        chunk_counts.keys().cloned(),
        &oversize_paths,
        &queued_list,
        &size_scan.indexable_files,
    );
    let total_on_disk = size_scan.indexable_files.len() + size_scan.oversize_files.len();

    // The listing below is exactly the union of the four states, so its
    // entry count always reconciles with the header.
    let mut all_files: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    all_files.extend(cats.indexed.iter().cloned());
    all_files.extend(cats.oversize.iter().cloned());
    all_files.extend(cats.queued.iter().cloned());
    all_files.extend(cats.not_yet_indexed.iter().cloned());

    if all_files.is_empty() {
        println!("\n\x1b[1m── Canopy RAG Report ──────────────────────────────────────────\x1b[0m");
        let cap_mb = max_bytes as f64 / (1024.0 * 1024.0);
        println!(" Limit:  {cap_mb:.0} MB per file (config.toml: rag_max_file_mb)");
        println!(
            " {}",
            crate::rag::size_report::exclusion_summary(0, max_bytes)
        );
        println!(
            "\n No indexed RAG data found yet. Make sure the daemon is running and RAG dirs are configured."
        );
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
        crate::rag::status::read_acquisition_state(db, config.embeddings_model.trim()),
    );

    let oversize_count = cats.oversize.len();

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
        match &model_status {
            crate::rag::status::RagModelStatus::Paused => "\x1b[33m⏸ paused\x1b[0m".to_string(),
            crate::rag::status::RagModelStatus::Ready =>
                "\x1b[32m● ready (model loaded)\x1b[0m".to_string(),
            crate::rag::status::RagModelStatus::Sleeping =>
                "\x1b[90m○ sleeping (lazy — loads on demand)\x1b[0m".to_string(),
            crate::rag::status::RagModelStatus::Unavailable(reason) =>
                format!("\x1b[31m✗ unavailable\x1b[0m — {reason}"),
            crate::rag::status::RagModelStatus::Downloading { started_at } => format!(
                "\x1b[33m⬇ downloading\x1b[0m ({}s so far)",
                crate::rag::status::elapsed_secs(*started_at)
            ),
            crate::rag::status::RagModelStatus::Preparing { started_at } => format!(
                "\x1b[33m⚙ preparing\x1b[0m ({}s so far)",
                crate::rag::status::elapsed_secs(*started_at)
            ),
            crate::rag::status::RagModelStatus::DownloadFailed(reason) => format!(
                "\x1b[31m✗ download failed\x1b[0m — {reason} (run 'canopy rag model retry')"
            ),
        }
    );
    if model_status == crate::rag::status::RagModelStatus::Ready {
        if let Some(since) = crate::rag::status::model_loaded_since(db) {
            println!("         since: {}", format_ts(since));
        }
    }
    println!(
        " Total:  {} indexed file(s), {} chunk(s)",
        cats.indexed.len(),
        total_chunks
    );
    let cap_mb = max_bytes as f64 / (1024.0 * 1024.0);
    println!(
        " On disk: {} file(s) ({} indexable, {} oversize)",
        total_on_disk,
        size_scan.indexable_files.len(),
        size_scan.oversize_files.len()
    );
    if !cats.not_yet_indexed.is_empty() {
        println!(
            " Not yet indexed: {} file(s) — run 'canopy rag backfill'",
            cats.not_yet_indexed.len()
        );
    }
    println!(" Limit:  {cap_mb:.0} MB per file (config.toml: rag_max_file_mb)");
    // Adjacent to the limit and always printed, even at zero: an excluded
    // file is a visible fact, not an absence nobody notices (CB20).
    let excl_icon = if oversize_count > 0 {
        "\x1b[33m⚠\x1b[0m"
    } else {
        " "
    };
    println!(
        " {excl_icon}{}",
        crate::rag::size_report::exclusion_summary(oversize_count, max_bytes)
    );
    if !queue_items.is_empty() {
        let queued = queue_items.iter().filter(|q| q.status == "queued").count();
        println!(" Queue:  {} queued, {} indexing", queued, processing_items);
    }

    println!("\n\x1b[1m── Files ──────────────────────────────────────────────────────\x1b[0m");

    for file in &all_files {
        let chunks = chunk_counts.get(file).copied().unwrap_or(0);
        let in_queue = cats.queued.contains(file);
        let events = events_by_file
            .get(file)
            .map(|v| v.as_slice())
            .unwrap_or(&[]);

        let deleted_count = events.iter().filter(|e| e.event_type == "deleted").count();
        let last_error = events.iter().find(|e| e.event_type == "error");
        let file_is_oversize = cats.oversize.contains(file);

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
            let size_bytes: Option<u64> = events
                .first()
                .and_then(|e| e.detail.as_deref())
                .and_then(|d| d.parse().ok())
                .or_else(|| std::fs::metadata(file).ok().map(|m| m.len()));
            if let Some(bytes) = size_bytes {
                println!(
                    "     \x1b[35moversize\x1b[0m: {:.1} MB (exceeds {:.0} MB limit — skipped)",
                    bytes as f64 / (1024.0 * 1024.0),
                    cap_mb
                );
            } else {
                println!(
                    "     \x1b[35moversize\x1b[0m: exceeds {:.0} MB limit — skipped",
                    cap_mb
                );
            }
        }
        if chunks > 0 {
            println!("     chunks: {}", chunks);
        }
        if cats.not_yet_indexed.contains(file) {
            println!("     not yet indexed — run 'canopy rag backfill'");
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

    #[test]
    fn model_retry_parses_as_canopy_rag_model_retry() {
        let cli = TestCli::try_parse_from(["test", "model", "retry"])
            .expect("'model retry' should parse");
        assert!(matches!(
            cli.action,
            RagAction::Model {
                action: ModelAction::Retry
            }
        ));
    }

    #[test]
    #[cfg(feature = "local-embeddings")]
    fn handle_model_retry_clears_a_failed_download() {
        let dir = tempfile::tempdir().unwrap();
        let db = Database::new(&dir.path().join("test.db")).unwrap();
        let config = crate::domain::canopy_config::CanopyConfig {
            embeddings_model: "baai/bge-small-en-v1.5".to_string(),
            ..Default::default()
        };
        config.save(dir.path()).unwrap();
        crate::rag::status::mark_downloading(&db, "baai/bge-small-en-v1.5");
        crate::rag::status::mark_failed(&db, "baai/bge-small-en-v1.5", "connection reset");

        handle_model_retry(dir.path(), &db).unwrap();

        assert_eq!(
            crate::rag::status::read_acquisition_state(&db, "baai/bge-small-en-v1.5"),
            None
        );
    }

    #[test]
    fn handle_model_retry_is_a_no_op_when_nothing_failed() {
        let dir = tempfile::tempdir().unwrap();
        let db = Database::new(&dir.path().join("test.db")).unwrap();
        let config = crate::domain::canopy_config::CanopyConfig {
            embeddings_model: "baai/bge-small-en-v1.5".to_string(),
            ..Default::default()
        };
        config.save(dir.path()).unwrap();

        // Must not error and must not fabricate a state where none exists.
        handle_model_retry(dir.path(), &db).unwrap();
        assert_eq!(
            crate::rag::status::read_acquisition_state(&db, "baai/bge-small-en-v1.5"),
            None
        );
    }

    #[test]
    #[cfg(feature = "local-embeddings")]
    fn handle_model_retry_leaves_an_in_progress_download_alone() {
        let dir = tempfile::tempdir().unwrap();
        let db = Database::new(&dir.path().join("test.db")).unwrap();
        let config = crate::domain::canopy_config::CanopyConfig {
            embeddings_model: "baai/bge-small-en-v1.5".to_string(),
            ..Default::default()
        };
        config.save(dir.path()).unwrap();
        crate::rag::status::mark_downloading(&db, "baai/bge-small-en-v1.5");

        handle_model_retry(dir.path(), &db).unwrap();

        assert!(matches!(
            crate::rag::status::read_acquisition_state(&db, "baai/bge-small-en-v1.5"),
            Some(crate::rag::status::AcquisitionState::Downloading { .. })
        ));
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

    // ── CB35: backfill + unified counting ──────────────────────────

    fn cb35_fixture_dirs() -> (
        tempfile::TempDir,
        std::path::PathBuf,
        Vec<std::path::PathBuf>,
    ) {
        let dir = tempfile::tempdir().unwrap();
        let rag_dir = dir.path().join("docs");
        std::fs::create_dir_all(&rag_dir).unwrap();
        let mut files = Vec::new();
        for i in 0..5 {
            let p = rag_dir.join(format!("file{i}.md"));
            std::fs::write(&p, format!("# File {i}\n")).unwrap();
            files.push(p);
        }
        (dir, rag_dir, files)
    }

    /// Given a store holding a subset of the configured files, the backfill
    /// indexes exactly the remainder and reports those numbers.
    #[test]
    fn backfill_indexes_exactly_the_remainder() {
        let (dir, _rag_dir, files) = cb35_fixture_dirs();
        let db = Database::new(&dir.path().join("test.db")).unwrap();
        let now = chrono::Utc::now().timestamp();

        // 2 of the 5 files are already in the store.
        let indexed: std::collections::HashSet<String> = files[..2]
            .iter()
            .map(|p| p.to_string_lossy().to_string())
            .collect();

        let outcome = run_backfill(&db, &files, &indexed, now);

        assert_eq!(outcome.indexable_on_disk, 5);
        assert_eq!(outcome.already_indexed, 2);
        assert_eq!(outcome.enqueued, 3);
        // Exactly the 3 missing files hit the queue — no more, no fewer.
        let pending = db.list_rag_queue(100).unwrap();
        assert_eq!(pending.len(), 3);
        let queued: std::collections::HashSet<String> =
            pending.into_iter().map(|q| q.source_path).collect();
        for p in &files[2..] {
            assert!(
                queued.contains(&p.to_string_lossy().to_string()),
                "missing file should be queued: {}",
                p.display()
            );
        }
        for p in &files[..2] {
            assert!(
                !queued.contains(&p.to_string_lossy().to_string()),
                "already-indexed file must not be re-enqueued: {}",
                p.display()
            );
        }
    }

    /// Running the backfill twice embeds nothing the second time: re-running
    /// against unchanged progress does not duplicate queue rows (upsert), and
    /// once the remainder is indexed the second run enqueues zero files.
    #[test]
    fn backfill_second_run_embeds_nothing() {
        let (dir, _rag_dir, files) = cb35_fixture_dirs();
        let db = Database::new(&dir.path().join("test.db")).unwrap();
        let now = chrono::Utc::now().timestamp();

        let indexed: std::collections::HashSet<String> = files[..2]
            .iter()
            .map(|p| p.to_string_lossy().to_string())
            .collect();
        let first = run_backfill(&db, &files, &indexed, now);
        assert_eq!(first.enqueued, 3);

        // Same progress, immediate re-run: upsert keeps 3 rows, not 6.
        let retry = run_backfill(&db, &files, &indexed, now + 1);
        assert_eq!(retry.enqueued, 3);
        assert_eq!(db.list_rag_queue(100).unwrap().len(), 3);

        // Daemon indexed the remainder (queue drained, store now complete).
        for q in db.list_rag_queue(100).unwrap() {
            db.remove_rag_item(&q.source_path).unwrap();
        }
        let all_indexed: std::collections::HashSet<String> = files
            .iter()
            .map(|p| p.to_string_lossy().to_string())
            .collect();
        let second = run_backfill(&db, &files, &all_indexed, now + 2);
        assert_eq!(second.enqueued, 0);
        assert_eq!(second.already_indexed, 5);
        assert!(db.list_rag_queue(100).unwrap().is_empty());
    }

    /// The report's claimed file count equals the number of entries it lists:
    /// the header is derived from the same categorized set as the listing, so
    /// they cannot diverge, and no per-file line can outnumber the files.
    #[test]
    fn report_claimed_count_equals_entries_listed() {
        let indexed_paths = vec!["/docs/a.md".to_string(), "/docs/b.md".to_string()];
        let oversize = vec![std::path::PathBuf::from("/docs/big.md")];
        let queued = vec!["/docs/c.md".to_string()];
        let indexable = vec![
            std::path::PathBuf::from("/docs/a.md"),
            std::path::PathBuf::from("/docs/b.md"),
            std::path::PathBuf::from("/docs/c.md"),
            std::path::PathBuf::from("/docs/d.md"),
        ];
        let cats = categorize_rag_files(indexed_paths, &oversize, &queued, &indexable);

        // Header count ...
        let claimed = cats.indexed.len();
        // ... equals the number of entries listed as indexed.
        assert_eq!(claimed, 2);
        let mut listed: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
        listed.extend(cats.indexed.iter().cloned());
        listed.extend(cats.oversize.iter().cloned());
        listed.extend(cats.queued.iter().cloned());
        listed.extend(cats.not_yet_indexed.iter().cloned());
        let listed_indexed = listed.intersection(&cats.indexed).count();
        assert_eq!(listed_indexed, claimed);
        // States are disjoint: the union holds each file exactly once.
        assert_eq!(
            listed.len(),
            cats.indexed.len()
                + cats.oversize.len()
                + cats.queued.len()
                + cats.not_yet_indexed.len()
        );
    }

    /// Indexed plus skipped-for-size plus queued plus not-yet-indexed equals
    /// the files found on the configured roots (real filesystem scan).
    #[test]
    fn indexed_plus_skipped_plus_queued_plus_not_yet_equals_found() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("docs");
        std::fs::create_dir_all(&root).unwrap();
        // 4 small indexable files + 2 big ones (over a 100-byte limit).
        for i in 0..4 {
            std::fs::write(root.join(format!("small{i}.md")), b"tiny").unwrap();
        }
        for i in 0..2 {
            std::fs::write(root.join(format!("big{i}.md")), vec![b'x'; 200]).unwrap();
        }
        let data_dir = dir.path().join(".canopy");
        std::fs::create_dir_all(&data_dir).unwrap();
        let scan =
            crate::rag::size_report::scan(&data_dir, &[root.to_string_lossy().to_string()], 100)
                .unwrap();
        assert_eq!(scan.indexable_files.len(), 4);
        assert_eq!(scan.oversize_files.len(), 2);

        // 1 file already indexed, 1 file queued.
        let indexed_paths = vec![scan.indexable_files[0].to_string_lossy().to_string()];
        let queued = vec![scan.indexable_files[1].to_string_lossy().to_string()];
        let cats = categorize_rag_files(
            indexed_paths,
            &scan.oversize_files,
            &queued,
            &scan.indexable_files,
        );

        let total_on_disk = scan.indexable_files.len() + scan.oversize_files.len();
        assert_eq!(
            cats.indexed.len()
                + cats.oversize.len()
                + cats.queued.len()
                + cats.not_yet_indexed.len(),
            total_on_disk,
            "every configured file must sit in exactly one state"
        );
        assert_eq!(cats.indexed.len(), 1);
        assert_eq!(cats.oversize.len(), 2);
        assert_eq!(cats.queued.len(), 1);
        assert_eq!(cats.not_yet_indexed.len(), 2);
    }

    /// A file over the size limit is reported as skipped-for-size, not as missing.
    #[test]
    fn oversize_file_reported_as_skipped_not_missing() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("docs");
        std::fs::create_dir_all(&root).unwrap();
        let big = root.join("huge.md");
        std::fs::write(&big, vec![b'x'; 500]).unwrap();
        let small = root.join("ok.md");
        std::fs::write(&small, b"fine").unwrap();

        let data_dir = dir.path().join(".canopy");
        std::fs::create_dir_all(&data_dir).unwrap();
        let scan =
            crate::rag::size_report::scan(&data_dir, &[root.to_string_lossy().to_string()], 100)
                .unwrap();

        let cats =
            categorize_rag_files(Vec::new(), &scan.oversize_files, &[], &scan.indexable_files);
        let big_str = big.to_string_lossy().to_string();
        assert!(
            cats.oversize.contains(&big_str),
            "oversize file must be skipped-for-size"
        );
        assert!(
            !cats.not_yet_indexed.contains(&big_str),
            "oversize file must not read as missing/never-indexed"
        );
        assert_eq!(cats.not_yet_indexed.len(), 1);
    }

    /// `canopy rag backfill` is a real subcommand (not just a helper): the
    /// watcher (`auto-index start`) must stay a separate operation.
    #[test]
    fn backfill_parses_as_canopy_rag_backfill() {
        let cli = TestCli::try_parse_from(["test", "backfill"]).expect("'backfill' should parse");
        assert!(matches!(cli.action, RagAction::Backfill));
    }
}
