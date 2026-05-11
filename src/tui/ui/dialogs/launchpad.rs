use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Wrap};
use ratatui::Frame;

use crate::tui::app::dialog::LaunchpadChoice;
use crate::tui::app::types::App;

use super::{centered_rect, truncate_str, ACCENT, BG_SELECTED, DIM};

pub fn draw_launchpad_dialog(frame: &mut Frame, app: &App) {
    let Some(dialog) = &app.launchpad_dialog else {
        return;
    };

    let area = centered_rect(70, 14, frame.area());
    frame.render_widget(Clear, area);

    let title = format!(
        " Nueva Sesion: {} ",
        truncate_str(&super::super::last_two_segments(&dialog.workdir), 40)
    );
    let block = Block::default()
        .title(title)
        .borders(Borders::ALL)
        .border_style(Style::default().fg(ACCENT))
        .style(Style::default().bg(ratatui::style::Color::Black));
    frame.render_widget(block, area);

    let inner = Rect::new(
        area.x.saturating_add(1),
        area.y.saturating_add(1),
        area.width.saturating_sub(2),
        area.height.saturating_sub(2),
    );

    let mut lines: Vec<Line> = Vec::new();
    if let Some(previous) = &dialog.previous {
        lines.push(Line::from(Span::styled(
            "Mision anterior (inconclusa):",
            Style::default().fg(DIM),
        )));
        lines.push(Line::from(format!(
            "  {}",
            truncate_str(&previous.mission, 80)
        )));
        if let Some(summary) = &previous.summary {
            lines.push(Line::from(Span::styled(
                format!("Resumen previo: {}", truncate_str(summary, 78)),
                Style::default().fg(DIM),
            )));
        }
    } else {
        lines.push(Line::from(Span::styled(
            "No previous mission found for this workspace.",
            Style::default().fg(DIM),
        )));
    }

    lines.push(Line::from(""));
    lines.push(Line::from("What do you want to do today?"));

    let continue_style = if dialog.selected == LaunchpadChoice::ContinuePrevious {
        Style::default().bg(BG_SELECTED).fg(ACCENT)
    } else {
        Style::default().fg(DIM)
    };
    let new_style = if dialog.selected == LaunchpadChoice::NewMission {
        Style::default().bg(BG_SELECTED).fg(ACCENT)
    } else {
        Style::default().fg(DIM)
    };

    lines.push(Line::from(Span::styled(
        if dialog.previous.is_some() {
            "  [ Continue previous mission ]"
        } else {
            "  [ Continue previous mission ] (disabled)"
        },
        continue_style,
    )));
    lines.push(Line::from(Span::styled("  [ New mission ]", new_style)));

    if dialog.selected == LaunchpadChoice::NewMission {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            format!("Mission: {}", dialog.new_mission),
            Style::default().fg(ratatui::style::Color::White),
        )));
    }

    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "[Enter] Confirm  [Tab] Toggle  [Esc] Cancel",
        Style::default().fg(DIM).add_modifier(Modifier::ITALIC),
    )));

    frame.render_widget(
        Paragraph::new(lines)
            .wrap(Wrap { trim: false })
            .block(Block::default()),
        inner,
    );

    if dialog.selected == LaunchpadChoice::NewMission {
        let mission_prefix = "Mission: ";
        let cursor_x = inner
            .x
            .saturating_add(mission_prefix.len() as u16)
            .saturating_add(dialog.cursor_display_col() as u16);
        let cursor_y = inner.y.saturating_add(8);
        frame.set_cursor_position((cursor_x, cursor_y));
    }
}
