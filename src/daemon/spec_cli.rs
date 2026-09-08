//! CLI handlers for `canopy spec` subcommands.
//!
//! Every subcommand here changes state (a spec's status, or the backlog
//! itself), so each one delegates to the daemon's MCP tool of the same
//! purpose (`spec_set_status`, `spec_create`, `queue_add_spec`) via
//! `daemon::cli_daemon::call_tool` rather than writing to the database
//! directly — see `loop_cli.rs` for the same pattern applied to loop
//! state.

use anyhow::{anyhow, Result};
use clap::Subcommand;

use crate::daemon::cli_daemon::call_tool;

#[derive(Subcommand)]
pub enum SpecAction {
    /// Mark a spec as completed.
    Complete {
        /// Spec ID to complete.
        spec_id: String,
        /// Reason for completion.
        #[arg(long)]
        reason: String,
    },
    /// Mark a spec as skipped.
    Skip {
        /// Spec ID to skip.
        spec_id: String,
        /// Reason for skipping.
        #[arg(long)]
        reason: String,
    },
    /// Reopen a completed/skipped spec back to pending.
    Reopen {
        /// Spec ID to reopen.
        spec_id: String,
        /// Reason for reopening.
        #[arg(long)]
        reason: String,
    },
    /// Create a standalone spec (a backlog item) and optionally add it to
    /// an existing queue.
    Create {
        /// Human-readable spec name.
        #[arg(long)]
        name: String,
        /// Spec description. Must use the tagged `<spec>` format with all
        /// seven section tags: `<objective>`, `<functional_requirements>`,
        /// `<non_functional_requirements>`, `<constraints>`, `<guidelines>`,
        /// `<in_scope>`, `<out_of_scope>`. Markdown is allowed inside each section.
        #[arg(long)]
        description: String,
        /// Absolute workdir tag, for backlog filtering only.
        #[arg(long)]
        workdir: Option<String>,
        /// Existing queue ID to append the new spec to.
        #[arg(long)]
        queue: Option<String>,
        /// Context group within the queue (only meaningful with --queue).
        #[arg(long)]
        group: Option<String>,
    },
    /// Convert all legacy heading-format specs to the tagged `<spec>` format.
    Convert,
}

pub async fn handle_spec_action(action: SpecAction, port_override: Option<u16>) -> Result<()> {
    match action {
        SpecAction::Complete { spec_id, reason } => {
            set_status(port_override, &spec_id, "completed", &reason)
        }
        SpecAction::Skip { spec_id, reason } => {
            set_status(port_override, &spec_id, "skipped", &reason)
        }
        SpecAction::Reopen { spec_id, reason } => {
            set_status(port_override, &spec_id, "pending", &reason)
        }
        SpecAction::Create {
            name,
            description,
            workdir,
            queue,
            group,
        } => create_spec(
            port_override,
            &name,
            &description,
            workdir.as_deref(),
            queue.as_deref(),
            group.as_deref(),
        ),
        SpecAction::Convert => convert_specs(port_override),
    }
}

fn set_status(port_override: Option<u16>, spec_id: &str, status: &str, reason: &str) -> Result<()> {
    let text = call_tool(
        port_override,
        "spec_set_status",
        &serde_json::json!({ "spec_id": spec_id, "status": status, "reason": reason }),
    )?;
    println!("{text}");
    Ok(())
}

fn create_spec(
    port_override: Option<u16>,
    name: &str,
    description: &str,
    workdir: Option<&str>,
    queue: Option<&str>,
    group: Option<&str>,
) -> Result<()> {
    let mut args = serde_json::json!({ "name": name, "description": description });
    if let Some(workdir) = workdir {
        args["workdir"] = serde_json::json!(workdir);
    }

    let text = call_tool(port_override, "spec_create", &args)?;
    let spec_id = serde_json::from_str::<serde_json::Value>(&text)
        .ok()
        .and_then(|value| {
            value
                .get("spec_id")
                .and_then(|id| id.as_str())
                .map(str::to_string)
        })
        .ok_or_else(|| anyhow!("Unexpected response from spec_create: {text}"))?;
    println!("Spec '{spec_id}' created.");

    if let Some(queue_id) = queue {
        let mut queue_args = serde_json::json!({ "queue_id": queue_id, "spec_id": spec_id });
        if let Some(group) = group {
            queue_args["group"] = serde_json::json!(group);
        }
        let queue_text = call_tool(port_override, "queue_add_spec", &queue_args)?;
        println!("{queue_text}");
    }

    Ok(())
}

