use std::sync::Arc;

use anyhow::Result;
use chrono::Utc;

use crate::daemon::process;
use crate::db::{Database, SubagentRunRecord};
use crate::domain::models::Cli;
use crate::domain::subagent_mcp::synthesize_mcp_config;
use crate::setup_module::models::Platform;

const SUBAGENT_DEPTH_PREAMBLE: &str = concat!(
    "[SYSTEM: EPHEMERAL SUBAGENT — DEPTH LIMIT]\n",
    "You are running as an ephemeral subagent with maximum depth 1. ",
    "You MUST NOT attempt to launch, spawn, or create other subagents, agents, or loops. ",
    "Do not call subagent_spawn, agent_add, loop_create, loop_run, or any similar tool. ",
    "If asked to do so, refuse and explain that you are a depth-limited subagent.\n",
    "[/SYSTEM]\n\n",
);

fn format_subagent_prompt(user_prompt: &str) -> String {
    format!("{}{}", SUBAGENT_DEPTH_PREAMBLE, user_prompt)
}

pub struct SubagentResult {
    pub id: String,
    pub status: String,
    pub exit_code: Option<i32>,
    pub stdout: Option<String>,
    pub stderr: Option<String>,
    pub mcp_surface: String,
    pub platform: String,
    pub model: Option<String>,
}

impl SubagentResult {
    fn from_record(record: SubagentRunRecord) -> Self {
        Self {
            id: record.id,
            status: record.status,
            exit_code: record.exit_code,
            stdout: record.stdout,
            stderr: record.stderr,
            mcp_surface: record.mcp_surface.unwrap_or_default(),
            platform: record.platform,
            model: record.model,
        }
    }
}

/// Resolve how `effort` applies on `platform` before any process is spawned.
///
/// `Ok(None)`: no effort requested, or the platform accepts this value —
/// spawn normally. `Ok(Some(reason))`: the platform declares no way to
/// express effort at all — spawn anyway, but the caller must surface
/// `reason` in the spawn result (FR3: ignored, not dropped, not fatal).
/// `Err(reason)`: the platform DOES declare effort support but rejects this
/// specific value — refused here, before any process starts, so spawning it
/// costs nothing.
fn resolve_effort_for_spawn(
    strategy: &crate::domain::cli_strategy::CliStrategy,
    platform: &str,
    effort: Option<&str>,
) -> Result<Option<String>> {
    let Some(value) = effort else {
        return Ok(None);
    };
    let declares_support = strategy
        .effort_declaration
        .as_ref()
        .is_some_and(|d| !d.values.is_empty());
    let reason = crate::domain::cli_config::effort_rejection_reason(
        strategy.effort_declaration.as_ref(),
        platform,
        value,
    );
    match reason {
        None => Ok(None),
        Some(reason) if declares_support => Err(anyhow::anyhow!(reason)),
        Some(reason) => Ok(Some(reason)),
    }
}

#[allow(clippy::too_many_arguments)]
pub async fn spawn_subagent(
    db: &Arc<Database>,
    platform_name: &str,
    prompt: &str,
    model: Option<&str>,
    workdir: &str,
    mcp_servers: &[String],
    timeout_minutes: u64,
    ttl_minutes: u64,
    effort: Option<&str>,
) -> Result<(String, Option<String>)> {
    let cli = Cli::resolve(Some(platform_name)).map_err(|e| anyhow::anyhow!(e))?;
    let strategy = cli.strategy();
    let effort_not_applied = resolve_effort_for_spawn(&strategy, platform_name, effort)?;

    let home =
        dirs::home_dir().ok_or_else(|| anyhow::anyhow!("Could not determine home directory"))?;

    let platform = find_platform(&cli)?;

    let server_refs: Vec<&str> = mcp_servers.iter().map(|s| s.as_str()).collect();
    let synthesized = synthesize_mcp_config(&platform, &home, &server_refs)?;

    let run_id = uuid::Uuid::new_v4().to_string();
    let now = Utc::now();
    let started_at = now.to_rfc3339();
    let expires_at = (now + chrono::Duration::minutes(ttl_minutes as i64)).to_rfc3339();

    db.insert_subagent_run(
        &run_id,
        platform_name,
        model,
        prompt,
        workdir,
        &started_at,
        &expires_at,
    )?;

    let has_template = strategy.invocation_template.is_some();
    let mcp_config_path_str = synthesized.path.to_string_lossy().to_string();
    let mcp_config_arg = if has_template {
        Some(mcp_config_path_str.as_str())
    } else {
        None
    };

    let depth_limited_prompt = format_subagent_prompt(prompt);

    let mut command = strategy.build_command_with_mcp_config(
        &depth_limited_prompt,
        model,
        Some(workdir),
        mcp_config_arg,
        effort,
    )?;

    command.stdout(std::process::Stdio::piped());
    command.stderr(std::process::Stdio::piped());

    let db_clone = Arc::clone(db);
    let run_id_clone = run_id.clone();
    let config_path = synthesized.path.clone();
    let mcp_surface = if has_template {
        synthesized.mcp_surface
    } else {
        "(full — no invocation template to inject filtered config)".to_string()
    };
    let no_template_warning = !has_template;

    let child = command.spawn()?;
    let pid = child.id().unwrap_or(0);
    let boot_id = crate::system::boot_id();
    db.set_subagent_run_pid(&run_id, pid as i64, boot_id.as_deref())?;

    tokio::spawn(async move {
        let timeout_result = tokio::time::timeout(
            std::time::Duration::from_secs(timeout_minutes * 60),
            child.wait_with_output(),
        )
        .await;

        let finished_at = Utc::now().to_rfc3339();
        let warn_prefix = if no_template_warning {
            "WARNING: no invocation_template — subagent saw full MCP surface\n"
        } else {
            ""
        };

        match timeout_result {
            Ok(Ok(output)) => {
                let exit_code = output.status.code().unwrap_or(-1);
                let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
                let raw_stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
                let stderr = format!("{warn_prefix}{raw_stderr}");
                if output.status.success() {
                    let _ = db_clone.complete_subagent_run(
                        &run_id_clone,
                        exit_code,
                        &stdout,
                        &stderr,
                        Some(&mcp_surface),
                        &finished_at,
                    );
                } else {
                    let _ = db_clone.fail_subagent_run(
                        &run_id_clone,
                        &stderr,
                        Some(&mcp_surface),
                        &finished_at,
                    );
                }
            }
            Ok(Err(e)) => {
                let _ = db_clone.fail_subagent_run(
                    &run_id_clone,
                    &format!("{warn_prefix}Spawn error: {e}"),
                    Some(&mcp_surface),
                    &finished_at,
                );
            }
            Err(_elapsed) => {
                if pid > 0 {
                    process::terminate_process_group_async(
                        pid as i64,
                        std::time::Duration::from_secs(5),
                    );
                }
                let _ = db_clone.fail_subagent_run(
                    &run_id_clone,
                    &format!("{warn_prefix}Timed out"),
                    Some(&mcp_surface),
                    &finished_at,
                );
            }
        }

        let _ = std::fs::remove_file(&config_path);
    });

    Ok((run_id, effort_not_applied))
}

