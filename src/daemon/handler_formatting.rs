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

/// Newest models listed per provider in `agent_models`.
const MODELS_PER_PROVIDER: usize = 8;

/// Cap on how many providers a single (platform-scoped) listing renders, so a
/// universal-gateway platform mapped to many providers still can't blow past
/// MCP result size limits: at most `MAX_PROVIDERS * MODELS_PER_PROVIDER` lines.
const MAX_PROVIDERS: usize = 12;

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

/// Format the cached models.dev catalog: newest models per major provider.
pub(crate) fn format_catalog_models(catalog: &crate::domain::models_db::ModelCatalog) -> String {
    let providers: Vec<(&str, String)> = MODEL_PROVIDERS
        .iter()
        .map(|(slug, display)| (*slug, (*display).to_string()))
        .collect();
    format_models_for_providers(catalog, &providers)
}

/// Format only the models available to `provider_slugs` (a platform's mapped
/// providers), newest-first per provider, bounded by [`MAX_PROVIDERS`].
pub(crate) fn format_platform_models(
    catalog: &crate::domain::models_db::ModelCatalog,
    provider_slugs: &[&str],
) -> String {
    let providers: Vec<(&str, String)> = provider_slugs
        .iter()
        .map(|slug| (*slug, provider_display(slug)))
        .collect();
    format_models_for_providers(catalog, &providers)
}

/// Format a platform's native enumeration (e.g. `opencode models`): the ids are
/// already the literal, passable strings the CLI accepts (`opencode/big-pickle`),
/// so they are rendered verbatim — never re-derived — grouped by their provider
/// prefix (the segment before the first `/`) for readability and bounded by the
/// same [`MAX_PROVIDERS`] x [`MODELS_PER_PROVIDER`] caps as the models.dev path.
/// The id is always the first token on the line; the parenthetical is only a
/// human label, so a caller copying the id verbatim always succeeds.
pub(crate) fn format_native_models(ids: &[String]) -> String {
    // Group by provider prefix, preserving first-seen order.
    let mut groups: Vec<(String, Vec<&String>)> = Vec::new();
    for id in ids {
        let provider = id.split_once('/').map(|(p, _)| p).unwrap_or("");
        match groups.iter_mut().find(|(p, _)| p == provider) {
            Some((_, models)) => models.push(id),
            None => groups.push((provider.to_string(), vec![id])),
        }
    }

    groups
        .iter()
        .take(MAX_PROVIDERS)
        .map(|(provider, models)| {
            let display = if provider.is_empty() {
                "native".to_string()
            } else {
                provider_display(provider)
            };
            models
                .iter()
                .take(MODELS_PER_PROVIDER)
                .map(|id| format!("  {id}  ({display})"))
                .collect::<Vec<_>>()
                .join("\n")
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Shared renderer: newest [`MODELS_PER_PROVIDER`] models for each listed
/// provider that has any, skipping empty providers, capped at [`MAX_PROVIDERS`]
/// non-empty providers.
fn format_models_for_providers(
    catalog: &crate::domain::models_db::ModelCatalog,
    providers: &[(&str, String)],
) -> String {
    let mut sections = Vec::new();

    for (slug, display) in providers {
        if sections.len() >= MAX_PROVIDERS {
            break;
        }
        let mut models: Vec<_> = catalog
            .models
            .iter()
            .filter(|m| m.provider == *slug)
            .collect();
        if models.is_empty() {
            continue;
        }
        // ISO release dates sort lexically; undated models go last.
        models.sort_by(|a, b| b.release_date.cmp(&a.release_date));

        let lines = models
            .iter()
            .take(MODELS_PER_PROVIDER)
            .map(|m| format!("  {}  ({display})", m.id))
            .collect::<Vec<_>>()
            .join("\n");
        sections.push(lines);
    }

    sections.join("\n")
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
        let out = format_native_models(&ids);
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
        let out = format_platform_models(&catalog, &["anthropic"]);
        assert!(out.contains("claude-opus-4-8"));
        assert!(
            !out.contains("anthropic/claude-opus-4-8"),
            "claude ids must stay bare (no provider prefix): {out}"
        );
    }

    #[test]
    fn native_listing_is_bounded() {
        // Far more than the caps allow; output must stay bounded.
        let ids: Vec<String> = (0..50)
            .map(|i| format!("nvidia/model-{i}"))
            .chain((0..50).map(|i| format!("opencode/zen-{i}")))
            .collect();
        let out = format_native_models(&ids);
        assert!(out.lines().count() <= MAX_PROVIDERS * MODELS_PER_PROVIDER);
    }
}
