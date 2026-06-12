use ratatui::style::{Color, Modifier, Style};
use ratatui::text::Span;
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, Paragraph};
use ratatui::Frame;

use crate::tui::app::dialog::SimplePromptDialog;

#[allow(unused_imports)]
use super::{BG_SELECTED, ERROR_COLOR, INTERACTIVE_COLOR};

// Old function removed - using simple prompt dialog instead
pub(crate) fn draw_at_picker_dropdown(
    frame: &mut Frame,
    dialog_area: ratatui::layout::Rect,
    anchor_area: ratatui::layout::Rect,
    accent: Color,
    dialog: &SimplePromptDialog,
) {
    let Some(picker) = &dialog.at_picker else {
        return;
    };

    const MAX_VISIBLE: usize = 8;
    let screen_h = frame.area().height;
    let available_below = screen_h.saturating_sub(anchor_area.y + anchor_area.height);
    let available_above = anchor_area.y.saturating_sub(dialog_area.y);

    let prefer_below = available_below >= 3 || available_below >= available_above;
    let available_space = if prefer_below {
        available_below
    } else {
        available_above
    };
    let visible_items = picker
        .entries
        .len()
        .clamp(1, MAX_VISIBLE)
        .min(available_space.saturating_sub(2) as usize);
    if visible_items == 0 {
        return;
    }
    let drop_h = visible_items as u16 + 2;
    let drop_y = if prefer_below {
        anchor_area.y + anchor_area.height
    } else {
        anchor_area.y.saturating_sub(drop_h)
    };

    let drop_area = ratatui::layout::Rect {
        x: anchor_area.x.saturating_sub(1).max(dialog_area.x),
        y: drop_y,
        width: dialog_area.width,
        height: drop_h,
    };
    frame.render_widget(Clear, drop_area);

    let title = format!(" {} ", picker.title());
    let block = Block::default()
        .title(title)
        .borders(Borders::ALL)
        .border_style(Style::default().fg(accent))
        .style(Style::default().bg(Color::Rgb(10, 20, 10)));
    let inner = block.inner(drop_area);
    frame.render_widget(block, drop_area);

    if picker.entries.is_empty() {
        frame.render_widget(
            Paragraph::new(Span::styled(
                "  no matches",
                Style::default().fg(Color::DarkGray),
            )),
            inner,
        );
        return;
    }

    let scroll = if picker.selected >= MAX_VISIBLE {
        picker.selected - MAX_VISIBLE + 1
    } else {
        0
    };

    let items: Vec<ListItem> = picker
        .entries
        .iter()
        .skip(scroll)
        .take(visible_items)
        .enumerate()
        .map(|(i, entry)| {
            let abs_idx = i + scroll;
            let icon = if entry.is_dir { "📁 " } else { "   " };

            // Show relative path when in recursive search mode (query active)
            let label = if picker.query.is_empty() {
                // Flat mode: just show filename
                format!("{}{}", icon, entry.name)
            } else {
                // Recursive mode: show relative path to distinguish files with same name
                let relative_path = entry
                    .path
                    .strip_prefix(&picker.workdir)
                    .unwrap_or(&entry.path);
                let display_path = if relative_path.to_string_lossy() == entry.name {
                    // Same directory as workdir
                    entry.name.clone()
                } else {
                    // Show relative path
                    relative_path.to_string_lossy().to_string()
                };
                format!("{}{}", icon, display_path)
            };

            let style = if abs_idx == picker.selected {
                Style::default()
                    .fg(Color::Black)
                    .bg(accent)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(Color::White)
            };
            ListItem::new(Span::styled(label, style))
        })
        .collect();

    frame.render_widget(List::new(items), inner);
}
