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
mod loop_engine;
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

use anyhow::Result;
use clap::{Parser, Subcommand};
use daemon::bridge::run_bridge;
use daemon::cli::{handle_daemon_action, DaemonAction};
use daemon::doctor::run_doctor;
use daemon::loop_cli::{handle_loop_action, LoopAction};
use daemon::rag_cli::{handle_rag_action, RagAction};
use daemon::spec_cli::{handle_spec_action, SpecAction};
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
    /// Start, stop, or manage the background daemon.
    Daemon {
        #[command(subcommand)]
        action: DaemonAction,
    },
    /// Run a health check diagnosing common issues.
    Doctor,
    /// Run the MCP server over stdio transport.
    Stdio,
    /// First-run setup wizard to configure agents and directories.
    Setup {
        /// Use a local registry directory instead of fetching from GitHub.
        /// Useful for development and testing registry changes before publishing.
        #[arg(long = "local-registry", value_name = "PATH")]
        local_registry: Option<PathBuf>,
        /// Overwrite local skill files that diverge from the sync source,
        /// even if they were modified locally. Default: diverging files are
        /// skipped with a WARN and left untouched.
        #[arg(long = "force-skills")]
        force_skills: bool,
    },
    /// Interactive wizard to configure MCP in your AI client.
    Mcp,
    /// RAG indexing management.
    Rag {
        #[command(subcommand)]
        action: RagAction,
    },
    /// Inspect loop state (read-only).
    Loop {
        #[command(subcommand)]
        action: LoopAction,
    },
    /// Manage standalone specs.
    Spec {
        #[command(subcommand)]
        action: SpecAction,
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
    /// Extract text content from a PDF file (internal use).
    #[command(hide = true)]
    InternalPdfExtract { path: PathBuf },
    /// Start the HTTP API server (used by the daemon).
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
        Some(Commands::Setup {
            local_registry,
            force_skills,
        }) => {
            if let Some(path) = local_registry {
                setup_module::registry_fetch::set_local_registry(path);
            }
            tokio::task::block_in_place(|| setup_module::run_setup(force_skills))?;
            Ok(())
        }
        Some(Commands::Mcp) => {
            tokio::task::block_in_place(mcp_wizard_module::run_mcp_wizard)?;
            Ok(())
        }
        Some(Commands::Rag { action }) => handle_rag_action(action).await,
        Some(Commands::Loop { action }) => handle_loop_action(action).await,
        Some(Commands::Spec { action }) => handle_spec_action(action).await,
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
                    setup_module::run_setup(false)?;
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

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    fn assert_all_subcommands_have_about(cmd: &clap::Command, prefix: &str) {
        for sub in cmd.get_subcommands() {
            let name = sub.get_name();
            let full = if prefix.is_empty() {
                name.to_string()
            } else {
                format!("{prefix} {name}")
            };
            assert!(
                sub.get_about()
                    .is_some_and(|a| !a.to_string().trim().is_empty()),
                "Subcommand '{full}' has no doc comment (about is empty). Add a `///` doc comment."
            );
            assert_all_subcommands_have_about(sub, &full);
        }
    }

    #[test]
    fn all_subcommands_have_help_text() {
        let cmd = <Cli as CommandFactory>::command();
        assert_all_subcommands_have_about(&cmd, "");
    }
}