fn convert_specs(port_override: Option<u16>) -> Result<()> {
    let text = call_tool(port_override, "spec_convert", &serde_json::json!({}))?;
    println!("{text}");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::mcp_client::test_support::{spawn_fake_daemon, unused_port};
    use clap::Parser;

    #[derive(Parser)]
    struct TestCli {
        #[command(subcommand)]
        action: SpecAction,
    }

    #[test]
    fn spec_action_complete_variant() {
        let cli = TestCli::try_parse_from(["test", "complete", "spec-1", "--reason", "done"])
            .expect("should parse");
        match cli.action {
            SpecAction::Complete { spec_id, reason } => {
                assert_eq!(spec_id, "spec-1");
                assert_eq!(reason, "done");
            }
            _ => panic!("Expected Complete variant"),
        }
    }

    #[test]
    fn spec_action_skip_variant() {
        let cli = TestCli::try_parse_from(["test", "skip", "spec-1", "--reason", "n/a"])
            .expect("should parse");
        match cli.action {
            SpecAction::Skip { spec_id, reason } => {
                assert_eq!(spec_id, "spec-1");
                assert_eq!(reason, "n/a");
            }
            _ => panic!("Expected Skip variant"),
        }
    }

    #[test]
    fn spec_action_reopen_variant() {
        let cli = TestCli::try_parse_from(["test", "reopen", "spec-1", "--reason", "retry"])
            .expect("should parse");
        match cli.action {
            SpecAction::Reopen { spec_id, reason } => {
                assert_eq!(spec_id, "spec-1");
                assert_eq!(reason, "retry");
            }
            _ => panic!("Expected Reopen variant"),
        }
    }

    #[test]
    fn spec_action_create_parses_all_fields() {
        let cli = TestCli::try_parse_from([
            "test",
            "create",
            "--name",
            "My Spec",
            "--description",
            "Objective: x. Functional requirements: y.",
            "--workdir",
            "/tmp/proj",
            "--queue",
            "queue-1",
            "--group",
            "g1",
        ])
        .expect("should parse");
        match cli.action {
            SpecAction::Create {
                name,
                description,
                workdir,
                queue,
                group,
            } => {
                assert_eq!(name, "My Spec");
                assert!(description.contains("Objective"));
                assert_eq!(workdir.as_deref(), Some("/tmp/proj"));
                assert_eq!(queue.as_deref(), Some("queue-1"));
                assert_eq!(group.as_deref(), Some("g1"));
            }
            _ => panic!("Expected Create variant"),
        }
    }

    #[test]
    fn spec_action_create_requires_name_and_description() {
        assert!(TestCli::try_parse_from(["test", "create"]).is_err());
        assert!(TestCli::try_parse_from(["test", "create", "--name", "x"]).is_err());
    }

    #[test]
    fn set_status_calls_spec_set_status_with_expected_status_string() {
        let fake = spawn_fake_daemon(serde_json::json!({
            "content": [{"type": "text", "text": "Spec 'spec-1' set to 'completed' (admin): done"}],
            "isError": false
        }));
        let port: u16 = fake.port.parse().unwrap();

        set_status(Some(port), "spec-1", "completed", "done").expect("should succeed");

        let calls = fake.recorded_calls();
        assert_eq!(calls[0]["name"], "spec_set_status");
        assert_eq!(calls[0]["arguments"]["spec_id"], "spec-1");
        assert_eq!(calls[0]["arguments"]["status"], "completed");
        assert_eq!(calls[0]["arguments"]["reason"], "done");
    }

    #[test]
    fn set_status_surfaces_tool_level_errors() {
        let fake = spawn_fake_daemon(serde_json::json!({
            "content": [{"type": "text", "text": "Spec 'spec-1' not found."}],
            "isError": true
        }));
        let port: u16 = fake.port.parse().unwrap();

        let err = set_status(Some(port), "spec-1", "completed", "done").unwrap_err();
        assert!(err.to_string().contains("not found"));
    }

    #[test]
    fn create_spec_without_queue_calls_only_spec_create() {
        let fake = spawn_fake_daemon(serde_json::json!({
            "content": [{"type": "text", "text": "{\"spec_id\":\"new-spec-1\"}"}],
            "isError": false
        }));
        let port: u16 = fake.port.parse().unwrap();

        create_spec(Some(port), "My Spec", "Objective: x.", None, None, None)
            .expect("should succeed");

        let calls = fake.recorded_calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0]["name"], "spec_create");
        assert_eq!(calls[0]["arguments"]["name"], "My Spec");
        assert_eq!(calls[0]["arguments"]["description"], "Objective: x.");
    }

    #[test]
    fn create_spec_with_queue_also_calls_queue_add_spec() {
        // The fake daemon always answers every `tools/call` with the same
        // canned result, so a single fixture text that's valid for both
        // `spec_create` (parsed for `spec_id`) and `queue_add_spec`
        // (printed as-is) covers this round trip.
        let fake = spawn_fake_daemon(serde_json::json!({
            "content": [{"type": "text", "text": "{\"spec_id\":\"new-spec-1\"}"}],
            "isError": false
        }));
        let port: u16 = fake.port.parse().unwrap();

        create_spec(
            Some(port),
            "My Spec",
            "Objective: x.",
            Some("/tmp/proj"),
            Some("queue-1"),
            Some("g1"),
        )
        .expect("should succeed");

        let calls = fake.recorded_calls();
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0]["name"], "spec_create");
        assert_eq!(calls[0]["arguments"]["workdir"], "/tmp/proj");
        assert_eq!(calls[1]["name"], "queue_add_spec");
        assert_eq!(calls[1]["arguments"]["queue_id"], "queue-1");
        assert_eq!(calls[1]["arguments"]["spec_id"], "new-spec-1");
        assert_eq!(calls[1]["arguments"]["group"], "g1");
    }

    #[test]
    fn create_spec_errors_on_unparseable_spec_create_response() {
        let fake = spawn_fake_daemon(serde_json::json!({
            "content": [{"type": "text", "text": "not json"}],
            "isError": false
        }));
        let port: u16 = fake.port.parse().unwrap();

        let err =
            create_spec(Some(port), "My Spec", "Objective: x.", None, None, None).unwrap_err();
        assert!(err.to_string().contains("Unexpected response"));
    }

    /// The daemon-unreachable path must read distinctly from a tool-level
    /// rejection — this is the CLI's whole reason for being.
    #[test]
    fn set_status_reports_daemon_unreachable() {
        for _ in 0..5 {
            let port: u16 = unused_port().parse().unwrap();
            match set_status(Some(port), "spec-1", "completed", "done") {
                Err(err) => {
                    let chain: Vec<String> = err.chain().map(ToString::to_string).collect();
                    assert!(chain
                        .iter()
                        .any(|m| m.contains("could not reach the daemon")));
                    return;
                }
                Ok(()) => continue,
            }
        }
        panic!("port kept getting claimed by another test after 5 attempts");
    }

    #[test]
    fn spec_action_convert_variant_parses() {
        let cli = TestCli::try_parse_from(["test", "convert"]).expect("should parse");
        assert!(matches!(cli.action, SpecAction::Convert));
    }

    #[test]
    fn convert_specs_calls_spec_convert_and_prints_report() {
        let fake = spawn_fake_daemon(serde_json::json!({
            "content": [{"type": "text", "text": "{\"scanned\":3,\"converted\":1}"}],
            "isError": false
        }));
        let port: u16 = fake.port.parse().unwrap();

        convert_specs(Some(port)).expect("should succeed");

        let calls = fake.recorded_calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0]["name"], "spec_convert");
    }
}
