use ratatui::layout::{Alignment, Constraint, Layout};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph};
use ratatui::Frame;

use super::{centered_rect, draw_dialog_left_wave, ACCENT, DIM};
use crate::tui::app::types::App;

pub fn draw_quit_confirm(frame: &mut Frame) {
    draw_modal_confirm(frame, " Quit? ", "Press y/Enter to quit, any key to cancel");
}

pub fn draw_delete_project_confirm(frame: &mut Frame) {
    draw_modal_confirm(
        frame,
        " Delete Project? ",
        "Are you sure you want to delete this project?\nY/Enter = Confirm  N/Esc = Cancel",
    );
}

pub fn draw_delete_workflow_confirm(frame: &mut Frame) {
    draw_modal_confirm(
        frame,
        " Delete Workflow? ",
        "Are you sure you want to delete this workflow?\nY/Enter = Confirm  N/Esc = Cancel",
    );
}

fn draw_modal_confirm(frame: &mut Frame, title: &str, text: &str) {
    let dialog_width = frame.area().width * 40 / 100;
    let inner_width = dialog_width.saturating_sub(2).max(1);
    let chars_per_line = inner_width as usize;
    let needed_lines = text
        .split('\n')
        .map(|line| (line.len().div_ceil(chars_per_line)).max(1) as u16)
        .sum::<u16>();
    let height = needed_lines + 2; // +2 for borders

    let area = centered_rect(40, height, frame.area());
    frame.render_widget(Clear, area);

    let block = Block::default()
        .title(title)
        .borders(Borders::ALL)
        .border_style(Style::default().fg(ACCENT))
        .style(Style::default().bg(Color::Rgb(15, 25, 15)));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let msg = Paragraph::new(text)
        .style(Style::default().fg(ACCENT))
        .alignment(ratatui::layout::Alignment::Center)
        .wrap(ratatui::widgets::Wrap { trim: true });
    frame.render_widget(msg, inner);
}

fn format_uptime_precise(seconds: u64) -> String {
    let days = seconds / 86_400;
    let hours = (seconds % 86_400) / 3_600;
    let mins = (seconds % 3_600) / 60;
    let secs = seconds % 60;
    match (days, hours, mins) {
        (0, 0, 0) => format!("{secs}s"),
        (0, 0, m) => format!("{m}m {secs}s"),
        (0, h, m) => format!("{h}h {m}m {secs}s"),
        (d, h, m) => format!("{d}d {h}h {m}m {secs}s"),
    }
}

fn category_label(category: &crate::domain::gamification::MissionCategory) -> &'static str {
    use crate::domain::gamification::MissionCategory;
    match category {
        MissionCategory::Environment => "Environment",
        MissionCategory::Intelligence => "Intelligence",
        MissionCategory::Projects => "Projects",
        MissionCategory::Workflow => "Workflow",
        MissionCategory::Seeds => "Seeds",
        MissionCategory::SysInfo => "System",
    }
}

fn mission_unlock_text(
    icon: &str,
    title: &str,
    category: &crate::domain::gamification::MissionCategory,
) -> String {
    format!(
        "Unlocked medal {icon} {title} in {}.",
        category_label(category)
    )
}

