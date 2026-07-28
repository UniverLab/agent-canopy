//! Focus panel for background agents (`AgentEntry::Agent`) — a fixed
//! metadata header on top and a scrollable, color-coded log below.

use super::details::agent_status;
use super::log_fallback::log_scrollbar_geometry;
use crate::domain::models::{Agent, Trigger};
use crate::tui::app::types::App;
use crate::tui::app::utils::relative_time;
use crate::tui::ui::theme::Theme;
use crate::tui::ui::ERROR_COLOR;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Paragraph, Scrollbar, ScrollbarOrientation, ScrollbarState, Wrap};
use ratatui::Frame;

const METADATA_HEIGHT: u16 = 6;
const WARN_COLOR: Color = Color::Rgb(255, 193, 7);

pub fn draw_background_agent_panel(
    frame: &mut Frame,
    area: Rect,
    agent: &Agent,
    app: &App,
    theme: &Theme,
) {
    if area.width == 0 || area.height == 0 {
        return;
    }

    let metadata_height = METADATA_HEIGHT.min(area.height);
    let chunks =
        Layout::vertical([Constraint::Length(metadata_height), Constraint::Min(0)]).split(area);

    let has_active_run = app.active_runs.contains_key(&agent.id);
    let metadata_lines = background_agent_metadata_lines(agent, has_active_run, theme);
    frame.render_widget(
        Paragraph::new(metadata_lines).wrap(Wrap { trim: false }),
        chunks[0],
    );

    draw_background_agent_log(frame, chunks[1], app, theme);
}

fn trigger_summary(agent: &Agent) -> String {
    match &agent.trigger {
        Some(Trigger::Cron { schedule_expr }) => format!("cron ({schedule_expr})"),
        Some(Trigger::Watch { path, .. }) => format!("watch ({path})"),
        None => "manual".to_string(),
    }
}

/// Builds the fixed-size metadata block for a background agent. Pure
/// function over `Agent` — no DB or live App state required, so it stays
/// cheap to unit test.
fn background_agent_metadata_lines(
    agent: &Agent,
    has_active_run: bool,
    theme: &Theme,
) -> Vec<Line<'static>> {
    let (status_text, status_color) = agent_status(agent, has_active_run);
    let model = agent.model.clone().unwrap_or_else(|| "-".to_string());
    let last_run = agent
        .last_run_at
        .map(|dt| relative_time(&dt))
        .unwrap_or_else(|| "never".to_string());

    vec![
        Line::from(vec![
            Span::styled("Status:  ", Style::default().fg(theme.dim_text)),
            Span::styled(status_text, Style::default().fg(status_color)),
        ]),
        Line::from(vec![
            Span::styled("CLI:     ", Style::default().fg(theme.dim_text)),
            Span::raw(agent.cli.as_str().to_string()),
        ]),
        Line::from(vec![
            Span::styled("Model:   ", Style::default().fg(theme.dim_text)),
            Span::raw(model),
        ]),
        Line::from(vec![
            Span::styled("Last run:", Style::default().fg(theme.dim_text)),
            Span::raw(format!(" {last_run}")),
        ]),
        Line::from(vec![
            Span::styled("Trigger: ", Style::default().fg(theme.dim_text)),
            Span::raw(trigger_summary(agent)),
        ]),
        Line::from(""),
    ]
}

/// Colors a single raw log line: ERROR in red, WARN in amber, the
/// `--- ... ---` run-header lines (which carry the timestamp) in gray,
/// everything else in white.
fn classify_log_line(line: &str, theme: &Theme) -> Style {
    let upper = line.to_ascii_uppercase();
    if upper.contains("ERROR") {
        Style::default().fg(ERROR_COLOR)
    } else if upper.contains("WARN") {
        Style::default().fg(WARN_COLOR)
    } else if line.starts_with("---") {
        Style::default().fg(theme.dim_text)
    } else {
        Style::default().fg(Color::White)
    }
}

fn colorize_log_line(line: &str, theme: &Theme) -> Line<'static> {
    Line::from(Span::styled(
        line.to_string(),
        classify_log_line(line, theme),
    ))
}

