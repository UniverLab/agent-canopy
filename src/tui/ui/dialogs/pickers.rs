use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, Paragraph};
use ratatui::Frame;

use super::{centered_rect, ACCENT, DIM};
use crate::tui::app::types::App;

pub fn draw_split_picker(frame: &mut Frame, app: &App) {
    if !app.split_picker_open {
        return;
    }

    let sessions = &app.split_picker_sessions;
    let current_name = match app.selected_agent() {
        Some(crate::tui::app::types::AgentEntry::Interactive(idx)) => {
            app.interactive_agents[*idx].name.clone()
        }
        Some(crate::tui::app::types::AgentEntry::Terminal(idx)) => {
            app.terminal_agents[*idx].name.clone()
        }
        _ => String::new(),
    };

    let visible = sessions.len().min(6) as u16;
    let height = 9 + visible;
    let area = centered_rect(60, height, frame.area());
    frame.render_widget(Clear, area);

    let block = Block::default()
        .title(" Split con... ")
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Green))
        .style(Style::default().bg(Color::Rgb(15, 25, 15)));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let mut lines = vec![
        Line::from(""),
        Line::from(vec![
            Span::styled("  Current: ", Style::default().fg(DIM)),
            Span::styled(
                &current_name,
                Style::default()
                    .fg(Color::White)
                    .add_modifier(Modifier::BOLD),
            ),
        ]),
        Line::from(""),
        Line::from(Span::styled("  Selecciona:", Style::default().fg(DIM))),
    ];

    for (i, (name, type_label)) in sessions.iter().enumerate() {
        if name == &current_name {
            continue;
        }
        let selected = i == app.split_picker_idx;
        let style = if selected {
            Style::default()
                .fg(Color::Black)
                .bg(Color::Green)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(Color::White)
        };
        let prefix = if selected { "  > " } else { "    " };
        lines.push(Line::from(vec![
            Span::styled(format!("{}{}", prefix, name), style),
            Span::styled(format!("  [{}]", type_label), Style::default().fg(DIM)),
        ]));
    }

    lines.push(Line::from(""));

    let orient_label = match app.split_picker_orientation {
        crate::domain::models::SplitOrientation::Horizontal => "● Horizontal  ○ Vertical",
        crate::domain::models::SplitOrientation::Vertical => "○ Horizontal  ● Vertical",
    };
    lines.push(Line::from(vec![
        Span::styled("  Orientación:  ", Style::default().fg(DIM)),
        Span::styled(orient_label, Style::default().fg(Color::White)),
    ]));
    lines.push(Line::from(Span::styled(
        "                Tab para alternar",
        Style::default().fg(DIM),
    )));
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "  Esc cancelar               Enter crear",
        Style::default().fg(DIM),
    )));

    frame.render_widget(Paragraph::new(lines), inner);
}
pub fn draw_suggestion_picker(frame: &mut Frame, app: &App, panel_area: ratatui::layout::Rect) {
    let Some(picker) = &app.suggestion_picker else {
        return;
    };

    // Determine the actual area to draw the picker in (respecting split view)
    let picker_area = if let Some(ref split_id) = app.active_split_id {
        if let Some(group) = app.split_groups.iter().find(|g| g.id == *split_id) {
            let orientation = group.orientation;
            let areas = match orientation {
                crate::domain::models::SplitOrientation::Horizontal => {
                    Layout::horizontal([Constraint::Percentage(50), Constraint::Percentage(50)])
                        .areas(panel_area)
                }
                crate::domain::models::SplitOrientation::Vertical => {
                    Layout::vertical([Constraint::Percentage(50), Constraint::Percentage(50)])
                        .areas(panel_area)
                }
            };
            let [area_a, area_b]: [Rect; 2] = areas;
            let raw = if app.split_right_focused {
                area_b
            } else {
                area_a
            };
            // Account for the split panel border (1px each side)
            Rect::new(
                raw.x.saturating_add(1),
                raw.y.saturating_add(1),
                raw.width.saturating_sub(2),
                raw.height.saturating_sub(2),
            )
        } else {
            panel_area
        }
    } else {
        panel_area
    };

    if picker.items.is_empty() {
        // Show "no matches" indicator
        let w = 30u16.min(picker_area.width.saturating_sub(2));
        let area = ratatui::layout::Rect::new(
            picker_area.x + 1,
            picker_area.y + picker_area.height.saturating_sub(3),
            w,
            2,
        );
        frame.render_widget(Clear, area);
        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(DIM))
            .style(Style::default().bg(Color::Rgb(15, 15, 25)));
        let inner = block.inner(area);
        frame.render_widget(block, area);
        let msg = Paragraph::new("  No matches").style(Style::default().fg(DIM));
        frame.render_widget(msg, inner);
        return;
    }

    let visible = picker.visible_count().min(10) as u16;
    let w = 60u16.min(picker_area.width.saturating_sub(2));
    let h = visible + 2; // items + border

    let total = picker.items.len();
    let title = match picker.mode {
        crate::tui::terminal_history::PickerMode::CommandHistory => {
            let filter_hint = if picker.input.is_empty() {
                String::new()
            } else {
                format!(" | {}", picker.input)
            };
            if total > picker.visible_count() {
                format!(
                    " History [{}/{}]{} ",
                    picker.selected + 1,
                    total,
                    filter_hint
                )
            } else {
                format!(" History{} ", filter_hint)
            }
        }
        crate::tui::terminal_history::PickerMode::CdDirectory => {
            format!(" {} ←→ ", picker.input)
        }
    };

    // Anchor above the warp input box (3 rows from bottom)
    let area = ratatui::layout::Rect::new(
        picker_area.x + 1,
        picker_area.y + picker_area.height.saturating_sub(h + 4),
        w,
        h,
    );
    frame.render_widget(Clear, area);

    let block = Block::default()
        .title(title)
        .borders(Borders::ALL)
        .border_style(Style::default().fg(ACCENT))
        .style(Style::default().bg(Color::Rgb(15, 25, 15)));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let scroll_offset = picker.scroll_offset;
    let items: Vec<ListItem> = picker
        .visible_items()
        .iter()
        .enumerate()
        .map(|(i, item)| {
            let abs_index = scroll_offset + i;
            let selected = abs_index == picker.selected;
            let style = if selected {
                Style::default()
                    .fg(Color::Black)
                    .bg(ACCENT)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(Color::White)
            };
            let prefix = if selected { "> " } else { "  " };
            ListItem::new(Line::from(vec![
                Span::styled(prefix, style),
                Span::styled(&item.label, style),
            ]))
        })
        .collect();

    let list = List::new(items);
    frame.render_widget(list, inner);
}
