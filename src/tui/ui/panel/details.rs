use super::{DIM, INTERACTIVE_COLOR, STATUS_DISABLED, STATUS_FAIL, STATUS_OK, STATUS_RUNNING};
use crate::domain::models::{Agent, Trigger};
use crate::tui::app::types::App;
use crate::tui::app::utils::relative_time;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Wrap};
use ratatui::Frame;

pub fn draw_group_details(frame: &mut Frame, area: Rect, app: &App, group_idx: usize) {
    let Some(group) = app.split_groups.get(group_idx) else {
        return;
    };

    let is_active = app
        .active_split_id
        .as_deref()
        .is_some_and(|id| id == group.id);

    let header_color = if is_active {
        Color::Green
    } else {
        Color::Rgb(150, 150, 200)
    };

    let mut lines = vec![
        Line::from(""),
        Line::from(vec![
            Span::styled(
                "  Split Group  ",
                Style::default()
                    .fg(header_color)
                    .add_modifier(Modifier::BOLD),
            ),
            if is_active {
                Span::styled("● active", Style::default().fg(Color::Green))
            } else {
                Span::styled("○ inactive", Style::default().fg(DIM))
            },
        ]),
        Line::from(""),
        Line::from(vec![
            Span::styled("  Session A:  ", Style::default().fg(DIM)),
            Span::styled(
                &group.session_a,
                Style::default()
                    .fg(INTERACTIVE_COLOR)
                    .add_modifier(Modifier::BOLD),
            ),
        ]),
        Line::from(vec![
            Span::styled("  Session B:  ", Style::default().fg(DIM)),
            Span::styled(
                &group.session_b,
                Style::default()
                    .fg(INTERACTIVE_COLOR)
                    .add_modifier(Modifier::BOLD),
            ),
        ]),
        Line::from(""),
        Line::from(vec![
            Span::styled("  Orientation: ", Style::default().fg(DIM)),
            Span::styled(
                group.orientation.as_str(),
                Style::default().fg(Color::White),
            ),
        ]),
        Line::from(""),
        Line::from(Span::styled(
            if is_active {
                "  F4 to dissolve  ·  Shift+F4 to end  ·  Ctrl+←/→ switch panel"
            } else {
                "  Enter to activate split  ·  D to dissolve"
            },
            Style::default().fg(DIM),
        )),
    ];

    // Show whether sessions still exist
    let session_a_exists = app
        .interactive_agents
        .iter()
        .any(|a| a.name == group.session_a)
        || app
            .terminal_agents
            .iter()
            .any(|a| a.name == group.session_a);
    let session_b_exists = app
        .interactive_agents
        .iter()
        .any(|a| a.name == group.session_b)
        || app
            .terminal_agents
            .iter()
            .any(|a| a.name == group.session_b);

    if !session_a_exists || !session_b_exists {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            "  ⚠ one or more sessions no longer exist",
            Style::default().fg(Color::Yellow),
        )));
    }

    frame.render_widget(Paragraph::new(lines), area);
}

pub fn draw_agent_details(frame: &mut Frame, area: Rect, agent: &Agent, app: &App) {
    let has_active = app.active_runs.contains_key(&agent.id);
    let (status_text, status_color) = if !agent.enabled {
        ("DISABLED", STATUS_DISABLED)
    } else if has_active {
        ("RUNNING", STATUS_RUNNING)
    } else if agent.last_run_ok == Some(false) {
        ("FAILED", STATUS_FAIL)
    } else if agent.last_run_ok == Some(true) {
        ("OK", STATUS_OK)
    } else {
        ("IDLE", STATUS_OK)
    };

    let mut lines = vec![
        Line::from(vec![
            Span::styled("Status:  ", Style::default().fg(DIM)),
            Span::styled(status_text, Style::default().fg(status_color)),
        ]),
        Line::from(""),
        Line::from(vec![
            Span::styled("Type:    ", Style::default().fg(DIM)),
            Span::styled(
                agent.trigger_type_label(),
                Style::default().fg(INTERACTIVE_COLOR),
            ),
        ]),
        Line::from(vec![
            Span::styled("Prompt:  ", Style::default().fg(DIM)),
            Span::raw(&agent.prompt),
        ]),
    ];

    match &agent.trigger {
        Some(Trigger::Cron { schedule_expr }) => {
            lines.push(Line::from(vec![
                Span::styled("Cron:    ", Style::default().fg(DIM)),
                Span::styled(schedule_expr, Style::default().fg(INTERACTIVE_COLOR)),
            ]));
        }
        Some(Trigger::Watch {
            path,
            events,
            debounce_seconds,
            recursive,
            ..
        }) => {
            lines.push(Line::from(""));
            lines.push(Line::from(vec![
                Span::styled("Path:    ", Style::default().fg(DIM)),
                Span::raw(path),
            ]));
            lines.push(Line::from(vec![
                Span::styled("Events:  ", Style::default().fg(DIM)),
                Span::raw(
                    events
                        .iter()
                        .map(|e| e.to_string())
                        .collect::<Vec<_>>()
                        .join(", "),
                ),
            ]));
            lines.push(Line::from(vec![
                Span::styled("Debounce:", Style::default().fg(DIM)),
                Span::raw(format!(" {}s", debounce_seconds)),
            ]));
            lines.push(Line::from(vec![
                Span::styled("Recursive:", Style::default().fg(DIM)),
                Span::raw(if *recursive { " yes" } else { " no" }),
            ]));
        }
        None => {}
    }

    lines.push(Line::from(vec![
        Span::styled("CLI:     ", Style::default().fg(DIM)),
        Span::raw(agent.cli.as_str()),
    ]));

    if let Some(ref model) = agent.model {
        lines.push(Line::from(vec![
            Span::styled("Model:   ", Style::default().fg(DIM)),
            Span::raw(model),
        ]));
    }

    if let Some(ref dir) = agent.working_dir {
        lines.push(Line::from(vec![
            Span::styled("Dir:     ", Style::default().fg(DIM)),
            Span::raw(dir),
        ]));
    }

    lines.push(Line::from(vec![
        Span::styled("Timeout: ", Style::default().fg(DIM)),
        Span::raw(format!("{} min", agent.timeout_minutes)),
    ]));

    if let Some(ref exp) = agent.expires_at {
        lines.push(Line::from(vec![
            Span::styled("Expires: ", Style::default().fg(DIM)),
            Span::raw(relative_time(exp)),
        ]));
    }

    if let Some(ref lr) = agent.last_run_at {
        lines.push(Line::from(vec![
            Span::styled("Last run:", Style::default().fg(DIM)),
            Span::raw(relative_time(lr)),
        ]));
    }

    if agent.trigger_count > 0 {
        lines.push(Line::from(vec![
            Span::styled("Triggers:", Style::default().fg(DIM)),
            Span::raw(agent.trigger_count.to_string()),
        ]));
    }

    let paragraph = Paragraph::new(lines).wrap(Wrap { trim: false });
    frame.render_widget(paragraph, area);
}
