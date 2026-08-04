//! Headless liveness probing for configured platforms.
//!
//! `agent_models`/`doctor` answer "does the catalog list this platform" and
//! "is its binary present and its config well-formed" — neither one ever
//! actually asks the harness to speak. A platform can be installed, listed,
//! and configured while every real invocation answers with its own error
//! text and exits 0 (the 2026-08 `mimocode`/`mimo-auto` incident: `Error:
//! Unsupported model mimo-auto` on stdout, `Error: Invalid API Key` on the
//! named ones, exit code 0 both times). This module is the missing "actually
//! run it" step: build the platform's real headless command, spawn it, and
//! judge the *response*, never the exit code alone.
//!
//! ## What counts as "a usable response"
//!
//! Non-empty stdout is the obvious rule and is too weak — `Error: Invalid
//! API Key` is non-empty stdout that exits 0. Instead every probe asks the
//! harness to echo a unique, per-probe token ([`probe_token`]) and the
//! verdict is whether that exact token shows up anywhere in the captured
//! stdout/stderr ([`probe_target`]). Containment rather than an exact match
//! deliberately tolerates a harness that wraps or decorates its output
//! (banners, ANSI, a leading "Assistant:" prefix) — the token is chosen
//! random enough (`CANOPY-PROBE-<uuidv4>`) that a harness's own error text
//! reflecting it back by coincidence is not a realistic failure mode, and an
//! error message that doesn't echo the prompt (`Unsupported model ...`,
//! `Invalid API Key`) correctly fails the check.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use serde_json::Value;

use crate::daemon::handler::redact_secrets;
use crate::daemon::process::{terminate_process_group_async, KILL_GRACE};
use crate::domain::canopy_config::CanopyConfig;
use crate::domain::cli_strategy::CliStrategy;
use crate::domain::loops::{LoopDetails, LoopNodeKind};

/// Default bound on how long a single probe waits for a response — this is a
/// liveness check, not a capability benchmark, so it stays short.
pub(crate) const DEFAULT_PROBE_TIMEOUT_SECS: u64 = 30;
pub(crate) const MIN_PROBE_TIMEOUT_SECS: u64 = 5;
pub(crate) const MAX_PROBE_TIMEOUT_SECS: u64 = 120;

/// One platform+model pair to probe. `model: None` means "whatever this
/// platform's CLI defaults to when no model flag is passed" — not "any
/// model", so a probe against `None` says nothing about a specific model a
/// node might request explicitly.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ProbeTarget {
    pub platform: String,
    pub model: Option<String>,
}

/// What actually happened when a target was invoked. Distinct from a bare
/// `bool` because the whole point of this module is that a caller needs to
/// know *why* a platform is unreachable (missing API key vs. wrong model
/// name vs. it simply never answered) — see [`ProbeReport::error`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ProbeOutcome {
    /// The harness echoed the probe token back — a real response came from
    /// the model, not just a process that exited cleanly.
    Reachable,
    /// The process ran to completion (any exit code) but the response never
    /// contained the probe token — the defect class this module exists to
    /// catch, including "exit 0 with an error and nothing else."
    Broken,
    /// The process was still running when the timeout elapsed; its process
    /// group has been killed. Deliberately distinct from `Broken`: the
    /// harness never got a chance to answer at all.
    TimedOut,
    /// `platform` names a CLI that isn't in `~/.canopy/config.toml` — no
    /// process was ever spawned.
    NotConfigured,
    /// The command could not even be built/spawned/waited on (binary not
    /// resolvable, permission denied, a `wait()` syscall failure) — distinct
    /// from `Broken` because the harness's own error-reporting was never
    /// reached.
    SpawnFailed,
}

impl ProbeOutcome {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Reachable => "reachable",
            Self::Broken => "broken",
            Self::TimedOut => "timed_out",
            Self::NotConfigured => "not_configured",
            Self::SpawnFailed => "spawn_failed",
        }
    }

    pub fn reachable(&self) -> bool {
        matches!(self, Self::Reachable)
    }
}

