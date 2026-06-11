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

/// Format the cached models.dev catalog: newest models per major provider.
pub(crate) fn format_catalog_models(catalog: &crate::domain::models_db::ModelCatalog) -> String {
    let mut sections = Vec::new();

    for (slug, display) in MODEL_PROVIDERS {
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

/// Static list used when the models.dev catalog is unavailable.
pub(crate) fn format_fallback_models() -> String {
    let models = [
        ("Anthropic", "claude-fable-5"),
        ("Anthropic", "claude-opus-4-8"),
        ("Anthropic", "claude-sonnet-4-6"),
        ("Anthropic", "claude-haiku-4-5"),
        ("OpenAI", "gpt-4.1"),
        ("OpenAI", "o3"),
        ("OpenAI", "o4-mini"),
        ("Google", "gemini-2.5-pro"),
        ("Google", "gemini-2.5-flash"),
        ("Mistral", "mistral-large-latest"),
        ("Amazon", "nova-pro"),
    ];

    models
        .iter()
        .map(|(provider, model)| format!("  {model}  ({provider})"))
        .collect::<Vec<_>>()
        .join("\n")
}
