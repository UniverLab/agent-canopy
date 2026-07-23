//! Dialog overlays — new agent, quit confirmation, color legend, context transfer.

use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Frame;

pub mod at_picker;
pub mod context_transfer;
pub mod knowledge_dialog;
pub mod launchpad;
pub mod loop_editor;
pub mod loop_form;
pub mod new_agent_dialog;
pub mod pickers;
pub mod rag_transfer;
pub mod section_picker;
pub mod simple_modals;
pub mod simple_prompt;

// Re-export public drawing functions
pub use context_transfer::draw_context_transfer_modal;
pub use knowledge_dialog::draw_knowledge_dialog;
pub use launchpad::draw_launchpad_dialog;
pub use loop_editor::draw_loop_editor_dialog;
pub use loop_form::draw_loop_form_dialog;
pub use new_agent_dialog::draw_new_agent_dialog;
pub use pickers::{draw_split_picker, draw_suggestion_picker};
pub use rag_transfer::draw_rag_transfer_modal;
pub use simple_modals::{
    draw_delete_loop_confirm, draw_delete_project_confirm, draw_legend, draw_quit_confirm,
};
pub use simple_prompt::draw_simple_prompt_dialog;

// Common imports shared with submodules
pub(crate) use super::ERROR_COLOR;
pub(crate) use super::{centered_rect, truncate_str};

fn gradient_wave_color(index: usize, shift: usize) -> Color {
    let gradient = crate::shared::banner::BANNER_GRADIENT;
    let len = gradient.len();
    if len == 0 {
        return Color::White;
    }
    if len == 1 {
        let (r, g, b) = gradient[0];
        return Color::Rgb(r, g, b);
    }

    let cycle_len = len * 2 - 2;
    let pos = (index + shift) % cycle_len;
    let gradient_idx = if pos < len { pos } else { cycle_len - pos };
    let (r, g, b) = gradient[gradient_idx];
    Color::Rgb(r, g, b)
}

pub(crate) fn draw_dialog_left_wave(frame: &mut Frame, area: Rect, tick: u64) {
    let wave = ["░", "▒", "░"];
    let shift =
        ((tick / 3) as usize) % (crate::shared::banner::BANNER_GRADIENT.len() * 2 - 1).max(1);
    let x = area.x.saturating_sub(1);
    let y = area.y + area.height.saturating_sub(wave.len() as u16) / 2;

    for (i, glyph) in wave.iter().enumerate() {
        let row = y + i as u16;
        if row >= area.y + area.height {
            break;
        }

        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                *glyph,
                Style::default()
                    .fg(gradient_wave_color(i, shift))
                    .add_modifier(Modifier::BOLD),
            ))),
            Rect::new(x, row, 1, 1),
        );
    }
}