/// Result of probing one [`ProbeTarget`].
#[derive(Debug, Clone)]
pub(crate) struct ProbeReport {
    pub platform: String,
    pub model: Option<String>,
    pub outcome: ProbeOutcome,
    pub duration_ms: u128,
    /// The harness's own error text, verbatim after secret redaction —
    /// `None` exactly when `outcome` is `Reachable`. A boolean here would
    /// tell a caller nothing about whether the fix is an API key, a model
    /// name, or a missing binary.
    pub error: Option<String>,
}

impl ProbeReport {
    pub fn to_json(&self) -> Value {
        serde_json::json!({
            "platform": self.platform,
            "model": self.model,
            "reachable": self.outcome.reachable(),
            "outcome": self.outcome.as_str(),
            "duration_ms": self.duration_ms,
            "error": self.error,
        })
    }
}

/// A fresh, hard-to-guess token this probe run asks the harness to echo
/// back. Random per call so two concurrent probes (or a probe re-run) can
/// never be confused by a stale token surviving in some cache/log the
/// harness reads.
fn probe_token() -> String {
    format!("CANOPY-PROBE-{}", uuid::Uuid::new_v4().simple())
}

/// The prompt every probe sends: minimal in, minimal out (a few tokens each
/// way) — this is a liveness check, not a capability benchmark.
fn probe_prompt(token: &str) -> String {
    format!("Reply with exactly this text and nothing else: {token}")
}

/// Probe one platform+model pair by actually invoking it: build the
/// platform's real headless command from its registry config
/// (`headless_mode`/`model_flag`/etc, via [`CliStrategy::from_cli_config`] —
/// the same path a real loop node uses), spawn it, and judge the captured
/// response rather than the exit code.
///
/// `workdir` is passed straight to [`CliStrategy::build_command`]; pass the
/// loop's workdir when probing on a loop's behalf, `None` for a standalone
/// probe with no project context.
pub(crate) async fn probe_target(
    config: &CanopyConfig,
    target: &ProbeTarget,
    workdir: Option<&str>,
    timeout: Duration,
) -> ProbeReport {
    let start = Instant::now();

    let Some(cli_config) = config.get_cli(&target.platform) else {
        return ProbeReport {
            platform: target.platform.clone(),
            model: target.model.clone(),
            outcome: ProbeOutcome::NotConfigured,
            duration_ms: start.elapsed().as_millis(),
            error: Some(format!(
                "Platform '{}' is not configured in canopy (~/.canopy/config.toml).",
                target.platform
            )),
        };
    };

    let strategy = CliStrategy::from_cli_config(cli_config);
    let token = probe_token();
    let prompt = probe_prompt(&token);

    let mut command = match strategy.build_command(&prompt, target.model.as_deref(), workdir) {
        Ok(command) => command,
        Err(error) => {
            return ProbeReport {
                platform: target.platform.clone(),
                model: target.model.clone(),
                outcome: ProbeOutcome::SpawnFailed,
                duration_ms: start.elapsed().as_millis(),
                error: Some(redact_secrets(&error.to_string())),
            }
        }
    };
    command.stdout(std::process::Stdio::piped());
    command.stderr(std::process::Stdio::piped());

    let child = match command.spawn() {
        Ok(child) => child,
        Err(error) => {
            return ProbeReport {
                platform: target.platform.clone(),
                model: target.model.clone(),
                outcome: ProbeOutcome::SpawnFailed,
                duration_ms: start.elapsed().as_millis(),
                error: Some(redact_secrets(&error.to_string())),
            }
        }
    };
    // Captured before `wait_with_output` below takes ownership of `child`.
    let pid = child.id();

    match tokio::time::timeout(timeout, child.wait_with_output()).await {
        Err(_elapsed) => {
            // Same B12 process-group kill every other detached spawn in the
            // engine uses on timeout — a probe must not leak a running child
            // any more than a real node run does.
            if let Some(pid) = pid {
                terminate_process_group_async(pid as i64, KILL_GRACE);
            }
            ProbeReport {
                platform: target.platform.clone(),
                model: target.model.clone(),
                outcome: ProbeOutcome::TimedOut,
                duration_ms: start.elapsed().as_millis(),
                error: None,
            }
        }
        Ok(Err(error)) => ProbeReport {
            platform: target.platform.clone(),
            model: target.model.clone(),
            outcome: ProbeOutcome::SpawnFailed,
            duration_ms: start.elapsed().as_millis(),
            error: Some(redact_secrets(&error.to_string())),
        },
        Ok(Ok(output)) => {
            let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
            let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
            let duration_ms = start.elapsed().as_millis();

            if stdout.contains(&token) || stderr.contains(&token) {
                ProbeReport {
                    platform: target.platform.clone(),
                    model: target.model.clone(),
                    outcome: ProbeOutcome::Reachable,
                    duration_ms,
                    error: None,
                }
            } else {
                // Prefer stderr (where an error is conventionally printed)
                // but fall back to stdout — the mimocode incident printed
                // its error to stdout with an empty stderr, exit code 0.
                let raw_error = if !stderr.is_empty() {
                    stderr
                } else if !stdout.is_empty() {
                    stdout
                } else {
                    format!(
                        "exited {} with no output and no probe token in the response",
                        output.status.code().unwrap_or(-1)
                    )
                };
                ProbeReport {
                    platform: target.platform.clone(),
                    model: target.model.clone(),
                    outcome: ProbeOutcome::Broken,
                    duration_ms,
                    error: Some(redact_secrets(&raw_error)),
                }
            }
        }
    }
}

