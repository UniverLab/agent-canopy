//! Shared daemon-call plumbing for the state-changing `canopy loop`/`canopy
//! spec` subcommands (run, pause, continue, reset, autorun, spec create).
//!
//! Every one of these reaches the exact same MCP tool the daemon already
//! exposes over its streamable-HTTP endpoint — the same interface
//! `canopy bridge` and the TUI use (`tui::mcp_client::call_daemon_tool`) —
//! rather than opening the database directly and reimplementing the
//! engine's state transitions in the CLI. That second path is the exact
//! failure mode this module exists to avoid: a CLI write must delegate to
//! the daemon, never race or duplicate it.

use anyhow::{Context, Result};

use crate::daemon::bridge::resolve_bridge_port;
use crate::tui::mcp_client::call_daemon_tool;

/// Call one daemon MCP tool and return its result text on success.
///
/// A transport failure (the daemon isn't listening on the resolved port) and
/// a tool-level rejection (e.g. "Loop not found") surface as distinct error
/// messages, so a caller can never mistake "the daemon is unreachable" for
/// "the loop doesn't exist" — the ambiguity that made the 2026-07-29
/// port-hijack incident hard to diagnose from the read-only CLI alone.
pub(crate) fn call_tool(
    port_override: Option<u16>,
    tool: &str,
    arguments: &serde_json::Value,
) -> Result<String> {
    let port = resolve_bridge_port(port_override);
    let outcome = call_daemon_tool(&port.to_string(), tool, arguments).with_context(|| {
        format!(
            "canopy: could not reach the daemon on port {port}. Check `canopy daemon status`, \
             or start it with `canopy daemon start`."
        )
    })?;

    if outcome.is_error {
        return Err(anyhow::anyhow!(outcome.text));
    }
    Ok(outcome.text)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::mcp_client::test_support::{spawn_fake_daemon, unused_port};

    #[test]
    fn call_tool_returns_text_on_success() {
        let fake = spawn_fake_daemon(serde_json::json!({
            "content": [{"type": "text", "text": "Loop 'x' launched in background."}],
            "isError": false
        }));
        let port: u16 = fake.port.parse().unwrap();

        let text = call_tool(Some(port), "loop_run", &serde_json::json!({"loop_id": "x"})).unwrap();

        assert_eq!(text, "Loop 'x' launched in background.");
        let calls = fake.recorded_calls();
        assert_eq!(calls[0]["name"], "loop_run");
    }

    #[test]
    fn call_tool_surfaces_tool_level_errors_distinctly() {
        let fake = spawn_fake_daemon(serde_json::json!({
            "content": [{"type": "text", "text": "Loop 'x' not found."}],
            "isError": true
        }));
        let port: u16 = fake.port.parse().unwrap();

        let err =
            call_tool(Some(port), "loop_run", &serde_json::json!({"loop_id": "x"})).unwrap_err();

        assert!(err.to_string().contains("not found"));
        // A tool-level rejection must never be dressed up as an unreachable
        // daemon — the two are diagnosed completely differently.
        assert!(!err.to_string().contains("could not reach the daemon"));
    }

    #[test]
    fn call_tool_reports_daemon_unreachable_distinctly() {
        // `unused_port()` frees the port before returning it, which leaves a
        // brief window where another test could claim the same port first.
        // Retry with a fresh port on the rare miss instead of flaking.
        for _ in 0..5 {
            let port: u16 = unused_port().parse().unwrap();
            match call_tool(Some(port), "loop_run", &serde_json::json!({"loop_id": "x"})) {
                Err(err) => {
                    let chain: Vec<String> = err.chain().map(ToString::to_string).collect();
                    assert!(chain
                        .iter()
                        .any(|m| m.contains("could not reach the daemon")));
                    assert!(chain.iter().any(|m| m.contains("not reachable")));
                    return;
                }
                Ok(_) => continue,
            }
        }
        panic!("port kept getting claimed by another test after 5 attempts");
    }
}