fn draw_background_agent_log(frame: &mut Frame, area: Rect, app: &App, theme: &Theme) {
    if area.width == 0 || area.height == 0 {
        return;
    }

    let lines: Vec<Line<'static>> = app
        .log_content
        .lines()
        .map(|line| colorize_log_line(line, theme))
        .collect();
    let line_count = lines.len() as u16;
    let max_scroll = line_count.saturating_sub(area.height);
    let scroll = app.log_scroll.min(max_scroll);

    let paragraph = Paragraph::new(Text::from(lines))
        .wrap(Wrap { trim: false })
        .scroll((scroll, 0));
    frame.render_widget(paragraph, area);

    if let Some((content_length, position, viewport)) =
        log_scrollbar_geometry(line_count, area.height, app.log_scroll)
    {
        let mut scrollbar_state = ScrollbarState::new(content_length)
            .position(position)
            .viewport_content_length(viewport);
        frame.render_stateful_widget(
            Scrollbar::default().orientation(ScrollbarOrientation::VerticalRight),
            area,
            &mut scrollbar_state,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::models::Cli;
    use chrono::Utc;

    fn sample_agent() -> Agent {
        Agent {
            id: "agent-1".to_string(),
            prompt: "Run tests".to_string(),
            trigger: Some(Trigger::Cron {
                schedule_expr: "*/5 * * * *".to_string(),
            }),
            cli: Cli::new("opencode"),
            model: Some("gpt-5".to_string()),
            working_dir: Some("/tmp/project".to_string()),
            enabled: true,
            enable_at: None,
            created_at: Utc::now(),
            log_path: "/tmp/agent-1.log".to_string(),
            timeout_minutes: 15,
            expires_at: None,
            last_run_at: None,
            last_run_ok: None,
            last_triggered_at: None,
            trigger_count: 0,
        }
    }

    #[test]
    fn metadata_block_is_six_fixed_lines() {
        let agent = sample_agent();
        let lines = background_agent_metadata_lines(&agent, false, &Theme::classic());
        assert_eq!(lines.len(), METADATA_HEIGHT as usize);
    }

    #[test]
    fn metadata_block_reflects_idle_status_without_db() {
        let agent = sample_agent();
        let lines = background_agent_metadata_lines(&agent, false, &Theme::classic());
        let status_line = lines[0]
            .spans
            .iter()
            .map(|s| s.content.as_ref())
            .collect::<String>();
        assert!(status_line.contains("IDLE"));
        assert!(status_line.contains("Status:"));
    }

    #[test]
    fn metadata_block_reflects_running_status_from_active_run_flag() {
        let agent = sample_agent();
        let lines = background_agent_metadata_lines(&agent, true, &Theme::classic());
        let status_line = lines[0]
            .spans
            .iter()
            .map(|s| s.content.as_ref())
            .collect::<String>();
        assert!(status_line.contains("RUNNING"));
    }

    #[test]
    fn metadata_block_shows_cli_model_and_trigger() {
        let agent = sample_agent();
        let lines = background_agent_metadata_lines(&agent, false, &Theme::classic());
        let cli_line = lines[1]
            .spans
            .iter()
            .map(|s| s.content.as_ref())
            .collect::<String>();
        let model_line = lines[2]
            .spans
            .iter()
            .map(|s| s.content.as_ref())
            .collect::<String>();
        let trigger_line = lines[4]
            .spans
            .iter()
            .map(|s| s.content.as_ref())
            .collect::<String>();
        assert!(cli_line.contains("opencode"));
        assert!(model_line.contains("gpt-5"));
        assert!(trigger_line.contains("cron"));
        assert!(trigger_line.contains("*/5 * * * *"));
    }

    #[test]
    fn color_coding_flags_error_lines_red() {
        let style = classify_log_line(
            "2024-01-01T00:00:00Z ERROR something broke",
            &Theme::classic(),
        );
        assert_eq!(style.fg, Some(ERROR_COLOR));
    }

    #[test]
    fn color_coding_flags_warn_lines_amber() {
        let style = classify_log_line(
            "2024-01-01T00:00:00Z WARN low disk space",
            &Theme::classic(),
        );
        assert_eq!(style.fg, Some(WARN_COLOR));
    }

    #[test]
    fn color_coding_flags_timestamp_headers_gray() {
        let style = classify_log_line(
            "--- [cron] agent-1 at 2024-01-01T00:00:00Z ---",
            &Theme::classic(),
        );
        assert_eq!(style.fg, Some(Theme::classic().dim_text));
    }

    #[test]
    fn color_coding_defaults_normal_lines_to_white() {
        let style = classify_log_line("plain informational output", &Theme::classic());
        assert_eq!(style.fg, Some(Color::White));
    }

    #[test]
    fn error_takes_priority_over_warn_when_both_present() {
        let style = classify_log_line("WARN escalated to ERROR", &Theme::classic());
        assert_eq!(style.fg, Some(ERROR_COLOR));
    }
}
