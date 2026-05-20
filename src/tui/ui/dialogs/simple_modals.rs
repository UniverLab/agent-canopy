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
pub fn draw_legend(frame: &mut Frame, app: &App) {
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

    let mut lines = vec![
        Line::from(""),
        Line::from(vec![
            Span::styled("Session uptime: ", label_style),
            Span::styled(&session_uptime, accent_style),
        ]),
        Line::from(vec![
            Span::styled("Canopy uptime:  ", label_style),
            Span::styled(&canopy_uptime, accent_style),
        ]),
        Line::from(""),
        Line::from(vec![
            Span::styled("Interactive:       ", label_style),
            Span::styled(format!("{interactive_count}"), value_style),
        ]),
        Line::from(vec![
            Span::styled("Terminal:          ", label_style),
            Span::styled(format!("{terminal_count}"), value_style),
        ]),
        Line::from(vec![
            Span::styled("Background agents: ", label_style),
            Span::styled(format!("{bg_count}"), value_style),
        ]),
        Line::from(vec![
            Span::styled("Runs executed:     ", label_style),
            Span::styled(format!("{runs_count}"), value_style),
        ]),
        Line::from(""),
    ];

    let top_clis = app.cli_usage.ranked();
    if !top_clis.is_empty() {
        lines.push(Line::from(Span::styled(
            "Most used harnesses",
            Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
        )));
        lines.push(Line::from(""));
        for (name, count) in top_clis.iter().take(4) {
            lines.push(Line::from(vec![
                Span::styled(format!("{name}  "), value_style),
                Span::styled(format!("{count} launches"), label_style),
            ]));
        }
        lines.push(Line::from(""));
    }

    lines.push(Line::from(Span::styled(
        "F1 or Esc to close",
        Style::default().fg(DIM),
    )));

    // Responsive sizing
    let content_height = lines.len() as u16 + 2; // +2 for borders
    let content_width = lines
        .iter()
        .map(|l| l.to_string().chars().count() as u16)
        .max()
        .unwrap_or(36)
        + 4; // padding
    let width = content_width.clamp(36, 50);
    let height = content_height.clamp(10, 22);
    let percent_x = (width * 100 / frame.area().width.max(1)).clamp(30, 60);
    let area = centered_rect(percent_x, height, frame.area());
    frame.render_widget(Clear, area);

    let block = Block::default()
        .title(" Canopy Stats ")
        .borders(Borders::ALL)
        .border_style(Style::default().fg(ACCENT))
        .style(Style::default().bg(Color::Rgb(15, 25, 15)));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    frame.render_widget(
        Paragraph::new(lines).alignment(ratatui::layout::Alignment::Center),
        inner,
    );
}
