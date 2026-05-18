//! Footer rendering — context-sensitive key hints + version.

use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Frame;

use super::DIM;
use crate::tui::app::types::{AgentEntry, App, Focus};

pub(super) fn draw_footer(frame: &mut Frame, area: Rect, app: &App) {
    let activity_available = app.activity_panel_available();
    let hints = match app.focus {
        Focus::Home => {
            let mut h = vec![("↑↓", "select"), ("n", "new"), ("F2", "projects")];
            if activity_available {
                h.push(("F3", "activity"));
            }
            h.push(("Shift+←→", "panels"));
            h.push(("F10", "preview"));
            h.push(("F1", "stats"));
            h
        }
        Focus::Preview => {
            if app.sidebar_mode == crate::tui::app::SidebarMode::Projects {
                if app.playground_active {
                    vec![
                        ("type", "search"),
                        ("↑↓", "results"),
                        ("Enter", "search/open"),
                        ("Shift+↑↓", "agents"),
                        ("Ctrl+T", "transfer"),
                        ("Esc", "close"),
                        ("F2", "agents"),
                    ]
                } else {
                    let mut h = vec![
                        ("Tab", "section"),
                        ("↑↓", "nav"),
                        ("←→", "node"),
                        ("[ ]", "spec"),
                        ("Enter/e", "edit"),
                        ("F2", "agents"),
                    ];
                    if app.projects_panel_focus == crate::tui::app::ProjectsPanelFocus::RagInfo {
                        h.push(("p", "pause rag"));
                    }
                    h.push(("Esc", "home"));
                    h
                }
            } else {
                let is_bg = matches!(app.selected_agent(), Some(AgentEntry::Agent(_)));
                let mut h = vec![("↑↓", "nav"), ("Enter", "focus")];
                if is_bg {
                    h.push(("e", "edit"));
                    h.push(("d", "toggle"));
                    h.push(("F4", "delete"));
                    h.push(("r", "rerun"));
                }
                h.push(("n", "new"));
                h.push(("F2", "projects"));
                if activity_available {
                    h.push(("F3", "activity"));
                }
                h.push(("Esc", "home"));
                h
            }
        }
        Focus::NewAgentDialog => vec![
            ("↑↓", "fields"),
            ("←→", "cycle"),
            ("Space", "pick/enter"),
            ("Enter", "confirm"),
            ("Esc", "cancel"),
        ],
        Focus::LaunchpadDialog => vec![
            ("Tab/←→", "toggle"),
            ("type", "mission"),
            ("Enter", "confirm"),
            ("Esc", "cancel"),
        ],
        Focus::Agent => {
            if app.playground_active {
                return draw_footer_playground(frame, area, app, activity_available);
            }

            let is_pty = matches!(
                app.selected_agent(),
                Some(AgentEntry::Interactive(_))
                    | Some(AgentEntry::Terminal(_))
                    | Some(AgentEntry::Group(_))
            );
            let in_split = app.active_split_id.is_some();
            if is_pty {
                let mut h = vec![
                    ("F10", "preview"),
                    ("Esc", "home"),
                    ("Shift+↑↓", "agents/rag"),
                    ("Ctrl+T", "context"),
                ];
                if in_split {
                    h.push(("F4", "dissolve"));
                    h.push(("Shift+F4", "end"));
                    h.push(("Shift+←→", "split focus"));
                } else {
                    h.push(("F4", "end"));
                }
                if matches!(app.selected_agent(), Some(AgentEntry::Terminal(_))) {
                    h.push(("Tab", "catalog"));
                    h.push(("Ctrl+W", "wrap"));
                }
                if matches!(app.selected_agent(), Some(AgentEntry::Interactive(_))) {
                    h.push(("Ctrl+B", "prompt"));
                }
                h.push(("F2", "projects"));
                if activity_available {
                    h.push(("F3", "activity"));
                }
                h.push(("Ctrl+N", "new"));
                h.push(("F1", "legend"));
                h
            } else {
                let mut h = vec![("F10", "preview"), ("Esc", "home"), ("F2", "projects")];
                if activity_available {
                    h.push(("F3", "activity"));
                }
                h.push(("Ctrl+N", "new"));
                h.push(("F1", "legend"));
                h
            }
        }
        Focus::ContextTransfer => vec![
            ("↑↓", "select"),
            ("Tab/Enter", "next step"),
            ("Esc", "cancel"),
        ],
        Focus::RagTransfer => vec![("↑↓", "select"), ("Enter", "transfer"), ("Esc", "cancel")],
        Focus::PromptTemplateDialog => vec![
            ("↑↓", "fields"),
            ("⇧↑↓←→", "cursor"),
            ("Ctrl+S", "send"),
            ("Ctrl+A/X", "add/memory/remove"),
            ("Esc", "cancel"),
        ],
        Focus::WorkflowEditorDialog => vec![
            ("type", "edit"),
            ("←→", "cursor"),
            ("Enter", "newline"),
            ("Ctrl+S", "save"),
            ("Esc", "cancel"),
        ],
    };

    let mut spans = Vec::new();
    spans.push(Span::raw("  "));
    for (i, (key, desc)) in hints.iter().enumerate() {
        if i > 0 {
            spans.push(Span::raw("  "));
        }
        spans.push(Span::styled(
            *key,
            Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
        ));
        spans.push(Span::raw(" "));
        spans.push(Span::styled(*desc, Style::default().fg(DIM)));
    }

    // Show split session names when in split view
    let split_label = if let Some(ref split_id) = app.active_split_id {
        app.split_groups
            .iter()
            .find(|g| g.id == *split_id)
            .map(|g| {
                let left_marker = if app.split_right_focused { " " } else { "●" };
                let right_marker = if app.split_right_focused { "●" } else { " " };
                format!(
                    " {left_marker} {} │ {} {right_marker} ",
                    g.session_a, g.session_b
                )
            })
    } else {
        None
    };

    let version = if app.daemon_version.is_empty() {
        String::new()
    } else {
        format!(" v{} ", app.daemon_version)
    };

    let hints_line = Line::from(spans);
    let hints_p = Paragraph::new(hints_line);
    frame.render_widget(hints_p, area);

    // Render split label + version on the right side
    let right_text = match (&split_label, version.is_empty()) {
        (Some(sl), false) => format!("{sl}{version}"),
        (Some(sl), true) => sl.clone(),
        (None, false) => version.clone(),
        (None, true) => String::new(),
    };
    let right_w = right_text.len() as u16;

    if right_w > 0 && area.width > right_w {
        let right_area = Rect::new(area.x + area.width - right_w, area.y, right_w, 1);

        let mut right_spans = Vec::new();
        if let Some(ref sl) = split_label {
            right_spans.push(Span::styled(
                sl.as_str(),
                Style::default()
                    .fg(super::ACCENT)
                    .add_modifier(Modifier::BOLD),
            ));
        }
        if !version.is_empty() {
            right_spans.push(Span::styled(
                &version,
                Style::default().fg(DIM).add_modifier(Modifier::BOLD),
            ));
        }
        let right_p = Paragraph::new(Line::from(right_spans));
        frame.render_widget(right_p, right_area);
    }
}

