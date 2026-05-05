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
        println!(
            "  \x1b[32m✓\x1b[0m Data directory: {}",
            canopy_dir.display()
        );
    } else {
        println!(
            "  \x1b[31m✗\x1b[0m Data directory not found: {}",
            canopy_dir.display()
        );
        issues.push("Run 'canopy setup' to initialize");
    }

    if db_path.exists() {
        println!("  \x1b[32m✓\x1b[0m Database: {}", db_path.display());
        if let Ok(db) = Database::new(&db_path) {
            if let Ok(agents) = db.list_agents() {
                let cron_count = agents.iter().filter(|a| a.is_cron()).count();
                let watch_count = agents.iter().filter(|a| a.is_watch()).count();
                println!(
                    "    Agents: {} (cron: {}, watch: {})",
                    agents.len(),
                    cron_count,
                    watch_count
                );
            }
        }
    } else {
        println!("  \x1b[33m⚠\x1b[0m  Database not found (will be created on setup)");
    }

    // Unified config.toml
    let config = crate::domain::canopy_config::CanopyConfig::load(&canopy_dir);
    if config.is_configured() {
        println!("  \x1b[32m✓\x1b[0m Config: config.toml");
        if config.clis.is_empty() {
            println!("    CLIs: (none configured)");
        } else {
            println!("    CLIs: {}", config.cli_names().join(", "));
        }
    } else {
        // Check for legacy files
        let cli_config_path = canopy_dir.join("cli_config.json");
        let configured_marker = canopy_dir.join(".configured");
        if cli_config_path.exists() || configured_marker.exists() {
            println!("  \x1b[33m⚠\x1b[0m  Legacy config files found (run setup to migrate to config.toml)");
        } else {
            println!("  \x1b[33m⚠\x1b[0m  Config not found (run setup)");
        }
    }

    let pid_path = canopy_dir.join("daemon.pid");
    if let Ok(pid_str) = std::fs::read_to_string(&pid_path) {
        if let Ok(pid) = pid_str.trim().parse::<u32>() {
            if is_process_running(pid) {
                println!("  \x1b[32m✓\x1b[0m Daemon running (PID: {})", pid);
            } else {
                println!("  \x1b[31m✗\x1b[0m Daemon not running (stale PID: {})", pid);
                issues.push("Stale PID file — run 'canopy daemon start'");
            }
        }
    } else {
        println!("  \x1b[33m⚠\x1b[0m  Daemon not running");
    }

    if config.is_configured() {
        println!("  \x1b[32m✓\x1b[0m Setup completed");
    } else {
        println!("  \x1b[33m⚠\x1b[0m  Setup not completed");
        issues.push("Run 'canopy setup'");
    }

    let available_clis = crate::domain::models::Cli::detect_available();
    if !available_clis.is_empty() {
        println!(
            "  \x1b[32m✓\x1b[0m CLIs in PATH: {}",
            available_clis
                .iter()
                .map(|c| c.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        );
    } else {
        println!("  \x1b[31m✗\x1b[0m No supported CLIs found in PATH");
        issues.push("Install at least one: opencode, kiro-cli, copilot, or qwen");
    }

    // ── RAG Health ──────────────────────────────────────────────
    println!("\n  \x1b[1;36m◆ Personal RAG\x1b[0m");

    let config = crate::domain::canopy_config::CanopyConfig::load(&canopy_dir);

    // Embeddings model
    if config.embeddings_model.is_empty() {
        println!("  \x1b[33m⚠\x1b[0m  Embeddings model not configured (run 'canopy setup')");
        issues.push("Configure embeddings model via 'canopy setup'");
    } else {
        println!(
            "  \x1b[32m✓\x1b[0m Embeddings model: {}",
            config.embeddings_model
        );
    }

    // Indexed directories
    if config.rag_personal_dirs.is_empty() {
        println!("  \x1b[33m⚠\x1b[0m  No personal RAG directories configured");
        issues.push("Add personal RAG directories via 'canopy setup'");
    } else {
        for dir in &config.rag_personal_dirs {
            let exists = std::path::Path::new(dir).exists();
            if exists {
                println!("  \x1b[32m✓\x1b[0m RAG dir: {dir}");
            } else {
                println!("  \x1b[31m✗\x1b[0m RAG dir missing: {dir}");
                issues.push("Personal RAG directory not found on disk");
            }
        }
    }

    // ragignore file
    let ragignore_path = canopy_dir.join("ragignore");
    if ragignore_path.exists() {
        println!("  \x1b[32m✓\x1b[0m ragignore: {}", ragignore_path.display());
    } else {
        println!("  \x1b[90m–\x1b[0m  ragignore not found (optional — create ~/.canopy/ragignore to exclude files)");
    }

    // Chunk count from DB
    if db_path.exists() {
        let chunk_info: Option<(i64, i64)> = (|| -> Option<(i64, i64)> {
            let conn = rusqlite::Connection::open(&db_path).ok()?;
            let total: i64 = conn
                .query_row("SELECT COUNT(*) FROM rag_chunks", [], |r| r.get(0))
                .ok()?;
            let queued: i64 = conn
                .query_row("SELECT COUNT(*) FROM rag_queue", [], |r| r.get(0))
                .ok()?;
            Some((total, queued))
        })();
        match chunk_info {
            Some((total, queued)) => {
                println!("  \x1b[32m✓\x1b[0m Indexed chunks: {total}");
                if queued > 0 {
                    println!(
                        "  \x1b[33m⚠\x1b[0m  Pending queue: {queued} file(s) awaiting indexing"
                    );
                }
            }
            None => {
                println!("  \x1b[90m–\x1b[0m  Could not read RAG chunk count");
            }
        }
    }

    if !issues.is_empty() {
        println!("\n  \x1b[1;33m⚠ Suggestions:\x1b[0m");
        for issue in &issues {
            println!("    • {}", issue);
        }
    } else {
        println!("\n  \x1b[32m✅ All checks passed!\x1b[0m");
    }
    println!();

    Ok(())
}
