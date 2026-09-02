use anyhow::Result;
use clap::Subcommand;

use crate::daemon::cli_daemon::call_tool;

#[derive(Subcommand)]
pub(crate) enum SubagentAction {
    /// Launch an ephemeral subagent.
    Spawn {
        #[arg(long)]
        prompt: String,
        #[arg(long)]
        cli: Option<String>,
        #[arg(long)]
        model: Option<String>,
        #[arg(long = "mcp-server")]
        mcp_servers: Vec<String>,
        #[arg(long = "timeout", default_value = "15")]
        timeout_minutes: u64,
        #[arg(long = "ttl", default_value = "60")]
        ttl_minutes: u64,
    },
    /// Collect the result of an ephemeral subagent.
    Collect {
        /// The run ID returned by spawn.
        id: String,
    },
}

pub(crate) async fn handle_subagent_action(
    action: SubagentAction,
    port_override: Option<u16>,
) -> Result<()> {
    // The subagent process and its completion writer are owned by the daemon,
    // not this short-lived CLI — otherwise the result would never be written
    // back for `collect` to find. Same routing every other loop/spec CLI
    // subcommand uses.
    match action {
        SubagentAction::Spawn {
            prompt,
            cli,
            model,
            mcp_servers,
            timeout_minutes,
            ttl_minutes,
        } => {
            let mut args = serde_json::json!({
                "prompt": prompt,
                "timeout_minutes": timeout_minutes,
                "ttl_minutes": ttl_minutes,
            });
            if let Some(cli) = cli {
                args["cli"] = serde_json::json!(cli);
            }
            if let Some(model) = model {
                args["model"] = serde_json::json!(model);
            }
            if !mcp_servers.is_empty() {
                args["mcp_servers"] = serde_json::json!(mcp_servers);
            }
            // Inherit the caller's workdir explicitly: the daemon's own cwd is
            // not this invocation's.
            if let Ok(cwd) = std::env::current_dir() {
                if let Some(cwd) = cwd.to_str() {
                    args["workdir"] = serde_json::json!(cwd);
                }
            }
            println!("{}", call_tool(port_override, "subagent_spawn", &args)?);
            Ok(())
        }
        SubagentAction::Collect { id } => {
            let args = serde_json::json!({ "id": id });
            println!("{}", call_tool(port_override, "subagent_collect", &args)?);
            Ok(())
        }
    }
}
