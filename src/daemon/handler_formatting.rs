use chrono::Utc;
use rmcp::ErrorData as McpError;

use crate::application::ports::{AgentRepository, RunRepository};
use crate::daemon::helpers::{data_dir, filter_log_line};
use crate::db::Database;
use crate::domain::models::{Agent, RunLog, Trigger};

pub(crate) fn format_uptime(secs: u64) -> String {
    if secs > 3600 {
        format!("{}h {}m", secs / 3600, (secs % 3600) / 60)
    } else if secs > 60 {
        format!("{}m {}s", secs / 60, secs % 60)
    } else {
        format!("{secs}s")
    }
}

pub(crate) fn format_agent_info(a: &Agent) -> String {
    let prompt_preview = if a.prompt.len() > 80 {
        format!("{}...", &a.prompt[..80])
    } else {
        a.prompt.clone()
    };

    let status = if !a.enabled {
        "disabled"
    } else if a.is_expired() {
        "expired"
    } else {
        "active"
    };

    let trigger_label = a.trigger_type_label();
    let trigger_detail = match &a.trigger {
        Some(Trigger::Cron { schedule_expr }) => schedule_expr.clone(),
        Some(Trigger::Watch { path, .. }) => path.clone(),
        None => "manual".to_string(),
    };

    let mut info = format!(
        "- **{}** [{}] ({})\n Trigger: {} `{}`\n CLI: {}\n Prompt: {}\n",
        a.id, status, trigger_label, trigger_label, trigger_detail, a.cli, prompt_preview
    );

    if let Some(last) = a.last_run_at {
        let ok_str = a
            .last_run_ok
            .map(|ok| if ok { "success" } else { "failed" })
            .unwrap_or("unknown");
        info.push_str(&format!(" Last run: {} ({})\n", last.to_rfc3339(), ok_str));
    }

    if let Some(last) = a.last_triggered_at {
        info.push_str(&format!(
            " Last triggered: {} (count: {})\n",
            last.to_rfc3339(),
            a.trigger_count
        ));
    }

    if let Some(exp) = a.expires_at {
        let remaining = exp.signed_duration_since(Utc::now());
        if remaining.num_seconds() > 0 {
            info.push_str(&format!(" Expires in: {}m\n", remaining.num_minutes()));
        } else {
            info.push_str(" Status: EXPIRED\n");
        }
    }

    info
}

pub(crate) fn resolve_log_path(db: &Database, id: &str) -> Result<String, McpError> {
    let Some(agent) = db.get_agent(id).map_err(internal_error)? else {
        return default_log_path(id);
    };
    Ok(agent.log_path)
}

fn default_log_path(id: &str) -> Result<String, McpError> {
    Ok(data_dir()
        .map_err(internal_error)?
        .join("logs")
        .join(id)
        .with_extension("log")
        .to_string_lossy()
        .to_string())
}

pub(crate) fn format_log_output(
    path: &std::path::Path,
    id: &str,
    since: Option<&str>,
    max_lines: usize,
) -> Result<String, McpError> {
    let content = std::fs::read_to_string(path)
        .map_err(|e| internal_error(format!("Failed to read log: {e}")))?;
    let mut lines: Vec<&str> = content.lines().collect();

    if let Some(since) = since {
        if let Ok(since_dt) = chrono::DateTime::parse_from_rfc3339(since) {
            lines.retain(|line| filter_log_line(line, &since_dt));
        }
    }

    let total = lines.len();
    if lines.len() > max_lines {
        lines = lines[lines.len() - max_lines..].to_vec();
    }

    if lines.is_empty() {
        return Ok(format!("No log entries for '{}' matching the filter.", id));
    }

    Ok(format!(
        "Logs for '{}' (showing {} of {} lines):\n\n{}",
        id,
        lines.len(),
        total,
        lines.join("\n")
    ))
}

pub(crate) fn recent_runs_output(db: &Database, id: &str) -> Option<String> {
    let Ok(runs) = db.list_runs(id, 5) else {
        return None;
    };
    if runs.is_empty() {
        return None;
    }

    let mut output = String::from("\n\nRecent executions:\n");
    for run in &runs {
        output.push_str(&format_run_line(run));
    }
    Some(output)
}

pub(crate) fn format_run_line(run: &RunLog) -> String {
    let duration = run
        .finished_at
        .map(|finished_at| {
            format!(
                "{}s",
                finished_at
                    .signed_duration_since(run.started_at)
                    .num_seconds()
            )
        })
        .unwrap_or_else(|| "in progress".to_string());
    let summary = run
        .summary
        .as_deref()
        .map(|summary| format!(" — {summary}"))
        .unwrap_or_default();

    format!(
        " - {} | {} | {} | {}{}\n",
        run.started_at.to_rfc3339(),
        run.trigger_type,
        run.status.as_str(),
        duration,
        summary,
    )
}

