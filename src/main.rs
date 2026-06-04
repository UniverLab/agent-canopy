#![allow(clippy::doc_markdown)]
//! canopy — MCP server for AI agent background_agent scheduling and file watching.
//!
//! Binary modes:
//! - `daemon start` — start the MCP server as a persistent background process
//! - `daemon stop` — stop the running daemon
//! - `daemon status` — check daemon health
//! - `stdio` — run in stdio MCP transport mode (legacy/fallback)
//! - (no args) — start in foreground with Streamable HTTP transport

mod application;
mod autoupdate;
mod config;
mod daemon;
mod db;
mod domain;
mod executor;
mod mcp_wizard_module;
mod rag;
mod scheduler;
mod setup_module;
mod shared;
mod skills_module;
mod sync_manager;
mod system;
mod tui;
mod watchers;
mod workflow_engine;

use anyhow::Result;
use clap::{Parser, Subcommand};
use daemon::bridge::run_bridge;
use daemon::cli::{handle_daemon_action, DaemonAction};
use daemon::doctor::run_doctor;
use daemon::rag_cli::{handle_rag_action, RagAction};
use daemon::server::{run_http_server, run_stdio_server};
use std::path::PathBuf;

#[derive(Parser)]
#[command(name = "canopy", version, about)]
struct Cli {
    #[command(subcommand)]
    command: Option<Commands>,

    #[arg(long, short, global = true)]
    port: Option<u16>,
}

#[derive(Subcommand)]
enum Commands {
    Daemon {
        #[command(subcommand)]
        action: DaemonAction,
    },
    Doctor,
    Stdio,
    Setup {
        /// Use a local registry directory instead of fetching from GitHub.
        /// Useful for development and testing registry changes before publishing.
        #[arg(long = "local-registry", value_name = "PATH")]
        local_registry: Option<PathBuf>,
    },
    Mcp,
    /// RAG indexing management.
    Rag {
        #[command(subcommand)]
        action: RagAction,
    },
    /// Run a stdio sidecar proxy that injects canopy identity headers.
    Bridge {
        /// Agent session ID to bind this bridge process.
        #[arg(long = "id")]
        agent_id: Option<String>,
        /// Explicit daemon port override.
        #[arg(long)]
        port: Option<u16>,
        /// Working directory forwarded to the daemon.
        #[arg(long)]
        workdir: Option<PathBuf>,
    },
    #[command(hide = true)]
    InternalPdfExtract {
        path: PathBuf,
    },
    #[command(hide = true)]
    Serve,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Some(Commands::Daemon { action }) => handle_daemon_action(action, cli.port).await,
        Some(Commands::Doctor) => run_doctor().await,
        Some(Commands::Stdio) => run_stdio_server().await,
        Some(Commands::Serve) => run_http_server(cli.port).await,
        Some(Commands::Setup { local_registry }) => {
            if let Some(path) = local_registry {
                setup_module::registry_fetch::set_local_registry(path);
            }
            tokio::task::block_in_place(setup_module::run_setup)?;
            Ok(())
        }
        Some(Commands::Mcp) => {
            tokio::task::block_in_place(mcp_wizard_module::run_mcp_wizard)?;
            Ok(())
        }
        Some(Commands::Rag { action }) => handle_rag_action(action).await,
        Some(Commands::Bridge {
            agent_id,
            port,
            workdir,
        }) => run_bridge(agent_id, port.or(cli.port), workdir).await,
        Some(Commands::InternalPdfExtract { path }) => {
            rag::ingestion::run_internal_pdf_extract(&path)
        }
        None => {
            tokio::task::block_in_place(|| {
                if setup_module::needs_setup() {
                    setup_module::run_setup()?;
                }
                setup_module::maybe_refresh_registry();
                let _ = autoupdate::check_and_update_if_needed();
                tui::run_tui()
            })?;
            Ok(())
        }
    }
}

pub(crate) fn resolve_port(port_override: Option<u16>) -> u16 {
    port_override
        .or_else(|| {
            std::env::var("CANOPY_PORT")
                .ok()
                .and_then(|p| p.parse::<u16>().ok())
        })
        .unwrap_or(7755)
}

pub(crate) fn ensure_data_dir() -> Result<std::path::PathBuf> {
    let home =
        dirs::home_dir().ok_or_else(|| anyhow::anyhow!("Could not determine home directory"))?;
    let data_dir = home.join(".canopy");
    std::fs::create_dir_all(&data_dir)?;
    std::fs::create_dir_all(data_dir.join("logs"))?;
    Ok(data_dir)
}