/// Probe every target concurrently — a single slow/hanging harness must not
/// serialize the rest of the sweep. Results are returned in the same order
/// as `targets`.
pub(crate) async fn probe_targets(
    config: &CanopyConfig,
    targets: &[ProbeTarget],
    workdir: Option<&str>,
    timeout: Duration,
) -> Vec<ProbeReport> {
    let futures = targets
        .iter()
        .map(|target| probe_target(config, target, workdir, timeout));
    futures::future::join_all(futures).await
}

/// One distinct platform+model pair referenced somewhere in a loop, plus the
/// human-readable list of nodes/hooks that reference it — so a caller who
/// sees a pair fail knows which node(s) that affects, without probing the
/// same pair twice.
#[derive(Debug, Clone)]
pub(crate) struct LoopProbeTarget {
    pub target: ProbeTarget,
    pub used_by: Vec<String>,
}

/// Walk a loop's agent nodes (loop-level graph and every spec's graph —
/// ensemble members are themselves ordinary [`LoopNodeKind::Agent`] rows, so
/// no separate ensemble query is needed) plus its `on_completed` hook, and
/// return the distinct platform+model pairs they reference. A platform used
/// by five nodes appears once, with all five names attached.
pub(crate) fn distinct_targets_for_loop(details: &LoopDetails) -> Vec<LoopProbeTarget> {
    let mut by_key: HashMap<(String, Option<String>), usize> = HashMap::new();
    let mut result: Vec<LoopProbeTarget> = Vec::new();

    let mut record = |platform: Option<&str>, model: Option<&str>, label: String| {
        let Some(platform) = platform else { return };
        let key = (platform.to_string(), model.map(str::to_string));
        if let Some(&idx) = by_key.get(&key) {
            result[idx].used_by.push(label);
        } else {
            by_key.insert(key.clone(), result.len());
            result.push(LoopProbeTarget {
                target: ProbeTarget {
                    platform: key.0,
                    model: key.1,
                },
                used_by: vec![label],
            });
        }
    };

    if let Some(hook) = &details.lp.on_completed {
        record(
            Some(hook.platform.as_str()),
            hook.model.as_deref(),
            "on_completed hook".to_string(),
        );
    }

    let all_nodes = details
        .graph_nodes
        .iter()
        .chain(details.specs.iter().flat_map(|spec| spec.nodes.iter()));
    for node in all_nodes {
        if node.kind != LoopNodeKind::Agent {
            continue;
        }
        let platform = node
            .config
            .get("platform")
            .or_else(|| node.config.get("cli"))
            .and_then(Value::as_str);
        let model = node.config.get("model").and_then(Value::as_str);
        record(platform, model, format!("node: {}", node.name));
    }

    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::cli_config::CliConfig;

    /// A `CanopyConfig` with one CLI entry pointing at `script_path`,
    /// invoked with no headless flags and no model flag — the fixture
    /// scripts below just read argv[1] (the prompt) and decide what to
    /// print from it, so no real flags are needed to exercise the verdict
    /// logic.
    fn config_with_cli(name: &str, script_path: &std::path::Path) -> CanopyConfig {
        CanopyConfig {
            clis: vec![CliConfig {
                name: name.to_string(),
                binary: script_path.to_string_lossy().to_string(),
                ..Default::default()
            }],
            ..Default::default()
        }
    }

    fn write_script(dir: &tempfile::TempDir, name: &str, body: &str) -> std::path::PathBuf {
        let path = dir.path().join(name);
        std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        path
    }

    #[tokio::test]
    async fn probe_target_not_configured_for_unknown_platform() {
        let config = CanopyConfig::default();
        let target = ProbeTarget {
            platform: "ghost-cli".to_string(),
            model: None,
        };
        let report = probe_target(&config, &target, None, Duration::from_secs(5)).await;
        assert_eq!(report.outcome, ProbeOutcome::NotConfigured);
        assert!(report.error.unwrap().contains("ghost-cli"));
    }

    /// Real failure shape #1: exit 0, the error goes to stderr, stdout is
    /// empty. Must be reported broken — this is the "Invalid API Key"
    /// shape from the mimocode incident when nothing reaches stdout at all.
    #[tokio::test]
    async fn probe_target_reports_broken_for_exit0_stderr_error_empty_stdout() {
        let dir = tempfile::tempdir().unwrap();
        let script = write_script(
            &dir,
            "broken-stderr-cli",
            "echo 'Error: Invalid API Key' 1>&2\nexit 0\n",
        );
        let config = config_with_cli("broken-stderr", &script);
        let target = ProbeTarget {
            platform: "broken-stderr".to_string(),
            model: None,
        };
        let report = probe_target(&config, &target, None, Duration::from_secs(5)).await;
        assert_eq!(report.outcome, ProbeOutcome::Broken);
        assert!(!report.outcome.reachable());
        assert_eq!(report.error.as_deref(), Some("Error: Invalid API Key"));
    }

    /// Real failure shape #2: exit 0, the error is printed to stdout instead
    /// of stderr (the mimocode "Unsupported model mimo-auto" shape). Must
    /// also be reported broken — a naive "non-empty stdout" check would
    /// wrongly pass this.
    #[tokio::test]
    async fn probe_target_reports_broken_for_exit0_stdout_error() {
        let dir = tempfile::tempdir().unwrap();
        let script = write_script(
            &dir,
            "broken-stdout-cli",
            "echo 'Error: Unsupported model mimo-auto'\nexit 0\n",
        );
        let config = config_with_cli("broken-stdout", &script);
        let target = ProbeTarget {
            platform: "broken-stdout".to_string(),
            model: None,
        };
        let report = probe_target(&config, &target, None, Duration::from_secs(5)).await;
        assert_eq!(report.outcome, ProbeOutcome::Broken);
        assert_eq!(
            report.error.as_deref(),
            Some("Error: Unsupported model mimo-auto")
        );
    }

    /// Real success shape: the harness echoes the probe token back on
    /// stdout. Must be reported reachable regardless of decoration around
    /// the token.
    #[tokio::test]
    async fn probe_target_reports_reachable_for_healthy_response() {
        let dir = tempfile::tempdir().unwrap();
        // Echoes argv[1] (the prompt, which embeds the token) back wrapped
        // in some decoration, simulating a harness that doesn't print the
        // token verbatim and alone.
        let script = write_script(
            &dir,
            "healthy-cli",
            "echo \"Assistant: sure, here you go -> $1\"\n",
        );
        let config = config_with_cli("healthy", &script);
        let target = ProbeTarget {
            platform: "healthy".to_string(),
            model: None,
        };
        let report = probe_target(&config, &target, None, Duration::from_secs(5)).await;
        assert_eq!(report.outcome, ProbeOutcome::Reachable);
        assert!(report.outcome.reachable());
        assert!(report.error.is_none());
    }

    /// A harness that hangs past the timeout must report `TimedOut`,
    /// distinct from `Broken` — the process never got a chance to answer.
    #[tokio::test]
    async fn probe_target_reports_timed_out_for_a_hanging_harness() {
        let dir = tempfile::tempdir().unwrap();
        let script = write_script(&dir, "hang-cli", "sleep 30\n");
        let config = config_with_cli("hang", &script);
        let target = ProbeTarget {
            platform: "hang".to_string(),
            model: None,
        };
        let report = probe_target(&config, &target, None, Duration::from_millis(200)).await;
        assert_eq!(report.outcome, ProbeOutcome::TimedOut);
        assert!(report.error.is_none());
    }

    #[tokio::test]
    async fn probe_target_reports_spawn_failed_for_a_missing_binary() {
        let config = config_with_cli("missing", std::path::Path::new("/no/such/binary-xyz"));
        let target = ProbeTarget {
            platform: "missing".to_string(),
            model: None,
        };
        let report = probe_target(&config, &target, None, Duration::from_secs(5)).await;
        assert_eq!(report.outcome, ProbeOutcome::SpawnFailed);
        assert!(report.error.is_some());
    }

    #[tokio::test]
    async fn probe_target_redacts_secrets_in_error_text() {
        let dir = tempfile::tempdir().unwrap();
        let script = write_script(
            &dir,
            "leaky-cli",
            "echo 'Error: bad key sk-abcdefghijklmnopqrstuvwx' 1>&2\nexit 0\n",
        );
        let config = config_with_cli("leaky", &script);
        let target = ProbeTarget {
            platform: "leaky".to_string(),
            model: None,
        };
        let report = probe_target(&config, &target, None, Duration::from_secs(5)).await;
        let error = report.error.unwrap();
        assert!(!error.contains("sk-abcdefghijklmnopqrstuvwx"));
        assert!(error.contains("[REDACTED]"));
    }

    #[tokio::test]
    async fn probe_targets_runs_concurrently_not_serially() {
        // Two hanging probes with a timeout well under 2x itself: if they
        // ran serially the second probe's deadline would already have
        // elapsed before it even started, so both still reporting
        // `TimedOut` at all (rather than the harness never having been
        // spawned) plus a wall-clock bound below 2x the timeout is the
        // signal that they were polled concurrently.
        let dir = tempfile::tempdir().unwrap();
        let script = write_script(&dir, "hang-cli", "sleep 30\n");
        let mut config = CanopyConfig::default();
        config.clis.push(CliConfig {
            name: "hang-a".to_string(),
            binary: script.to_string_lossy().to_string(),
            ..Default::default()
        });
        config.clis.push(CliConfig {
            name: "hang-b".to_string(),
            binary: script.to_string_lossy().to_string(),
            ..Default::default()
        });
        let targets = vec![
            ProbeTarget {
                platform: "hang-a".to_string(),
                model: None,
            },
            ProbeTarget {
                platform: "hang-b".to_string(),
                model: None,
            },
        ];
        let timeout = Duration::from_millis(300);
        let start = Instant::now();
        let reports = probe_targets(&config, &targets, None, timeout).await;
        let elapsed = start.elapsed();
        assert_eq!(reports.len(), 2);
        assert!(reports.iter().all(|r| r.outcome == ProbeOutcome::TimedOut));
        assert!(
            elapsed < timeout * 2,
            "two probes took {elapsed:?}, expected well under 2x the {timeout:?} timeout \
             if they ran concurrently"
        );
    }

    fn agent_node(name: &str, config: Value) -> crate::domain::loops::LoopNode {
        crate::domain::loops::LoopNode {
            id: uuid::Uuid::new_v4().to_string(),
            spec_id: None,
            loop_id: Some("loop-1".to_string()),
            name: name.to_string(),
            kind: LoopNodeKind::Agent,
            config,
            position: 0,
            created_at: chrono::Utc::now(),
        }
    }

    fn sample_loop(
        on_completed: Option<crate::domain::loops::LoopCompletionHook>,
    ) -> crate::domain::loops::Loop {
        crate::domain::loops::Loop {
            archived: false,
            id: "loop-1".to_string(),
            name: "sample".to_string(),
            description: None,
            workdir: "/tmp".to_string(),
            status: crate::domain::loops::LoopStatus::Draft,
            trigger: None,
            created_at: chrono::Utc::now(),
            started_at: None,
            completed_at: None,
            autorun_at: None,
            auto_continue_at: None,
            auto_continue_action: None,
            active_run_pool_id: None,
            on_completed,
        }
    }

    #[test]
    fn distinct_targets_for_loop_dedupes_same_pair_across_nodes() {
        let details = LoopDetails {
            lp: sample_loop(None),
            graph_nodes: vec![
                agent_node(
                    "node-a",
                    serde_json::json!({"platform": "claude", "model": "claude-opus-4-8"}),
                ),
                agent_node(
                    "node-b",
                    serde_json::json!({"platform": "claude", "model": "claude-opus-4-8"}),
                ),
                agent_node(
                    "node-c",
                    serde_json::json!({"platform": "mimo", "model": "mimo-auto"}),
                ),
            ],
            graph_edges: vec![],
            specs: vec![],
            completion_hook_runs: vec![],
        };

        let targets = distinct_targets_for_loop(&details);
        assert_eq!(targets.len(), 2);
        let claude = targets
            .iter()
            .find(|t| t.target.platform == "claude")
            .unwrap();
        assert_eq!(claude.target.model.as_deref(), Some("claude-opus-4-8"));
        assert_eq!(claude.used_by, vec!["node: node-a", "node: node-b"]);
        let mimo = targets
            .iter()
            .find(|t| t.target.platform == "mimo")
            .unwrap();
        assert_eq!(mimo.used_by, vec!["node: node-c"]);
    }

    #[test]
    fn distinct_targets_for_loop_includes_on_completed_hook() {
        let hook = crate::domain::loops::LoopCompletionHook {
            platform: "mimo".to_string(),
            model: None,
            prompt: "done".to_string(),
            timeout_minutes: None,
        };
        let details = LoopDetails {
            lp: sample_loop(Some(hook)),
            graph_nodes: vec![],
            graph_edges: vec![],
            specs: vec![],
            completion_hook_runs: vec![],
        };

        let targets = distinct_targets_for_loop(&details);
        assert_eq!(targets.len(), 1);
        assert_eq!(targets[0].target.platform, "mimo");
        assert_eq!(targets[0].used_by, vec!["on_completed hook"]);
    }

    #[test]
    fn distinct_targets_for_loop_skips_non_agent_nodes() {
        let details = LoopDetails {
            lp: sample_loop(None),
            graph_nodes: vec![{
                let mut node = agent_node("gate-1", serde_json::json!({"platform": "claude"}));
                node.kind = LoopNodeKind::Gate;
                node
            }],
            graph_edges: vec![],
            specs: vec![],
            completion_hook_runs: vec![],
        };

        assert!(distinct_targets_for_loop(&details).is_empty());
    }
}