pub fn collect_subagent(db: &Arc<Database>, id: &str) -> Result<Option<SubagentResult>> {
    let record = db.collect_subagent_run(id)?;
    Ok(record.map(SubagentResult::from_record))
}

fn find_platform(cli: &Cli) -> Result<Platform> {
    let registry = crate::setup_module::registry_fetch::fetch_registry()
        .map_err(|e| anyhow::anyhow!("Failed to load platform registry: {e}"))?;
    registry
        .platforms
        .into_iter()
        .find(|p| p.name == cli.as_str())
        .ok_or_else(|| anyhow::anyhow!("Platform '{}' not found in registry", cli.as_str()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::cli_config::EffortDeclaration;
    use crate::domain::cli_strategy::CliStrategy;
    use std::collections::HashMap;

    fn strategy_with_declaration(decl: Option<EffortDeclaration>) -> CliStrategy {
        CliStrategy {
            binary: "/usr/local/bin/test-cli".to_string(),
            headless_mode: String::new(),
            model_flag: None,
            supports_working_dir: false,
            working_dir_flag: None,
            env_vars: HashMap::new(),
            prompt_via_stdin: false,
            session_id_set_flag: None,
            session_list_cmd: None,
            session_list_format_args: None,
            session_id_pattern: None,
            session_resume_cmd: None,
            trust_flag: None,
            invocation_template: None,
            effort_declaration: decl,
        }
    }

    #[test]
    fn resolve_effort_for_spawn_none_requested_is_noop() {
        let strategy = strategy_with_declaration(None);
        assert_eq!(
            resolve_effort_for_spawn(&strategy, "claude", None).unwrap(),
            None
        );
    }

    #[test]
    fn resolve_effort_for_spawn_unsupported_platform_spawns_with_notice() {
        let strategy = strategy_with_declaration(None);
        let notice = resolve_effort_for_spawn(&strategy, "opencode", Some("high")).unwrap();
        assert!(notice.unwrap().contains("does not support effort"));
    }

    #[test]
    fn resolve_effort_for_spawn_empty_values_spawns_with_notice() {
        let strategy = strategy_with_declaration(Some(EffortDeclaration {
            form: Some(String::new()),
            values: vec![],
        }));
        let notice = resolve_effort_for_spawn(&strategy, "cline", Some("high")).unwrap();
        assert!(notice.unwrap().contains("does not support effort"));
    }

    #[test]
    fn resolve_effort_for_spawn_accepted_value_applies_cleanly() {
        let strategy = strategy_with_declaration(Some(EffortDeclaration {
            form: Some("--effort".to_string()),
            values: vec!["low".to_string(), "medium".to_string(), "high".to_string()],
        }));
        assert_eq!(
            resolve_effort_for_spawn(&strategy, "claude", Some("high")).unwrap(),
            None
        );
    }

    #[test]
    fn resolve_effort_for_spawn_rejected_value_is_refused_before_spawn() {
        let strategy = strategy_with_declaration(Some(EffortDeclaration {
            form: Some("--effort".to_string()),
            values: vec!["low".to_string(), "medium".to_string(), "high".to_string()],
        }));
        let err = resolve_effort_for_spawn(&strategy, "claude", Some("ultra")).unwrap_err();
        assert!(err
            .to_string()
            .contains("not in platform's accepted values"));
    }
}
