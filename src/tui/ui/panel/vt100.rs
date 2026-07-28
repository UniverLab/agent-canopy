use crate::tui::agent::ScreenSnapshot;
use crate::tui::app::types::App;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::widgets::Paragraph;
use ratatui::Frame;

/// Background for mouse-selected cells in a PTY pane.
const SELECTION_BG: Color = Color::Rgb(70, 90, 130);

/// Normalized linear selection endpoints: ((row, col) start, (row, col) end).
pub(super) type PaneSelection = ((u16, u16), (u16, u16));

fn cell_selected(selection: PaneSelection, row: u16, col: u16) -> bool {
    let ((r0, c0), (r1, c1)) = selection;
    if row < r0 || row > r1 {
        return false;
    }
    if r0 == r1 {
        return col >= c0 && col <= c1;
    }
    match row {
        r if r == r0 => col >= c0,
        r if r == r1 => col <= c1,
        _ => true,
    }
}

pub fn render_vt_screen(
    frame: &mut Frame,
    area: Rect,
    snap: &ScreenSnapshot,
    selection: Option<PaneSelection>,
) {
    let buf = frame.buffer_mut();
    for (row_idx, row) in snap.cells.iter().enumerate() {
        if row_idx as u16 >= area.height {
            break;
        }
        let y = area.y + row_idx as u16;

        for (col_idx, cell) in row.iter().enumerate() {
            if col_idx as u16 >= area.width {
                break;
            }
            let x = area.x + col_idx as u16;
            let selected =
                selection.is_some_and(|sel| cell_selected(sel, row_idx as u16, col_idx as u16));

            let Some(c) = cell else {
                if selected {
                    let buf_cell = &mut buf[(x, y)];
                    buf_cell.set_symbol(" ");
                    buf_cell.set_style(Style::default().bg(SELECTION_BG));
                }
                continue;
            };

            let ch = if c.ch.is_empty() { " " } else { &c.ch };
            let (fg, bg) = if c.inverse {
                (c.bg, c.fg)
            } else {
                (c.fg, c.bg)
            };

            let mut style = Style::default().fg(fg).bg(bg);
            if c.bold {
                style = style.add_modifier(Modifier::BOLD);
            }
            if c.underline {
                style = style.add_modifier(Modifier::UNDERLINED);
            }
            if selected {
                style = style.bg(SELECTION_BG);
            }

            let buf_cell = &mut buf[(x, y)];
            buf_cell.set_symbol(ch);
            buf_cell.set_style(style);
        }
    }
}

pub(super) fn render_indicators(frame: &mut Frame, inner: Rect, snap: &ScreenSnapshot, _app: &App) {
    if snap.scrolled {
        let msg = " \u{2592} SCROLLED \u{2592} ";
        let w = msg.chars().count() as u16;
        let x = inner.x + inner.width.saturating_sub(w + 1);
        let area = Rect::new(x, inner.y, w, 1);
        let widget = Paragraph::new(msg).style(Style::default().fg(Color::Yellow).bg(Color::Black));
        frame.render_widget(widget, area);
    }
}