pub(crate) fn make_log_path(id: &str) -> Result<String, McpError> {
    let log_dir = data_dir().map_err(internal_error)?.join("logs");
    std::fs::create_dir_all(&log_dir).map_err(internal_error)?;
    Ok(log_dir
        .join(id)
        .with_extension("log")
        .to_string_lossy()
        .to_string())
}

pub(crate) fn internal_error(error: impl std::fmt::Display) -> McpError {
    McpError::internal_error(error.to_string(), None)
}

pub(crate) fn format_temporal_agents(agents: &[Agent]) -> String {
    agents
        .iter()
        .filter(|a| a.expires_at.is_some() && a.enabled)
        .map(|a| {
            let remaining = a.expires_at.unwrap().signed_duration_since(Utc::now());
            if remaining.num_seconds() > 0 {
                format!(" - {}: {}m remaining", a.id, remaining.num_minutes())
            } else {
                format!(" - {}: EXPIRED", a.id)
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Providers surfaced by `agent_models`, with display names.
const MODEL_PROVIDERS: &[(&str, &str)] = &[
    ("anthropic", "Anthropic"),
    ("openai", "OpenAI"),
    ("google", "Google"),
    ("mistral", "Mistral"),
    ("xai", "xAI"),
    ("deepseek", "DeepSeek"),
    ("amazon", "Amazon"),
    ("alibaba", "Alibaba"),
];

/// Cap on how many models are shown per provider in the **unfiltered**
/// (no-platform) `agent_models` listing before `full: true` is required to see
/// the rest — *not* a "newest N" guarantee: ordering is source-dependent (see
/// [`format_models_for_providers`], which sorts by release date, and
/// [`format_native_models`], which preserves whatever order the CLI itself
/// enumerated in, e.g. alphabetical for opencode-go). Anything this cap cuts
/// is reported via [`ModelTruncation`]. Platform-scoped listings (with
/// `platform` set) are never capped — the caller already narrowed by platform
/// (CB37 FR1).
const MODELS_PER_PROVIDER: usize = 8;

/// Cap on how many providers the **unfiltered** (no-platform) listing renders,
/// so the full models.dev catalogue can't blow past MCP result size limits: at
/// most `MAX_PROVIDERS * MODELS_PER_PROVIDER` lines. Bypassed by `full: true`;
/// anything it cuts is reported via [`ModelTruncation`]. Platform-scoped
/// listings (with `platform` set) are never capped (CB37 FR1).
const MAX_PROVIDERS: usize = 12;

/// What a `format_*_models` call left out, so the caller can render an honest
/// notice instead of silently dropping ids that exist. Both fields empty
/// means nothing was cut.
#[derive(Default, Debug, PartialEq)]
pub(crate) struct ModelTruncation {
    /// Providers whose own model list was cut by [`MODELS_PER_PROVIDER`]:
    /// (display name, shown, total).
    per_provider: Vec<(String, usize, usize)>,
    /// Providers dropped entirely by [`MAX_PROVIDERS`]: (shown, total).
    providers: Option<(usize, usize)>,
    /// Why the shown models were chosen (FR3) — e.g. "newest first by release
    /// date". Stated per-provider in the notice so the caller can judge
    /// whether the missing ones matter.
    ordering_rule: Option<String>,
}

impl ModelTruncation {
    fn is_empty(&self) -> bool {
        self.per_provider.is_empty() && self.providers.is_none()
    }

    /// Render as a footer-voice notice, or `None` if nothing was cut.
    pub(crate) fn notice(&self) -> Option<String> {
        if self.is_empty() {
            return None;
        }
        let mut lines = Vec::new();
        let rule_suffix = self
            .ordering_rule
            .as_deref()
            .map(|r| format!(" ({r})"))
            .unwrap_or_default();
        for (display, shown, total) in &self.per_provider {
            lines.push(format!(
                "  {display}: showing {shown} of {total} models{rule_suffix}"
            ));
        }
        if let Some((shown, total)) = self.providers {
            lines.push(format!("  showing {shown} of {total} providers"));
        }
        Some(format!(
            "Truncated (pass `full: true` to see everything):\n{}",
            lines.join("\n")
        ))
    }
}

/// Human-readable name for a provider slug — the curated display name when we
/// have one, otherwise a title-cased fallback so platform-native providers
/// (e.g. `opencode-go`) still render nicely.
fn provider_display(slug: &str) -> String {
    if let Some((_, display)) = MODEL_PROVIDERS.iter().find(|(s, _)| *s == slug) {
        return (*display).to_string();
    }
    slug.split(['-', '_'])
        .map(|word| {
            let mut chars = word.chars();
            match chars.next() {
                Some(first) => first.to_uppercase().chain(chars).collect::<String>(),
                None => String::new(),
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// Format the cached models.dev catalog: models per major provider, source
/// order (release-date descending when dates are present), bounded by
/// [`MAX_PROVIDERS`] x [`MODELS_PER_PROVIDER`] unless `full` is true.
pub(crate) fn format_catalog_models(
    catalog: &crate::domain::models_db::ModelCatalog,
    full: bool,
) -> (String, ModelTruncation) {
    let providers: Vec<(&str, String)> = MODEL_PROVIDERS
        .iter()
        .map(|(slug, display)| (*slug, (*display).to_string()))
        .collect();
    format_models_for_providers(catalog, &providers, full, false)
}

/// Format only the models available to `provider_slugs` (a platform's mapped
/// providers), source order (release-date descending when dates are present).
/// Platform-scoped listings return every model uncapped (CB37 FR1) — the
/// caller already narrowed by platform, so an additional cap is neither
/// protective nor defensible.
pub(crate) fn format_platform_models(
    catalog: &crate::domain::models_db::ModelCatalog,
    provider_slugs: &[&str],
    full: bool,
) -> (String, ModelTruncation) {
    let providers: Vec<(&str, String)> = provider_slugs
        .iter()
        .map(|slug| (*slug, provider_display(slug)))
        .collect();
    format_models_for_providers(catalog, &providers, full, true)
}

/// Format a platform's native enumeration (e.g. `opencode models`): the ids are
/// already the literal, passable strings the CLI accepts (`opencode/big-pickle`),
/// so they are rendered verbatim — never re-derived — grouped by their provider
/// prefix (the segment before the first `/`) for readability. Platform-scoped
/// listings return every model uncapped (CB37 FR1) — the caller already narrowed
/// by platform, so an additional cap is neither protective nor defensible. The
/// `full` parameter is accepted for API compatibility but is a no-op here.
pub(crate) fn format_native_models(ids: &[String], _full: bool) -> (String, ModelTruncation) {
    // full is a no-op: platform-scoped listings are never capped (CB37)
    let mut groups: Vec<(String, Vec<&String>)> = Vec::new();
    for id in ids {
        let provider = id.split_once('/').map(|(p, _)| p).unwrap_or("");
        match groups.iter_mut().find(|(p, _)| p == provider) {
            Some((_, models)) => models.push(id),
            None => groups.push((provider.to_string(), vec![id])),
        }
    }

    let sections: Vec<String> = groups
        .iter()
        .map(|(provider, models)| {
            let display = if provider.is_empty() {
                "native".to_string()
            } else {
                provider_display(provider)
            };
            models
                .iter()
                .map(|id| format!("  {id}  ({display})"))
                .collect::<Vec<_>>()
                .join("\n")
        })
        .collect();

    (sections.join("\n"), ModelTruncation::default())
}

/// Shared renderer: source order (release-date descending when dates are
/// present) models for each listed provider that has any, skipping empty
/// providers. When `platform_scoped` is true (CB37), returns every model for
/// every provider — no per-provider or per-listing cap. When false (the
/// unfiltered, provider-wide listing), capped at [`MAX_PROVIDERS`] non-empty
/// providers x [`MODELS_PER_PROVIDER`] models unless `full` is true.
fn format_models_for_providers(
    catalog: &crate::domain::models_db::ModelCatalog,
    providers: &[(&str, String)],
    full: bool,
    platform_scoped: bool,
) -> (String, ModelTruncation) {
    let mut sections = Vec::new();
    let mut truncation = ModelTruncation::default();

    let total_providers = providers.len();
    let providers_to_show = if full || platform_scoped {
        total_providers
    } else {
        total_providers.min(MAX_PROVIDERS)
    };

    if !full && !platform_scoped && total_providers > MAX_PROVIDERS {
        truncation.providers = Some((providers_to_show, total_providers));
        truncation.ordering_rule = Some("newest first by release date".to_string());
    }

    for (slug, display) in providers.iter().take(providers_to_show) {
        let mut models: Vec<_> = catalog
            .models
            .iter()
            .filter(|m| m.provider == *slug)
            .collect();
        if models.is_empty() {
            continue;
        }
        models.sort_by(|a, b| b.release_date.cmp(&a.release_date));

        let total_models = models.len();
        let models_to_show = if full || platform_scoped {
            total_models
        } else {
            total_models.min(MODELS_PER_PROVIDER)
        };

        if !full && !platform_scoped && total_models > MODELS_PER_PROVIDER {
            truncation
                .per_provider
                .push((display.clone(), models_to_show, total_models));
            truncation.ordering_rule = Some("newest first by release date".to_string());
        }

        let lines = models
            .iter()
            .take(models_to_show)
            .map(|m| format!("  {}  ({display})", m.id))
            .collect::<Vec<_>>()
            .join("\n");
        sections.push(lines);
    }

    (sections.join("\n"), truncation)
}

#[cfg(test)]
mod formatting_unit_tests {
    use super::*;
    use crate::domain::models::{Cli, RunStatus, TriggerType, WatchEvent};
    use chrono::{Duration, Utc};

    fn make_agent(id: &str) -> Agent {
        Agent {
            id: id.to_string(),
            prompt: "test prompt".to_string(),
            trigger: None,
            cli: Cli("opencode".to_string()),
            model: None,
            effort: None,
            working_dir: None,
            enabled: true,
            enable_at: None,
            created_at: Utc::now(),
            log_path: "/tmp/test.log".to_string(),
            timeout_minutes: 15,
            expires_at: None,
            last_run_at: None,
            last_run_ok: None,
            last_triggered_at: None,
            trigger_count: 0,
        }
    }

    fn make_run(status: RunStatus, trigger: TriggerType) -> RunLog {
        RunLog {
            id: "run-1".to_string(),
            background_agent_id: "agent-1".to_string(),
            status,
            trigger_type: trigger,
            summary: None,
            started_at: Utc::now(),
            finished_at: None,
            exit_code: None,
            timeout_at: None,
        }
    }

    // ── format_uptime ────────────────────────────────────────────────

    #[test]
    fn uptime_zero_seconds() {
        assert_eq!(format_uptime(0), "0s");
    }

    #[test]
    fn uptime_one_second() {
        assert_eq!(format_uptime(1), "1s");
    }

    #[test]
    fn uptime_59_seconds() {
        assert_eq!(format_uptime(59), "59s");
    }

    #[test]
    fn uptime_exactly_60_seconds() {
        assert_eq!(format_uptime(60), "60s");
    }

    #[test]
    fn uptime_90_seconds() {
        assert_eq!(format_uptime(90), "1m 30s");
    }

    #[test]
    fn uptime_exactly_3600_seconds() {
        assert_eq!(format_uptime(3600), "60m 0s");
    }

    #[test]
    fn uptime_3661_seconds() {
        assert_eq!(format_uptime(3661), "1h 1m");
    }

    #[test]
    fn uptime_86400_seconds() {
        assert_eq!(format_uptime(86400), "24h 0m");
    }

    #[test]
    fn uptime_large_value() {
        assert_eq!(format_uptime(90061), "25h 1m");
    }

    // ── format_agent_info ────────────────────────────────────────────

    #[test]
    fn agent_info_active_cron() {
        let mut a = make_agent("cron-1");
        a.trigger = Some(Trigger::Cron {
            schedule_expr: "*/5 * * * *".to_string(),
        });
        let info = format_agent_info(&a);
        assert!(info.contains("**cron-1**"));
        assert!(info.contains("[active]"));
        assert!(info.contains("(cron)"));
        assert!(info.contains("*/5 * * * *"));
        assert!(info.contains("test prompt"));
    }

    #[test]
    fn agent_info_disabled() {
        let mut a = make_agent("dis-1");
        a.enabled = false;
        let info = format_agent_info(&a);
        assert!(info.contains("[disabled]"));
    }

    #[test]
    fn agent_info_expired() {
        let mut a = make_agent("exp-1");
        a.expires_at = Some(Utc::now() - Duration::hours(1));
        let info = format_agent_info(&a);
        assert!(info.contains("[expired]"));
        assert!(info.contains("EXPIRED"));
    }

    #[test]
    fn agent_info_manual_trigger() {
        let a = make_agent("man-1");
        let info = format_agent_info(&a);
        assert!(info.contains("(manual)"));
        assert!(info.contains("Trigger: manual `manual`"));
    }

    #[test]
    fn agent_info_watch_trigger() {
        let mut a = make_agent("watch-1");
        a.trigger = Some(Trigger::Watch {
            path: "/home/user/project".to_string(),
            events: vec![WatchEvent::Modify],
            debounce_seconds: 2,
            recursive: false,
        });
        let info = format_agent_info(&a);
        assert!(info.contains("(watch)"));
        assert!(info.contains("/home/user/project"));
    }

    #[test]
    fn agent_info_long_prompt_truncated() {
        let mut a = make_agent("long-1");
        a.prompt = "x".repeat(120);
        let info = format_agent_info(&a);
        assert!(info.contains("..."));
        let prompt_line = info.lines().find(|l| l.contains("Prompt:")).unwrap();
        // The prompt value should be at most 80 chars + "..."
        let after_prompt = prompt_line.split("Prompt: ").nth(1).unwrap();
        assert!(after_prompt.len() <= 84);
    }

    #[test]
    fn agent_info_short_prompt_not_truncated() {
        let mut a = make_agent("short-1");
        a.prompt = "hello".to_string();
        let info = format_agent_info(&a);
        assert!(info.contains("hello"));
        assert!(!info.contains("..."));
    }

    #[test]
    fn agent_info_last_run_success() {
        let mut a = make_agent("run-ok");
        a.last_run_at = Some(Utc::now() - Duration::minutes(5));
        a.last_run_ok = Some(true);
        let info = format_agent_info(&a);
        assert!(info.contains("Last run:"));
        assert!(info.contains("success"));
    }

    #[test]
    fn agent_info_last_run_failed() {
        let mut a = make_agent("run-fail");
        a.last_run_at = Some(Utc::now() - Duration::minutes(2));
        a.last_run_ok = Some(false);
        let info = format_agent_info(&a);
        assert!(info.contains("failed"));
    }

    #[test]
    fn agent_info_last_run_unknown() {
        let mut a = make_agent("run-unk");
        a.last_run_at = Some(Utc::now());
        a.last_run_ok = None;
        let info = format_agent_info(&a);
        assert!(info.contains("unknown"));
    }

    #[test]
    fn agent_info_last_triggered() {
        let mut a = make_agent("trig-1");
        a.last_triggered_at = Some(Utc::now() - Duration::hours(1));
        a.trigger_count = 42;
        let info = format_agent_info(&a);
        assert!(info.contains("Last triggered:"));
        assert!(info.contains("count: 42"));
    }

    #[test]
    fn agent_info_expires_in() {
        let mut a = make_agent("exp-in");
        a.expires_at = Some(Utc::now() + Duration::minutes(120));
        let info = format_agent_info(&a);
        assert!(info.contains("Expires in:"));
        assert!(info.contains("m\n"));
    }

    // ── format_run_line ──────────────────────────────────────────────

    #[test]
    fn run_line_in_progress() {
        let run = make_run(RunStatus::InProgress, TriggerType::Scheduled);
        let line = format_run_line(&run);
        assert!(line.contains("in progress"));
        assert!(line.contains("scheduled"));
        assert!(line.contains("in_progress"));
    }

    #[test]
    fn run_line_finished() {
        let mut run = make_run(RunStatus::Success, TriggerType::Manual);
        run.started_at = Utc::now() - Duration::seconds(10);
        run.finished_at = Some(Utc::now());
        let line = format_run_line(&run);
        assert!(line.contains("10s"));
        assert!(line.contains("success"));
        assert!(line.contains("manual"));
    }

    #[test]
    fn run_line_with_summary() {
        let mut run = make_run(RunStatus::Error, TriggerType::Watch);
        run.summary = Some("something went wrong".to_string());
        run.started_at = Utc::now() - Duration::seconds(5);
        run.finished_at = Some(Utc::now());
        let line = format_run_line(&run);
        assert!(line.contains("something went wrong"));
        assert!(line.contains("error"));
    }

    #[test]
    fn run_line_no_summary() {
        let run = make_run(RunStatus::Success, TriggerType::Scheduled);
        let line = format_run_line(&run);
        assert!(!line.contains("—"));
    }

    // ── format_temporal_agents ───────────────────────────────────────

    #[test]
    fn temporal_agents_empty() {
        assert_eq!(format_temporal_agents(&[]), "");
    }

    #[test]
    fn temporal_agents_filters_disabled() {
        let mut a = make_agent("d-1");
        a.enabled = false;
        a.expires_at = Some(Utc::now() + Duration::minutes(10));
        assert_eq!(format_temporal_agents(&[a]), "");
    }

    #[test]
    fn temporal_agents_filters_no_expiry() {
        let a = make_agent("ne-1");
        assert_eq!(format_temporal_agents(&[a]), "");
    }

    #[test]
    fn temporal_agents_active_expiry() {
        let mut a = make_agent("ok-1");
        a.expires_at = Some(Utc::now() + Duration::minutes(120));
        let out = format_temporal_agents(&[a]);
        assert!(out.contains("ok-1"));
        assert!(out.contains("remaining"));
    }

    #[test]
    fn temporal_agents_expired() {
        let mut a = make_agent("exp-2");
        a.expires_at = Some(Utc::now() - Duration::minutes(5));
        let out = format_temporal_agents(&[a]);
        assert!(out.contains("exp-2"));
        assert!(out.contains("EXPIRED"));
    }

    #[test]
    fn temporal_agents_mixed() {
        let mut a1 = make_agent("a1");
        a1.expires_at = Some(Utc::now() + Duration::minutes(10));
        let mut a2 = make_agent("a2");
        a2.enabled = false;
        a2.expires_at = Some(Utc::now() + Duration::minutes(5));
        let mut a3 = make_agent("a3");
        a3.expires_at = Some(Utc::now() - Duration::minutes(1));
        let out = format_temporal_agents(&[a1, a2, a3]);
        // Only a1 (active + expires) and a3 (active + expired) appear
        assert!(out.contains("a1"));
        assert!(!out.contains("a2"));
        assert!(out.contains("a3"));
        assert!(out.lines().count() == 2);
    }

    // ── provider_display ─────────────────────────────────────────────

    #[test]
    fn provider_display_known() {
        assert_eq!(provider_display("anthropic"), "Anthropic");
        assert_eq!(provider_display("openai"), "OpenAI");
        assert_eq!(provider_display("google"), "Google");
        assert_eq!(provider_display("mistral"), "Mistral");
        assert_eq!(provider_display("xai"), "xAI");
        assert_eq!(provider_display("deepseek"), "DeepSeek");
        assert_eq!(provider_display("amazon"), "Amazon");
        assert_eq!(provider_display("alibaba"), "Alibaba");
    }

    #[test]
    fn provider_display_unknown_single_word() {
        assert_eq!(provider_display("cohere"), "Cohere");
    }

    #[test]
    fn provider_display_hyphenated() {
        assert_eq!(provider_display("opencode-go"), "Opencode Go");
    }

    #[test]
    fn provider_display_underscored() {
        assert_eq!(provider_display("my_provider"), "My Provider");
    }

    #[test]
    fn provider_display_empty() {
        assert_eq!(provider_display(""), "");
    }

    // ── format_catalog_models ────────────────────────────────────────

    #[test]
    fn catalog_models_empty() {
        use crate::domain::models_db::ModelCatalog;
        use std::time::SystemTime;
        let catalog = ModelCatalog {
            models: vec![],
            fetched_at: SystemTime::now(),
        };
        let (out, _) = format_catalog_models(&catalog, false);
        assert!(out.is_empty());
    }

    #[test]
    fn catalog_models_single_provider() {
        use crate::domain::models_db::{ModelCatalog, ModelEntry};
        use std::time::SystemTime;
        let catalog = ModelCatalog {
            models: vec![
                ModelEntry {
                    id: "claude-opus-4-8".to_string(),
                    name: "Claude Opus 4.8".to_string(),
                    provider: "anthropic".to_string(),
                    release_date: Some("2025-01-01".to_string()),
                    size_hint: None,
                },
                ModelEntry {
                    id: "claude-sonnet-4-6".to_string(),
                    name: "Claude Sonnet 4.6".to_string(),
                    provider: "anthropic".to_string(),
                    release_date: Some("2024-06-01".to_string()),
                    size_hint: None,
                },
            ],
            fetched_at: SystemTime::now(),
        };
        let (out, _) = format_catalog_models(&catalog, false);
        assert!(out.contains("claude-opus-4-8"));
        assert!(out.contains("claude-sonnet-4-6"));
        assert!(out.contains("(Anthropic)"));
    }

    #[test]
    fn catalog_models_empty_provider_skipped() {
        use crate::domain::models_db::{ModelCatalog, ModelEntry};
        use std::time::SystemTime;
        let catalog = ModelCatalog {
            models: vec![ModelEntry {
                id: "gpt-4".to_string(),
                name: "GPT-4".to_string(),
                provider: "openai".to_string(),
                release_date: None,
                size_hint: None,
            }],
            fetched_at: SystemTime::now(),
        };
        let (out, _) = format_catalog_models(&catalog, false);
        // anthropic is listed first in MODEL_PROVIDERS but has no models → skipped
        assert!(out.contains("gpt-4"));
        assert!(out.contains("(OpenAI)"));
    }

    #[test]
    fn catalog_models_sorted_by_release_date() {
        use crate::domain::models_db::{ModelCatalog, ModelEntry};
        use std::time::SystemTime;
        let catalog = ModelCatalog {
            models: vec![
                ModelEntry {
                    id: "old-model".to_string(),
                    name: "Old".to_string(),
                    provider: "anthropic".to_string(),
                    release_date: Some("2023-01-01".to_string()),
                    size_hint: None,
                },
                ModelEntry {
                    id: "new-model".to_string(),
                    name: "New".to_string(),
                    provider: "anthropic".to_string(),
                    release_date: Some("2025-06-01".to_string()),
                    size_hint: None,
                },
            ],
            fetched_at: SystemTime::now(),
        };
        let (out, _) = format_catalog_models(&catalog, false);
        let new_pos = out.find("new-model").unwrap();
        let old_pos = out.find("old-model").unwrap();
        assert!(new_pos < old_pos, "newer model should appear first");
    }

    #[test]
    fn catalog_models_capped_per_provider() {
        use crate::domain::models_db::{ModelCatalog, ModelEntry};
        use std::time::SystemTime;
        let models: Vec<ModelEntry> = (0..20)
            .map(|i| ModelEntry {
                id: format!("model-{i}"),
                name: format!("Model {i}"),
                provider: "anthropic".to_string(),
                release_date: Some(format!("2025-01-{:02}", i + 1)),
                size_hint: None,
            })
            .collect();
        let catalog = ModelCatalog {
            models,
            fetched_at: SystemTime::now(),
        };
        let (out, truncation) = format_catalog_models(&catalog, false);
        let anthropic_lines = out.lines().filter(|l| l.contains("Anthropic")).count();
        assert_eq!(anthropic_lines, MODELS_PER_PROVIDER);
        assert_eq!(truncation.per_provider.len(), 1);
        assert_eq!(
            truncation.per_provider[0],
            ("Anthropic".to_string(), MODELS_PER_PROVIDER, 20)
        );
    }

    // ── format_platform_models ───────────────────────────────────────

    #[test]
    fn platform_models_unknown_provider_display() {
        use crate::domain::models_db::{ModelCatalog, ModelEntry};
        use std::time::SystemTime;
        let catalog = ModelCatalog {
            models: vec![ModelEntry {
                id: "my-model".to_string(),
                name: "My Model".to_string(),
                provider: "my-custom".to_string(),
                release_date: None,
                size_hint: None,
            }],
            fetched_at: SystemTime::now(),
        };
        let (out, _) = format_platform_models(&catalog, &["my-custom"], false);
        assert!(out.contains("my-model"));
        assert!(out.contains("(My Custom)"));
    }

    #[test]
    fn platform_models_uncapped() {
        use crate::domain::models_db::{ModelCatalog, ModelEntry};
        use std::time::SystemTime;
        let models: Vec<ModelEntry> = (0..15)
            .map(|i| ModelEntry {
                id: format!("model-{i}"),
                name: format!("Model {i}"),
                provider: format!("provider-{i}"),
                release_date: None,
                size_hint: None,
            })
            .collect();
        let catalog = ModelCatalog {
            models,
            fetched_at: SystemTime::now(),
        };
        let slugs: Vec<&str> = (0..15)
            .map(|i| Box::leak(format!("provider-{i}").into_boxed_str()) as &str)
            .collect();
        let (out, truncation) = format_platform_models(&catalog, &slugs, false);
        assert!(truncation.is_empty());
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines.len(), 15);
    }

    // ── format_native_models edge cases ──────────────────────────────

    #[test]
    fn native_models_empty() {
        let (out, truncation) = format_native_models(&[], false);
        assert_eq!(out, "");
        assert!(truncation.is_empty());
    }

    #[test]
    fn native_models_no_slash() {
        let ids = vec!["bare-model-id".to_string()];
        let (out, _) = format_native_models(&ids, false);
        assert!(out.contains("bare-model-id"));
        assert!(out.contains("(native)"));
    }

    #[test]
    fn native_models_groups_preserve_order() {
        let ids = vec![
            "anthropic/claude-a".to_string(),
            "openai/gpt-b".to_string(),
            "anthropic/claude-c".to_string(),
        ];
        let (out, _) = format_native_models(&ids, false);
        let a_pos = out.find("claude-a").unwrap();
        let b_pos = out.find("gpt-b").unwrap();
        let c_pos = out.find("claude-c").unwrap();
        assert!(a_pos < c_pos, "models within a provider are grouped");
        assert!(c_pos < b_pos, "first-seen provider ordering preserved");
    }

    #[test]
    fn native_models_many_providers_uncapped_when_platform_scoped() {
        let ids: Vec<String> = (0..30).map(|i| format!("prov{i}/model-{i}")).collect();
        let (out, truncation) = format_native_models(&ids, false);
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines.len(), 30);
        assert!(truncation.is_empty());
    }

    #[test]
    fn native_models_many_models_per_provider_uncapped() {
        let ids: Vec<String> = (0..30).map(|i| format!("anthropic/model-{i}")).collect();
        let (out, truncation) = format_native_models(&ids, false);
        let anthropic_lines = out.lines().filter(|l| l.contains("Anthropic")).count();
        assert_eq!(anthropic_lines, 30);
        assert!(truncation.is_empty());
    }

    // ── format_log_output edge cases ─────────────────────────────────

    #[test]
    fn log_output_nonexistent_file() {
        let path = std::path::Path::new("/nonexistent/path/to/log.log");
        let result = format_log_output(path, "test-agent", None, 100);
        assert!(result.is_err());
    }
}

#[cfg(test)]
mod model_listing_tests {
    use super::*;
    use crate::domain::models_db::{ModelCatalog, ModelEntry};
    use std::time::SystemTime;

    fn entry(provider: &str, id: &str) -> ModelEntry {
        ModelEntry {
            id: id.to_string(),
            name: id.to_string(),
            provider: provider.to_string(),
            release_date: None,
            size_hint: None,
        }
    }

    /// opencode enumerates its own models: the ids are already the passable
    /// `provider/model` form and must be rendered verbatim (this is the bug the
    /// spec exists for — bare ids fail at runtime with a generic server error).
    #[test]
    fn native_listing_emits_provider_prefixed_ids_verbatim() {
        let ids = vec![
            "opencode/big-pickle".to_string(),
            "opencode/mimo-v2.5-free".to_string(),
            "opencode-go/glm-5.2".to_string(),
        ];
        let (out, _) = format_native_models(&ids, false);
        // The passable id is the first token on each line, prefix intact.
        for want in &ids {
            assert!(
                out.lines().any(|l| l.trim_start().starts_with(want)),
                "missing passable id {want} in:\n{out}"
            );
        }
        // Grouped by provider prefix, with the prefix as a human label only.
        assert!(out.contains("(Opencode)"));
        assert!(out.contains("(Opencode Go)"));
        // Exactly the zen models that models.dev does not carry are present.
        assert!(out.contains("opencode/mimo-v2.5-free"));
        assert!(out.contains("opencode/big-pickle"));
    }

    /// claude has no native enumeration, so its listing derives bare ids from
    /// models.dev — `claude-opus-4-8`, not `anthropic/claude-opus-4-8`.
    #[test]
    fn models_dev_listing_emits_bare_ids_for_claude() {
        let catalog = ModelCatalog {
            models: vec![entry("anthropic", "claude-opus-4-8")],
            fetched_at: SystemTime::now(),
        };
        let (out, _) = format_platform_models(&catalog, &["anthropic"], false);
        assert!(out.contains("claude-opus-4-8"));
        assert!(
            !out.contains("anthropic/claude-opus-4-8"),
            "claude ids must stay bare (no provider prefix): {out}"
        );
    }

    #[test]
    fn native_listing_platform_scoped_returns_all() {
        // CB37: platform-scoped native listings are uncapped — every model
        // appears with no truncation notice.
        let ids: Vec<String> = (0..50)
            .map(|i| format!("nvidia/model-{i}"))
            .chain((0..50).map(|i| format!("opencode/zen-{i}")))
            .collect();
        let (out, truncation) = format_native_models(&ids, false);
        assert_eq!(out.lines().count(), 100);
        assert!(truncation.is_empty());
    }

    #[test]
    fn native_listing_full_is_noop() {
        // CB37: full is a no-op for native (platform-scoped) listings —
        // both produce the same uncapped output.
        let ids: Vec<String> = (0..50).map(|i| format!("nvidia/model-{i}")).collect();
        let (out_false, _) = format_native_models(&ids, false);
        let (out_true, _) = format_native_models(&ids, true);
        assert_eq!(out_false, out_true);
        assert_eq!(out_false.lines().count(), 50);
    }

    #[test]
    fn no_truncation_notice_when_nothing_cut() {
        let catalog = ModelCatalog {
            models: vec![entry("anthropic", "claude-opus-4-8")],
            fetched_at: SystemTime::now(),
        };
        let (_, truncation) = format_platform_models(&catalog, &["anthropic"], false);
        assert!(truncation.notice().is_none());
    }

    // ── CB37: platform-scoped uncapped, unfiltered still capped ──────

    /// T1 — platform-scoped call returns all models, no truncation notice.
    #[test]
    fn platform_scoped_returns_all_models_no_truncation() {
        let ids: Vec<String> = (0..30).map(|i| format!("opencode/model-{i}")).collect();
        let (out, truncation) = format_native_models(&ids, false);
        assert_eq!(out.lines().count(), 30, "all 30 models must appear");
        assert!(truncation.is_empty());
    }

    /// T2 — unfiltered listing still caps per provider.
    #[test]
    fn unfiltered_listing_still_capped() {
        let models: Vec<ModelEntry> = (0..20)
            .map(|i| entry("anthropic", &format!("claude-{i}")))
            .collect();
        let catalog = ModelCatalog {
            models,
            fetched_at: SystemTime::now(),
        };
        let (out, truncation) = format_catalog_models(&catalog, false);
        let anthropic_lines = out.lines().filter(|l| l.contains("Anthropic")).count();
        assert_eq!(anthropic_lines, MODELS_PER_PROVIDER);
        assert!(!truncation.is_empty());
    }

    /// T3 — truncation notice carries the ordering rule.
    #[test]
    fn truncation_notice_includes_ordering_rule() {
        let models: Vec<ModelEntry> = (0..20)
            .map(|i| entry("anthropic", &format!("claude-{i}")))
            .collect();
        let catalog = ModelCatalog {
            models,
            fetched_at: SystemTime::now(),
        };
        let (_, truncation) = format_catalog_models(&catalog, false);
        let notice = truncation.notice().expect("should be truncated");
        assert!(
            notice.contains("newest first by release date"),
            "notice must state ordering rule: {notice}"
        );
    }

    /// T4 — platform-scoped ordering is deterministic across calls.
    #[test]
    fn platform_scoped_ordering_is_deterministic() {
        let ids: Vec<String> = (0..20).map(|i| format!("opencode/model-{i}")).collect();
        let (out1, _) = format_native_models(&ids, false);
        let (out2, _) = format_native_models(&ids, false);
        assert_eq!(
            out1, out2,
            "two consecutive calls must produce identical output"
        );
    }
}