fn draw_footer_playground(frame: &mut Frame, area: Rect, app: &App, activity_available: bool) {
    let mut hints = vec![
        ("type", "search"),
        ("↑↓", "results"),
        ("Enter", "search/open"),
        ("Shift+↑↓", "agents"),
        ("Ctrl+T", "transfer"),
        ("F10", "preview"),
        ("Esc", "close"),
        ("F2", "projects"),
    ];
    if activity_available {
        hints.push(("F3", "activity"));
    }

    let mut spans = Vec::new();
    spans.push(Span::raw("  "));
    for (i, (key, desc)) in hints.iter().enumerate() {
        if i > 0 {
            spans.push(Span::raw("  "));
        }
        spans.push(Span::styled(
            *key,
            Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
        ));
        spans.push(Span::raw(" "));
        spans.push(Span::styled(*desc, Style::default().fg(DIM)));
    }

    let version = if app.daemon_version.is_empty() {
        String::new()
    } else {
        format!(" v{} ", app.daemon_version)
    };

    let hints_line = Line::from(spans);
    frame.render_widget(Paragraph::new(hints_line), area);

    if !version.is_empty() && area.width > version.len() as u16 {
        let right_w = version.len() as u16;
        let right_area = Rect::new(area.x + area.width - right_w, area.y, right_w, 1);
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                version,
                Style::default().fg(DIM).add_modifier(Modifier::BOLD),
            ))),
            right_area,
        );
    }
}
