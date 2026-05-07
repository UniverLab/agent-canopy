use anyhow::{Context, Result};

use crate::application::ports::AgentRepository;
use crate::daemon::process::is_process_running;
use crate::db::Database;

pub(crate) async fn run_doctor() -> Result<()> {
    use crate::shared::banner;

    banner::print_banner_with_gradient("canopy doctor");
    println!();

    let home = dirs::home_dir().context("No home directory")?;
    let canopy_dir = home.join(".canopy");
    let db_path = canopy_dir.join("background_agents.db");

    let mut issues = Vec::new();

    if canopy_dir.exists() {
        println!(" \x1b[32m✓\x1b[0m Data directory: {}", canopy_dir.display());
    } else {
        println!(
            " \x1b[31m✗\x1b[0m Data directory not found: {}",
            canopy_dir.display()
        );
        issues.push("Run 'canopy setup' to initialize");
    }

    if db_path.exists() {
        println!(" \x1b[32m✓\x1b[0m Database: {}", db_path.display());
        if let Ok(db) = Database::new(&db_path) {
            if let Ok(agents) = db.list_agents() {
                let cron_count = agents.iter().filter(|a| a.is_cron()).count();
                let watch_count = agents.iter().filter(|a| a.is_watch()).count();
                println!(
                    " Agents: {} (cron: {}, watch: {})",
                    agents.len(),
                    cron_count,
                    watch_count
                );
            }
        }
    } else {
        println!(" \x1b[33m⚠\x1b[0m Database not found (will be created on setup)");
    }

    let config = crate::domain::canopy_config::CanopyConfig::load(&canopy_dir);
    if config.is_configured() {
        println!(" \x1b[32m✓\x1b[0m Config: config.toml");
        if config.clis.is_empty() {
            println!(" Harnesses: (none configured)");
        } else {
            println!(" Harnesses: {}", config.cli_names().join(", "));
        }
    } else {
        let cli_config_path = canopy_dir.join("cli_config.json");
        let configured_marker = canopy_dir.join(".configured");
        if cli_config_path.exists() || configured_marker.exists() {
            println!(
                " \x1b[33m⚠\x1b[0m Legacy config files found (run setup to migrate to config.toml)"
            );
        } else {
            println!(" \x1b[33m⚠\x1b[0m Config not found (run setup)");
        }
    }

    let pid_path = canopy_dir.join("daemon.pid");
    if let Ok(pid_str) = std::fs::read_to_string(&pid_path) {
        if let Ok(pid) = pid_str.trim().parse::<u32>() {
            if is_process_running(pid) {
                println!(" \x1b[32m✓\x1b[0m Daemon running (PID: {})", pid);
            } else {
                println!(" \x1b[31m✗\x1b[0m Daemon not running (stale PID: {})", pid);
                issues.push("Stale PID file — run 'canopy daemon start'");
            }
        }
    } else {
        println!(" \x1b[33m⚠\x1b[0m Daemon not running");
    }

    if config.is_configured() {
        println!(" \x1b[32m✓\x1b[0m Setup completed");
    } else {
        println!(" \x1b[33m⚠\x1b[0m Setup not completed");
        issues.push("Run 'canopy setup'");
    }

    let available_clis = crate::domain::models::Cli::detect_available();
    if !available_clis.is_empty() {
        println!(
            " \x1b[32m✓\x1b[0m Harnesses in PATH: {}",
            available_clis
                .iter()
                .map(|c| c.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        );
    } else {
        println!(" \x1b[31m✗\x1b[0m No supported harnesses found in PATH");
        issues.push("Install at least one: opencode, kiro-cli, copilot, or qwen");
    }

    // ── RAG Health ──────────────────────────────────────────────
    println!(" \x1b[1;36m◆ Personal RAG\x1b[0m");

    let config = crate::domain::canopy_config::CanopyConfig::load(&canopy_dir);

    if config.embeddings_model.is_empty() {
        println!(" \x1b[31m✗\x1b[0m Embeddings model not configured (run 'canopy setup')");
        issues.push("Configure embeddings model via 'canopy setup'");
    } else {
        println!(
            " \x1b[32m✓\x1b[0m Embeddings model: {}",
            config.embeddings_model
        );

        // Check that the required API key is present.
        let api_key_info = match crate::rag::embedding_client::provider_for_model(
            &config.embeddings_model,
        ) {
            Some(crate::rag::embedding_client::EmbeddingProvider::OpenAi) => {
                Some(("OPENAI_API_KEY", std::env::var("OPENAI_API_KEY").is_ok()))
            }
            Some(crate::rag::embedding_client::EmbeddingProvider::Gemini) => {
                Some(("GEMINI_API_KEY", std::env::var("GEMINI_API_KEY").is_ok()))
            }
            None => {
                println!(
                        " \x1b[31m✗\x1b[0m Model '{}' is not supported — only OpenAI and Google models are supported. Run 'canopy setup' to pick a compatible model.",
                        config.embeddings_model
                    );
                issues.push("Run 'canopy setup' and select an OpenAI or Google embedding model");
                None
            }
        };

        if let Some((key_var, present)) = api_key_info {
            if present {
                println!(" \x1b[32m✓\x1b[0m API key {key_var} is set");
            } else {
                println!(" \x1b[31m✗\x1b[0m {key_var} is NOT set — indexing will fail silently");
                issues.push("Export the required API key before starting the daemon");
            }
        }
    }

    if config.rag_personal_dirs.is_empty() {
        println!(" \x1b[33m⚠\x1b[0m No personal RAG directories configured");
        issues.push("Add personal RAG directories via 'canopy setup'");
    } else {
        let mut total_files: usize = 0;
        for dir in &config.rag_personal_dirs {
            let path = std::path::Path::new(dir);
            if path.exists() {
                let file_count = walkdir::WalkDir::new(path)
                    .follow_links(false)
                    .into_iter()
                    .filter_map(|e| e.ok())
                    .filter(|e| {
                        e.file_type().is_file()
                            && crate::rag::chunker::detect_lang(&e.path().to_string_lossy())
                                .is_some()
                    })
                    .count();
                println!(" \x1b[32m✓\x1b[0m RAG dir: {dir} ({file_count} indexable file(s))");
                total_files += file_count;
            } else {
                println!(" \x1b[31m✗\x1b[0m RAG dir missing: {dir}");
                issues.push("Personal RAG directory not found on disk");
            }
        }
        if total_files == 0 && !config.rag_personal_dirs.is_empty() {
            println!(
                " \x1b[33m⚠\x1b[0m No indexable files found (.md, .mdx, .pdf) in RAG directories"
            );
        }
    }

    let ragignore_path = canopy_dir.join("ragignore");
    if ragignore_path.exists() {
        println!(" \x1b[32m✓\x1b[0m ragignore: {}", ragignore_path.display());
    } else {
        println!(" \x1b[90m–\x1b[0m ragignore not found (optional — create ~/.canopy/ragignore to exclude files)");
    }

    // LanceDB vector store
    let lancedb_path = match crate::rag::vector_store::VectorStore::default_lancedb_path() {
        Ok(p) => p,
        Err(_) => {
            println!(" \x1b[33m⚠\x1b[0m Could not determine LanceDB path");
            issues.push("Home directory not found");
            dirs::home_dir()
                .unwrap_or_default()
                .join(".canopy/rag/vectors.lancedb")
        }
    };

    if lancedb_path.exists() {
        println!(" \x1b[32m✓\x1b[0m Vector store: {}", lancedb_path.display());
    } else {
        println!(
            " \x1b[90m–\x1b[0m Vector store not yet created (will be created on first indexing)"
        );
    }

    // Chunk count from LanceDB
    if !config.embeddings_model.is_empty() {
        if let Ok(dimensions) =
            crate::rag::embedding_client::model_dimensions(&config.embeddings_model)
        {
            match crate::rag::vector_store::VectorStore::new(dimensions).await {
                Ok(store) => match store.count_chunks().await {
                    Ok(total) => {
                        if total > 0 {
                            println!(" \x1b[32m✓\x1b[0m Indexed chunks: {total}");
                            if let Ok(unique) = store.count_unique_paths().await {
                                println!(" \x1b[32m✓\x1b[0m Indexed files: {unique}");

                                // Surface mismatch between files on disk and indexed files.
                                let disk_files: usize = config
                                    .rag_personal_dirs
                                    .iter()
                                    .map(std::path::Path::new)
                                    .filter(|p| p.exists())
                                    .flat_map(|p| {
                                        walkdir::WalkDir::new(p)
                                            .follow_links(false)
                                            .into_iter()
                                            .filter_map(|e| e.ok())
                                            .filter(|e| {
                                                e.file_type().is_file()
                                                    && crate::rag::chunker::detect_lang(
                                                        &e.path().to_string_lossy(),
                                                    )
                                                    .is_some()
                                            })
                                    })
                                    .count();

                                if disk_files > 0 && (unique as usize) < disk_files {
                                    println!(
                                        " \x1b[33m⚠\x1b[0m {disk_files} indexable file(s) on disk but only {unique} indexed — \
                                         check daemon logs for embedding errors"
                                    );
                                    issues.push(
                                        "Some files may not be indexed — verify API key and daemon logs",
                                    );
                                }
                            }
                        } else {
                            println!(" \x1b[31m✗\x1b[0m No chunks indexed yet");
                            if !config.rag_personal_dirs.is_empty() {
                                issues.push(
                                    "RAG directories are configured but nothing is indexed — \
                                     ensure the daemon is running and the API key env var is set",
                                );
                            }
                        }
                    }
                    Err(_) => {
                        println!(" \x1b[90m–\x1b[0m Could not read chunk count from LanceDB");
                    }
                },
                Err(e) => {
                    println!(" \x1b[31m✗\x1b[0m Could not open LanceDB: {e}");
                    issues.push("LanceDB open error — check if the embeddings model is supported");
                }
            }
        }
    }

    // Queue count from SQLite
    if db_path.exists() {
        if let Ok(db) = Database::new(&db_path) {
            if let Ok((queued, processing)) = db.rag_queue_counts() {
                if queued > 0 || processing > 0 {
                    if processing > 0 {
                        println!(
                            " \x1b[33m⚠\x1b[0m Pending queue: {queued} queued, {processing} indexing"
                        );
                    } else {
                        println!(
                            " \x1b[33m⚠\x1b[0m Pending queue: {queued} file(s) awaiting indexing"
                        );
                    }
                }
            }
        }
    }

    if !issues.is_empty() {
        println!("\n \x1b[1;33m⚠ Suggestions:\x1b[0m");
        for issue in &issues {
            println!(" • {}", issue);
        }
    } else {
        println!("\n \x1b[32m✅ All checks passed!\x1b[0m");
    }
    println!();

    Ok(())
}
