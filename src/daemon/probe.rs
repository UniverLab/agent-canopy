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
    pub effort: Option<String>,
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
    /// A `model` was requested but this platform's CLI has no way to select
    /// one explicitly (no `model_flag` configured) — the 2026-08-13
    /// `mistral` case, whose CLI addresses named agents, not models, so
    /// `agent_models` output is never valid input for it. No process was
    /// spawned: probing "whatever the bare CLI defaults to" would validate
    /// a different pair than the one asked about and silently pass, which
    /// is the exact defect this variant exists to prevent. Never
    /// `reachable` — a caller that treated this as passing would repeat
    /// the 2026-08-13 incident.
    Unknown,
    /// The harness accepted the request and answered, but its own response
    /// carries a "requested model not recognized, using a different one"
    /// warning (see [`detect_model_substitution`]) — the `gpt-5.6` case:
    /// the probe token can still show up because the *substituted* model
    /// answered it, not the one that was actually requested. Reporting
    /// this `Reachable` would validate a pair a real node never actually
    /// runs.
    Substituted,
}

impl ProbeOutcome {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Reachable => "reachable",
            Self::Broken => "broken",
            Self::TimedOut => "timed_out",
            Self::NotConfigured => "not_configured",
            Self::SpawnFailed => "spawn_failed",
            Self::Unknown => "unknown",
            Self::Substituted => "substituted",
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

/// Marker substring of the harness's own "requested model not recognized,
/// substituting a different one" warning — the 2026-08-13 incident's
/// `Model metadata for 'gpt-5.6' not found. Defaulting to fallback
/// metadata`. Matched case-insensitively and on the "defaulting to
/// fallback metadata" half only, since that half is the actual signal (the
/// model name/quoting around "not found" is free-form harness text not
/// worth pinning exactly).
const MODEL_SUBSTITUTION_MARKER: &str = "defaulting to fallback metadata";

/// Find the line (if any) in the captured response that shows the harness
/// silently answered with a different model than the one requested. Checked
/// against the *raw* stdout/stderr, independent of whether the probe token
/// is present — the whole defect this catches is a response that both
/// contains the token (a real, usable-looking answer) and this warning (the
/// answer came from a model nobody asked for).
fn detect_model_substitution(stdout: &str, stderr: &str) -> Option<String> {
    for text in [stderr, stdout] {
        if let Some(line) = text
            .lines()
            .find(|line| line.to_lowercase().contains(MODEL_SUBSTITUTION_MARKER))
        {
            return Some(line.to_string());
        }
    }
    None
}

/// Substrings that indicate the requested model was rejected outright — the
/// 2026-08-13 `gpt-5.6` case where Codex returned `400 invalid_request_error
/// — "The 'gpt-5.6' model is not supported when using Codex with a ChatGPT
/// account."` Matched case-insensitively and per-line so the returned error
/// is the exact harness line, like [`detect_model_substitution`]. Only checked
/// when a specific `model` was requested; otherwise a platform mention of
/// "model" in help text would false-positive.
fn detect_model_rejection(stdout: &str, stderr: &str) -> Option<String> {
    const MARKERS: &[&str] = &[
        "model is not supported",
        "not supported when using",
        "model not available",
    ];
    for text in [stderr, stdout] {
        for line in text.lines() {
            let lower = line.to_lowercase();
            if MARKERS.iter().any(|m| lower.contains(m)) {
                return Some(line.to_string());
            }
            // `invalid_request_error` + `model` in the same line covers the raw
            // JSON body variant: `{"error":{"type":"invalid_request_error","message":"The 'gpt-5.6' model ..."}}`
            // without pinning the full JSON shape. Anchored on `invalid_request`
            // rather than a bare `invalid` so an unrelated line that merely
            // mentions "invalid" and "model" on a healthy probe can't be
            // misread as a rejection.
            if lower.contains("invalid_request") && lower.contains("model") {
                return Some(line.to_string());
            }
        }
    }
    None
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

    // A model was requested but this platform's CLI has no flag to select
    // one — `CliStrategy::build_command` would silently drop it and probe
    // the bare default instead, which is exactly how the 2026-08-13
    // `mistral` pair was wrongly reported `reachable`: the probe validated
    // a different pair than the one a real node run would use. Report
    // `Unknown` without spawning rather than pretend the specific model was
    // exercised.
    if let Some(model) = target.model.as_deref() {
        if strategy.model_flag.is_none() {
            return ProbeReport {
                platform: target.platform.clone(),
                model: target.model.clone(),
                outcome: ProbeOutcome::Unknown,
                duration_ms: start.elapsed().as_millis(),
                error: Some(format!(
                    "Platform '{}' has no configured way to select a model explicitly \
                     (no model_flag) — cannot validate model '{}' end-to-end. Omit `model` \
                     to probe the platform's own default instead.",
                    target.platform, model
                )),
            };
        }
    }

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

            // Checked before the token match: a substituted-model response
            // can still contain the probe token (the fallback model
            // answered it), which would otherwise be reported `Reachable`
            // for a model nobody actually validated — the 2026-08-13
            // `gpt-5.6` case.
            if target.model.is_some() {
                if let Some(warning) = detect_model_substitution(&stdout, &stderr) {
                    return ProbeReport {
                        platform: target.platform.clone(),
                        model: target.model.clone(),
                        outcome: ProbeOutcome::Substituted,
                        duration_ms,
                        error: Some(redact_secrets(&warning)),
                    };
                }
                if let Some(rejection) = detect_model_rejection(&stdout, &stderr) {
                    return ProbeReport {
                        platform: target.platform.clone(),
                        model: target.model.clone(),
                        outcome: ProbeOutcome::Broken,
                        duration_ms,
                        error: Some(redact_secrets(&rejection)),
                    };
                }
            }

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

/// Count of reports that are *known* to fail a real run — every outcome
/// except `Reachable` and `Unknown`. Deliberately excludes `Unknown`: a pair
/// whose validity could not be determined is not a confirmed failure, and
/// folding it in here would make `would_fail` claim more certainty than the
/// probe actually has. Shared by `agent_probe` and `loop_preflight` so the
/// two tools can never disagree about what counts as "would fail".
pub(crate) fn would_fail_count(reports: &[ProbeReport]) -> usize {
    reports
        .iter()
        .filter(|r| !r.outcome.reachable() && r.outcome != ProbeOutcome::Unknown)
        .count()
}

/// Count of reports whose validity could not be determined at all (see
/// [`ProbeOutcome::Unknown`]). Reported alongside `would_fail` rather than
/// folded into it — an unknown pair is not a confirmed pass (excluded from
/// `would_fail`'s complement) and not a confirmed failure (excluded from
/// `would_fail` itself), so it needs its own count to stay honest about
/// which pairs were actually validated.
pub(crate) fn unknown_count(reports: &[ProbeReport]) -> usize {
    reports
        .iter()
        .filter(|r| r.outcome == ProbeOutcome::Unknown)
        .count()
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
/// no separate ensemble query is needed) plus every hook in every event, and
/// return the distinct platform+model pairs they reference. A platform used
/// by five nodes appears once, with all five names attached.
pub(crate) fn distinct_targets_for_loop(details: &LoopDetails) -> Vec<LoopProbeTarget> {
    let mut by_key: HashMap<(String, Option<String>, Option<String>), usize> = HashMap::new();
    let mut result: Vec<LoopProbeTarget> = Vec::new();

    let mut record =
        |platform: Option<&str>, model: Option<&str>, effort: Option<&str>, label: String| {
            let Some(platform) = platform else { return };
            let key = (
                platform.to_string(),
                model.map(str::to_string),
                effort.map(str::to_string),
            );
            if let Some(&idx) = by_key.get(&key) {
                result[idx].used_by.push(label);
            } else {
                by_key.insert(key.clone(), result.len());
                result.push(LoopProbeTarget {
                    target: ProbeTarget {
                        platform: key.0,
                        model: key.1,
                        effort: key.2,
                    },
                    used_by: vec![label],
                });
            }
        };

    for (event, hooks) in &details.lp.hooks {
        for (idx, hook) in hooks.iter().enumerate() {
            // Command hooks have no platform — they are not agent targets.
            record(
                hook.platform.as_deref(),
                hook.model.as_deref(),
                hook.effort.as_deref(),
                format!("{} hook {}", event.as_str(), idx),
            );
        }
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
        let effort = node.config.get("effort").and_then(Value::as_str);
        record(platform, model, effort, format!("node: {}", node.name));
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

    /// Same as [`config_with_cli`] but with `--model` configured as the
    /// model flag — for exercising the `platform`+`model` path the way a
    /// real loop node (e.g. codex) does, as opposed to platforms like
    /// `mistral` that have no model flag at all.
    fn config_with_cli_and_model_flag(name: &str, script_path: &std::path::Path) -> CanopyConfig {
        CanopyConfig {
            clis: vec![CliConfig {
                name: name.to_string(),
                binary: script_path.to_string_lossy().to_string(),
                model_flag: Some("--model".to_string()),
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
            effort: None,
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
            effort: None,
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
            effort: None,
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
            effort: None,
        };
        let report = probe_target(&config, &target, None, Duration::from_secs(5)).await;
        assert_eq!(report.outcome, ProbeOutcome::Reachable);
        assert!(report.outcome.reachable());
        assert!(report.error.is_none());
    }

    /// No-regression check for the healthy path with a *specific* model
    /// requested against a platform that actually has a model flag
    /// configured (unlike the `mistral` case below): a clean answer
    /// containing the probe token must still be `Reachable`.
    #[tokio::test]
    async fn probe_target_reports_reachable_for_healthy_response_with_model() {
        let dir = tempfile::tempdir().unwrap();
        let script = write_script(&dir, "healthy-model-cli", "echo \"ok: $1\"\n");
        let config = config_with_cli_and_model_flag("healthy-model", &script);
        let target = ProbeTarget {
            platform: "healthy-model".to_string(),
            model: Some("some-model".to_string()),
            effort: None,
        };
        let report = probe_target(&config, &target, None, Duration::from_secs(5)).await;
        assert_eq!(report.outcome, ProbeOutcome::Reachable);
        assert!(report.outcome.reachable());
        assert!(report.error.is_none());
    }

    /// The 2026-08-13 incident, reproduced as a fixture: the real 400 body
    /// Codex returned for `gpt-5.6` on a ChatGPT account. It never echoes
    /// the probe token, so it must be reported `Broken` (not `Reachable`) —
    /// this was already true before this fix; this test pins it against
    /// regressions now that the substitution check runs in the same branch.
    #[tokio::test]
    async fn probe_target_reports_broken_for_real_unsupported_model_400_body() {
        let dir = tempfile::tempdir().unwrap();
        let body = r#"{"type":"error","status":400,"error":{"type":"invalid_request_error","message":"The 'gpt-5.6' model is not supported when using Codex with a ChatGPT account."}}"#;
        let script = write_script(&dir, "codex-400-cli", &format!("echo '{body}'\nexit 0\n"));
        let config = config_with_cli_and_model_flag("codex-400", &script);
        let target = ProbeTarget {
            platform: "codex-400".to_string(),
            model: Some("gpt-5.6".to_string()),
            effort: None,
        };
        let report = probe_target(&config, &target, None, Duration::from_secs(5)).await;
        assert_eq!(report.outcome, ProbeOutcome::Broken);
        assert!(!report.outcome.reachable());
        assert!(report.error.unwrap().contains("not supported"));
    }

    /// The gap the `detect_model_rejection` fix closes: the harness echoes the
    /// prompt (which contains the probe token) inside its error output *and*
    /// also carries a "model is not supported" rejection. A naive token check
    /// would report `Reachable` because the token is present; rejection must
    /// override token presence and report `Broken`.
    #[tokio::test]
    async fn probe_target_reports_broken_for_model_rejection_even_with_token_present() {
        let dir = tempfile::tempdir().unwrap();
        let script = write_script(
            &dir,
            "reject-with-token-cli",
            "echo \"The 'gpt-5.6' model is not supported when using Codex with a ChatGPT account.\"\necho \"$1\"\n",
        );
        let config = config_with_cli_and_model_flag("reject-with-token", &script);
        let target = ProbeTarget {
            platform: "reject-with-token".to_string(),
            model: Some("gpt-5.6".to_string()),
            effort: None,
        };
        let report = probe_target(&config, &target, None, Duration::from_secs(5)).await;
        assert_eq!(report.outcome, ProbeOutcome::Broken);
        assert!(!report.outcome.reachable());
        assert!(report
            .error
            .unwrap()
            .to_lowercase()
            .contains("not supported"));
    }

    /// The `invalid_request` + `model` branch of `detect_model_rejection`:
    /// a rejection whose wording ("Unknown model") none of the fixed
    /// markers match, but which the CLI still tags as an
    /// `invalid_request_error`. Token is echoed too, so this also pins that
    /// the rejection wins over token presence.
    #[tokio::test]
    async fn probe_target_reports_broken_for_invalid_request_model_rejection() {
        let dir = tempfile::tempdir().unwrap();
        let body = r#"{"error":{"type":"invalid_request_error","message":"Unknown model gpt-5.6 requested"}}"#;
        let script = write_script(
            &dir,
            "invalid-request-cli",
            &format!("echo '{body}'\necho \"$1\"\n"),
        );
        let config = config_with_cli_and_model_flag("invalid-request", &script);
        let target = ProbeTarget {
            platform: "invalid-request".to_string(),
            model: Some("gpt-5.6".to_string()),
            effort: None,
        };
        let report = probe_target(&config, &target, None, Duration::from_secs(5)).await;
        assert_eq!(report.outcome, ProbeOutcome::Broken);
        assert!(!report.outcome.reachable());
        assert!(report.error.unwrap().contains("invalid_request_error"));
    }

    /// A healthy probe whose output happens to contain both "invalid" and
    /// "model" on one line (but not `invalid_request`) must NOT be misread
    /// as a rejection — guards the anchoring of the catch-all branch.
    #[tokio::test]
    async fn probe_target_reachable_despite_incidental_invalid_and_model_words() {
        let dir = tempfile::tempdir().unwrap();
        let script = write_script(
            &dir,
            "noisy-healthy-cli",
            "echo 'note: pruned invalid cache entries for model gpt-5'\necho \"$1\"\n",
        );
        let config = config_with_cli_and_model_flag("noisy-healthy", &script);
        let target = ProbeTarget {
            platform: "noisy-healthy".to_string(),
            model: Some("gpt-5".to_string()),
            effort: None,
        };
        let report = probe_target(&config, &target, None, Duration::from_secs(5)).await;
        assert_eq!(report.outcome, ProbeOutcome::Reachable);
        assert!(report.error.is_none());
    }

    /// The other half of the 2026-08-13 incident: the harness answers with
    /// the probe token present (so a naive check would call it healthy) but
    /// its own output also carries the "requested model not recognized,
    /// substituting a different one" warning. Must be `Substituted`, never
    /// `Reachable` — the answer did not come from the model that was
    /// actually requested.
    #[tokio::test]
    async fn probe_target_reports_substituted_for_model_metadata_fallback_warning() {
        let dir = tempfile::tempdir().unwrap();
        let script = write_script(
            &dir,
            "fallback-cli",
            "echo \"warning: Model metadata for 'gpt-5.6' not found. Defaulting to fallback metadata\" 1>&2\necho \"$1\"\n",
        );
        let config = config_with_cli_and_model_flag("fallback", &script);
        let target = ProbeTarget {
            platform: "fallback".to_string(),
            model: Some("gpt-5.6".to_string()),
            effort: None,
        };
        let report = probe_target(&config, &target, None, Duration::from_secs(5)).await;
        assert_eq!(report.outcome, ProbeOutcome::Substituted);
        assert!(!report.outcome.reachable());
        assert!(report
            .error
            .unwrap()
            .to_lowercase()
            .contains("fallback metadata"));
    }

    /// A response with no substitution warning and no probe token at all
    /// must stay `Broken`, not `Substituted` — the substitution check must
    /// not fire on unrelated broken output.
    #[tokio::test]
    async fn probe_target_does_not_report_substituted_without_the_warning() {
        let dir = tempfile::tempdir().unwrap();
        let script = write_script(
            &dir,
            "plain-broken-cli",
            "echo 'Error: something else went wrong' 1>&2\nexit 0\n",
        );
        let config = config_with_cli_and_model_flag("plain-broken", &script);
        let target = ProbeTarget {
            platform: "plain-broken".to_string(),
            model: Some("some-model".to_string()),
            effort: None,
        };
        let report = probe_target(&config, &target, None, Duration::from_secs(5)).await;
        assert_eq!(report.outcome, ProbeOutcome::Broken);
    }

    /// The `mistral` case: a `model` is requested but this platform's CLI
    /// has no `model_flag` configured at all — it addresses named agents,
    /// not models, so the pair can never be exercised end to end. Must be
    /// `Unknown` without ever spawning the (misleading) bare-default
    /// process, and never `Reachable`.
    #[tokio::test]
    async fn probe_target_reports_unknown_when_platform_has_no_model_flag() {
        let dir = tempfile::tempdir().unwrap();
        // If this script ran and its output were consulted, the probe would
        // wrongly look healthy — proving the `Unknown` verdict came from
        // the pre-spawn check, not from judging a response.
        let script = write_script(&dir, "no-model-flag-cli", "echo \"$1\"\n");
        let config = config_with_cli("no-model-flag", &script);
        let target = ProbeTarget {
            platform: "no-model-flag".to_string(),
            model: Some("mistral-medium-latest".to_string()),
            effort: None,
        };
        let report = probe_target(&config, &target, None, Duration::from_secs(5)).await;
        assert_eq!(report.outcome, ProbeOutcome::Unknown);
        assert!(!report.outcome.reachable());
        assert!(report.error.unwrap().contains("no configured way"));
    }

    /// Omitting `model` must keep today's behaviour exactly: even on a
    /// platform with no `model_flag`, probing the bare default is fine and
    /// must not trip the `Unknown` check (that check only applies when a
    /// *specific* model was requested and can't be honored).
    #[tokio::test]
    async fn probe_target_without_model_ignores_missing_model_flag() {
        let dir = tempfile::tempdir().unwrap();
        let script = write_script(&dir, "no-model-flag-default-cli", "echo \"$1\"\n");
        let config = config_with_cli("no-model-flag-default", &script);
        let target = ProbeTarget {
            platform: "no-model-flag-default".to_string(),
            model: None,
            effort: None,
        };
        let report = probe_target(&config, &target, None, Duration::from_secs(5)).await;
        assert_eq!(report.outcome, ProbeOutcome::Reachable);
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
            effort: None,
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
            effort: None,
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
            effort: None,
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
                effort: None,
            },
            ProbeTarget {
                platform: "hang-b".to_string(),
                model: None,
                effort: None,
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
        let mut hooks = std::collections::BTreeMap::new();
        if let Some(hook) = on_completed {
            hooks.insert(crate::domain::loops::LoopHookEvent::OnCompleted, vec![hook]);
        }
        crate::domain::loops::Loop {
            archived: false,
            paused_by_reconciliation: false,
            infra_node_id: None,
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
            active_run_queue_id: None,
            hooks,
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
            platform: Some("mimo".to_string()),
            model: None,
            effort: None,
            prompt: Some("done".to_string()),
            command: None,
            target_session_id: None,
            timeout_minutes: None,
            target_loop_id: None,
            queue_id: None,
            workdir_override: None,
            idea: None,
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
        assert_eq!(targets[0].used_by, vec!["on_completed hook 0"]);
    }

    #[test]
    fn distinct_targets_for_loop_includes_all_hook_events_and_dedups() {
        use crate::domain::loops::{LoopCompletionHook, LoopHookEvent};
        use std::collections::BTreeMap;
        let mut hooks: BTreeMap<LoopHookEvent, Vec<LoopCompletionHook>> = BTreeMap::new();
        hooks.insert(
            LoopHookEvent::OnCompleted,
            vec![LoopCompletionHook {
                platform: Some("mimo".to_string()),
                model: None,
                effort: None,
                prompt: Some("done".to_string()),
                command: None,
                target_session_id: None,
                timeout_minutes: None,
                target_loop_id: None,
                queue_id: None,
                workdir_override: None,
                idea: None,
            }],
        );
        hooks.insert(
            LoopHookEvent::OnFailed,
            vec![LoopCompletionHook {
                platform: Some("mimo".to_string()),
                model: None,
                effort: None,
                prompt: Some("failed {{blocker}}".to_string()),
                command: None,
                target_session_id: None,
                timeout_minutes: None,
                target_loop_id: None,
                queue_id: None,
                workdir_override: None,
                idea: None,
            }],
        );
        hooks.insert(
            LoopHookEvent::OnSpecCompleted,
            vec![LoopCompletionHook {
                platform: Some("other-cli".to_string()),
                model: None,
                effort: None,
                prompt: Some("spec {{spec_name}}".to_string()),
                command: None,
                target_session_id: None,
                timeout_minutes: None,
                target_loop_id: None,
                queue_id: None,
                workdir_override: None,
                idea: None,
            }],
        );
        let mut lp = sample_loop(None);
        lp.hooks = hooks;
        let details = LoopDetails {
            lp,
            graph_nodes: vec![],
            graph_edges: vec![],
            specs: vec![],
            completion_hook_runs: vec![],
        };

        let targets = distinct_targets_for_loop(&details);
        assert_eq!(targets.len(), 2);
        let mimo = targets
            .iter()
            .find(|t| t.target.platform == "mimo")
            .unwrap();
        assert_eq!(
            mimo.used_by,
            vec!["on_completed hook 0", "on_failed hook 0"]
        );
        let other = targets
            .iter()
            .find(|t| t.target.platform == "other-cli")
            .unwrap();
        assert_eq!(other.used_by, vec!["on_spec_completed hook 0"]);
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

    fn report(outcome: ProbeOutcome) -> ProbeReport {
        ProbeReport {
            platform: "p".to_string(),
            model: None,
            outcome,
            duration_ms: 0,
            error: None,
        }
    }

    /// `would_fail` must count every confirmed-failure outcome — not just
    /// `Broken` — and must never count `Reachable`.
    #[test]
    fn would_fail_count_counts_every_confirmed_failure_outcome() {
        let reports = vec![
            report(ProbeOutcome::Reachable),
            report(ProbeOutcome::Broken),
            report(ProbeOutcome::TimedOut),
            report(ProbeOutcome::NotConfigured),
            report(ProbeOutcome::SpawnFailed),
            report(ProbeOutcome::Substituted),
        ];
        assert_eq!(would_fail_count(&reports), 5);
    }

    /// The core honesty requirement: a pair whose validity is `Unknown`
    /// must not be counted as a confirmed failure by `would_fail`, and must
    /// not vanish into a `would_fail` of zero as if it had passed either —
    /// it has its own count via `unknown_count`.
    #[test]
    fn would_fail_count_does_not_count_unknown_as_a_failure_or_a_pass() {
        let reports = vec![report(ProbeOutcome::Unknown), report(ProbeOutcome::Unknown)];
        assert_eq!(would_fail_count(&reports), 0);
        assert_eq!(unknown_count(&reports), 2);
    }

    #[test]
    fn unknown_count_ignores_every_other_outcome() {
        let reports = vec![
            report(ProbeOutcome::Reachable),
            report(ProbeOutcome::Broken),
            report(ProbeOutcome::TimedOut),
            report(ProbeOutcome::NotConfigured),
            report(ProbeOutcome::SpawnFailed),
            report(ProbeOutcome::Substituted),
        ];
        assert_eq!(unknown_count(&reports), 0);
    }

    /// `agent_probe` and `loop_preflight` (handler.rs) both build their JSON
    /// response exclusively from `ProbeReport::to_json()` and this module's
    /// `would_fail_count`/`unknown_count` — there is no second copy of the
    /// outcome vocabulary for either tool to drift from. This test pins the
    /// vocabulary itself so a future new variant can't silently ship a word
    /// one caller knows about and the other doesn't.
    #[test]
    fn probe_outcome_vocabulary_is_stable_and_shared() {
        let all = [
            ProbeOutcome::Reachable,
            ProbeOutcome::Broken,
            ProbeOutcome::TimedOut,
            ProbeOutcome::NotConfigured,
            ProbeOutcome::SpawnFailed,
            ProbeOutcome::Unknown,
            ProbeOutcome::Substituted,
        ];
        let words: Vec<&str> = all.iter().map(ProbeOutcome::as_str).collect();
        assert_eq!(
            words,
            vec![
                "reachable",
                "broken",
                "timed_out",
                "not_configured",
                "spawn_failed",
                "unknown",
                "substituted",
            ]
        );
        // Only `Reachable` is ever a pass.
        assert_eq!(all.iter().filter(|o| o.reachable()).count(), 1);
    }
}
