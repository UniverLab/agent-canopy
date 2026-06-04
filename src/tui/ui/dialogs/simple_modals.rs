use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph};
use ratatui::Frame;

use super::{centered_rect, ACCENT, DIM};
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
pub fn draw_legend(frame: &mut Frame, app: &mut App) {
    use crate::domain::gamification::{MissionCategory, MISSIONS};

    let label_style = Style::default().fg(DIM);
    let value_style = Style::default()
        .fg(Color::White)
        .add_modifier(Modifier::BOLD);
    let accent_style = Style::default().fg(ACCENT);

    let session_uptime = format_uptime_precise(app.process_start_time.elapsed().as_secs());
    let canopy_uptime = format_uptime_precise(app.cli_usage.canopy_uptime_seconds());
    let interactive_count = app.db.count_interactive_sessions().unwrap_or(0);
    let terminal_count = app.db.count_terminal_sessions().unwrap_or(0);
    let bg_count = app.db.count_background_agents().unwrap_or(0);
    let runs_count = app.db.count_runs().unwrap_or(0);

    let mut header_lines = vec![
        Line::from(vec![
            Span::styled("Session: ", label_style),
            Span::styled(&session_uptime, accent_style),
            Span::raw("   "),
            Span::styled("Canopy: ", label_style),
            Span::styled(&canopy_uptime, accent_style),
        ]),
        Line::from(""),
        Line::from(vec![
            Span::styled("Interactive: ", label_style),
            Span::styled(format!("{interactive_count}"), value_style),
            Span::raw("   "),
            Span::styled("Terminal: ", label_style),
            Span::styled(format!("{terminal_count}"), value_style),
            Span::raw("   "),
            Span::styled("BG: ", label_style),
            Span::styled(format!("{bg_count}"), value_style),
            Span::raw("   "),
            Span::styled("Runs: ", label_style),
            Span::styled(format!("{runs_count}"), value_style),
        ]),
        Line::from(""),
    ];

    let top_clis = app.cli_usage.ranked();
    if !top_clis.is_empty() {
        let clis: Vec<String> = top_clis
            .iter()
            .take(3)
            .map(|(name, count)| format!("{name}({count})"))
            .collect();
        header_lines.push(Line::from(vec![
            Span::styled("Harnesses: ", label_style),
            Span::styled(clis.join("  "), value_style),
        ]));
        header_lines.push(Line::from(""));
    }

    let total = MISSIONS.len();
    let unlocked_n = app.mission_manager.unlocked_count();
    let visible_rows = 6u16;
    let scroll = app.legend_scroll.min(total.saturating_sub(1) as u16);
    app.legend_scroll = scroll;

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

    let category_label = |cat: &MissionCategory| -> &'static str {
        match cat {
            MissionCategory::Environment => "Environment",
            MissionCategory::Intelligence => "Intelligence",
            MissionCategory::Projects => "Projects",
            MissionCategory::Workflow => "Workflow",
            MissionCategory::Seeds => "Seeds",
            MissionCategory::SysInfo => "System",
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
                let min_brightness = 0.4;
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

    let mut medal_lines = vec![
        Line::from(""),
        Line::from(vec![
            Span::styled(
                " ✦ ",
                Style::default()
                    .fg(ACCENT)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                "Missions",
                Style::default()
                    .fg(Color::White)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!("  {unlocked_n}/{total}"),
                Style::default().fg(DIM),
            ),
        ]),
        Line::from(""),
    ];

    let mut visible_idx = 0usize;
    for (i, def) in MISSIONS.iter().enumerate() {
        if (i as u16) < scroll || (i as u16) >= scroll + visible_rows {
            continue;
        }
        let unlocked = app.mission_manager.is_unlocked(def.id);
        let base_color = category_color(&def.category);

        if unlocked {
            let twinkle_factor = twinkle(visible_idx);
            let icon_color = dim_color(base_color, twinkle_factor);
            let icon_style = Style::default()
                .fg(icon_color)
                .add_modifier(Modifier::BOLD);

            medal_lines.push(Line::from(vec![
                Span::styled(format!("  {} ", def.icon), icon_style),
                Span::styled(def.title, Style::default().fg(Color::White)),
                Span::styled(
                    format!("  · {}", category_label(&def.category)),
                    Style::default().fg(DIM),
                ),
            ]));
        } else {
            medal_lines.push(Line::from(vec![
                Span::styled("  · ", Style::default().fg(Color::Rgb(50, 50, 50))),
                Span::styled(def.title, Style::default().fg(Color::Rgb(70, 70, 70))),
                Span::styled(
                    format!("  · {}", category_label(&def.category)),
                    Style::default().fg(Color::Rgb(50, 50, 50)),
                ),
            ]));
        }

        visible_idx += 1;
        medal_lines.push(Line::from(""));
    }

    let scroll_indicator = if total > visible_rows as usize {
        let pct = if total <= 1 {
            0
        } else {
            scroll as usize * 100 / (total - visible_rows as usize).max(1)
        };
        format!(" {}% ", pct)
    } else {
        String::new()
    };

    let footer_lines = vec![
        Line::from(""),
        Line::from(vec![
            Span::styled(" F1/Esc ", label_style),
            Span::styled("close   ", Style::default().fg(Color::White)),
            Span::styled("↑↓/jk ", label_style),
            Span::styled("scroll   ", Style::default().fg(Color::White)),
            Span::styled("⊞ ", label_style),
            Span::styled("mouse wheel", Style::default().fg(Color::White)),
            if scroll_indicator.is_empty() {
                Span::raw("")
            } else {
                Span::styled(scroll_indicator, accent_style)
            },
        ]),
    ];

    let all_lines: Vec<Line> = header_lines
        .into_iter()
        .chain(medal_lines)
        .chain(footer_lines)
        .collect();

    let content_height = all_lines.len() as u16 + 2;
    let width = 52u16;
    let height = content_height.clamp(16, 36);
    let percent_x = (width * 100 / frame.area().width.max(1)).clamp(35, 65);
    let area = centered_rect(percent_x, height, frame.area());
    frame.render_widget(Clear, area);

    let block = Block::default()
        .title(" Canopy Stats ")
        .borders(Borders::ALL)
        .border_style(Style::default().fg(ACCENT))
        .style(Style::default().bg(Color::Rgb(12, 20, 12)));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    frame.render_widget(
        Paragraph::new(all_lines).alignment(ratatui::layout::Alignment::Left),
        inner,
    );
}