pub fn draw_legend(frame: &mut Frame, app: &mut App) {
    use crate::domain::gamification::{MissionCategory, MISSIONS};

    let label_style = Style::default().fg(DIM);
    let value_style = Style::default()
        .fg(Color::White)
        .add_modifier(Modifier::BOLD);
    let accent_style = Style::default().fg(ACCENT);

    let session_uptime = format_uptime_precise(app.process_start_time.elapsed().as_secs());
    let canopy_uptime = format_uptime_precise(app.accumulated_uptime_secs());
    let interactive_count = app.db.count_interactive_sessions().unwrap_or(0);
    let terminal_count = app.db.count_terminal_sessions().unwrap_or(0);
    let bg_count = app.db.count_background_agents().unwrap_or(0);
    let runs_count = app.db.count_runs().unwrap_or(0);

    let total = MISSIONS.len();
    let unlocked_n = app.mission_manager.unlocked_count();

    let unlocked_missions: Vec<_> = MISSIONS
        .iter()
        .filter(|def| app.mission_manager.is_unlocked(def.id))
        .collect();

    let max_selected = unlocked_missions.len().saturating_sub(1);
    if app.legend_selected > max_selected {
        app.legend_selected = max_selected;
    }

    let selected = app.legend_selected;

    let base_height = 2 + 4 + 1; // summary + usage + instructions
    let extra_height = if unlocked_missions.is_empty() {
        0
    } else {
        1 + unlocked_missions.len().min(15) as u16 + 1 + 3 // spacers + missions + spacer + detail
    };
    let percent_x = 52u16;
    let height =
        (base_height + extra_height + 2).min(frame.area().height.saturating_sub(2).max(12));
    let area = centered_rect(percent_x, height, frame.area());
    frame.render_widget(Clear, area);
    draw_dialog_left_wave(frame, area, app.animation_tick.into());

    let block = Block::default()
        .title(" Canopy Missions ")
        .borders(Borders::ALL)
        .border_style(Style::default().fg(ACCENT))
        .style(Style::default().bg(Color::Rgb(12, 20, 12)));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let category_color = |cat: &MissionCategory| -> Color {
        match cat {
            MissionCategory::Environment => Color::Rgb(100, 220, 100),
            MissionCategory::Intelligence => Color::Rgb(100, 180, 255),
            MissionCategory::Projects => Color::Rgb(255, 200, 80),
            MissionCategory::Workflow => Color::Rgb(220, 120, 255),
            MissionCategory::Seeds => Color::Rgb(80, 220, 180),
            MissionCategory::SysInfo => Color::Rgb(255, 130, 80),
        }
    };

    let now_ms = chrono::Utc::now().timestamp_millis() as f64;
    let twinkle = |offset: usize| -> f64 {
        let phase = (now_ms / 1500.0) + (offset as f64 * 0.7);
        (phase.sin() + 1.0) / 2.0
    };

    let dim_color = |base: Color, factor: f64| -> Color {
        match base {
            Color::Rgb(r, g, b) => {
                let min_brightness = 0.35;
                let f = min_brightness + (1.0 - min_brightness) * factor;
                Color::Rgb(
                    (r as f64 * f) as u8,
                    (g as f64 * f) as u8,
                    (b as f64 * f) as u8,
                )
            }
            _ => base,
        }
    };

    let sections = Layout::vertical([
        Constraint::Length(2),
        Constraint::Length(if unlocked_missions.is_empty() { 0 } else { 1 }),
        Constraint::Length(if unlocked_missions.is_empty() {
            0
        } else {
            unlocked_missions.len().min(15) as u16
        }),
        Constraint::Length(if unlocked_missions.is_empty() { 0 } else { 1 }),
        Constraint::Length(if unlocked_missions.is_empty() { 0 } else { 3 }),
        Constraint::Length(4),
        Constraint::Length(1),
    ])
    .split(inner);

    let summary_lines = vec![
        Line::from(vec![
            Span::styled("Missions ", value_style),
            Span::styled(format!("{unlocked_n}/{total}"), accent_style),
            Span::raw("    "),
            Span::styled("Session ", label_style),
            Span::styled(&session_uptime, accent_style),
            Span::raw("    "),
            Span::styled("Canopy ", label_style),
            Span::styled(&canopy_uptime, accent_style),
        ]),
        Line::from(vec![
            Span::styled("Interactive ", label_style),
            Span::styled(format!("{interactive_count}"), value_style),
            Span::raw("  "),
            Span::styled("Terminal ", label_style),
            Span::styled(format!("{terminal_count}"), value_style),
            Span::raw("  "),
            Span::styled("BG ", label_style),
            Span::styled(format!("{bg_count}"), value_style),
            Span::raw("  "),
            Span::styled("Runs ", label_style),
            Span::styled(format!("{runs_count}"), value_style),
        ]),
    ];
    frame.render_widget(
        Paragraph::new(summary_lines).alignment(Alignment::Center),
        sections[0],
    );

    let mut mission_lines: Vec<Line> = Vec::new();
    if !unlocked_missions.is_empty() {
        let visible_rows = sections[2].height as usize;
        let start = selected.saturating_sub(visible_rows.saturating_sub(1) / 2);
        let start = start.min(unlocked_missions.len().saturating_sub(visible_rows));
        let end = (start + visible_rows).min(unlocked_missions.len());
        for (i, def) in unlocked_missions[start..end].iter().enumerate() {
            let mission_index = start + i;
            let is_selected = mission_index == selected;
            let base_color = category_color(&def.category);
            let icon_color = dim_color(base_color, twinkle(i));
            let marker = if is_selected { "▶" } else { "·" };
            let title_style = if is_selected {
                Style::default()
                    .fg(Color::White)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(Color::Rgb(200, 200, 200))
            };
            mission_lines.push(Line::from(vec![
                Span::styled(marker, Style::default().fg(ACCENT)),
                Span::raw(" "),
                Span::styled(
                    def.icon,
                    Style::default().fg(icon_color).add_modifier(Modifier::BOLD),
                ),
                Span::raw(" "),
                Span::styled(def.title, title_style),
            ]));
        }
    }
    frame.render_widget(
        Paragraph::new(mission_lines).alignment(Alignment::Center),
        sections[2],
    );

    let selected_detail_lines = if let Some(selected_def) = unlocked_missions.get(selected) {
        let date_str = app
            .mission_manager
            .unlock_timestamp(selected_def.id)
            .and_then(|ts| chrono::DateTime::from_timestamp(ts, 0))
            .map(|dt| dt.format("%Y-%m-%d").to_string())
            .unwrap_or_else(|| "Unknown".to_string());

        vec![
            Line::from(Span::styled(
                selected_def.challenge,
                Style::default().fg(Color::Rgb(190, 190, 190)),
            )),
            Line::from(Span::styled(
                mission_unlock_text(
                    selected_def.icon,
                    selected_def.title,
                    &selected_def.category,
                ),
                Style::default().fg(Color::Rgb(140, 205, 140)),
            )),
            Line::from(Span::styled(
                format!("Completed: {}", date_str),
                Style::default().fg(Color::Rgb(100, 100, 100)),
            )),
        ]
    } else {
        vec![]
    };
    frame.render_widget(
        Paragraph::new(selected_detail_lines).alignment(Alignment::Center),
        sections[4],
    );

    let mut usage_pairs: Vec<_> = app.cli_usage.counts.iter().collect();
    usage_pairs.sort_by(|(name_a, count_a), (name_b, count_b)| {
        count_b.cmp(count_a).then_with(|| name_a.cmp(name_b))
    });
    let used_harnesses = usage_pairs.iter().filter(|(_, count)| **count > 0).count();
    let usage_lines = if usage_pairs.is_empty() {
        vec![Line::from(Span::styled(
            "No harness usage yet.",
            Style::default().fg(Color::White),
        ))]
    } else {
        let visible_rows = sections[5].height.saturating_sub(1) as usize;
        usage_pairs
            .iter()
            .take(visible_rows.max(1))
            .map(|(name, count)| {
                Line::from(Span::styled(
                    format!("{name}: {count}"),
                    Style::default().fg(Color::White),
                ))
            })
            .collect::<Vec<_>>()
    };
    let mut usage_panel_lines = vec![Line::from(Span::styled(
        format!("Harnesses used: {used_harnesses}"),
        Style::default().fg(Color::Rgb(210, 210, 210)),
    ))];
    usage_panel_lines.extend(usage_lines);
    frame.render_widget(
        Paragraph::new(usage_panel_lines).alignment(Alignment::Center),
        sections[5],
    );

    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled("F1/Esc ", label_style),
            Span::styled("close   ", Style::default().fg(Color::White)),
            Span::styled("↑↓/jk ", label_style),
            Span::styled("select", Style::default().fg(Color::White)),
        ]))
        .alignment(Alignment::Center),
        sections[6],
    );
}
