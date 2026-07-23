//! Centralized color palette for the TUI. header.rs, footer.rs, and
//! sidebar.rs consume it as of T2; panel/dashboard/dialog surfaces (T3-T4)
//! still read the free-standing constants in `ui/mod.rs` directly.

use ratatui::style::Color;

/// `panel_bg`, `sidebar_bg`, and `show_borders` stay unread until T3-T5 wire
/// the remaining renderers and the borderless modern theme.
#[allow(dead_code)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Theme {
    pub border_color: Color,
    pub panel_bg: Color,
    pub sidebar_bg: Color,
    pub selected_bg: Color,
    pub header_color: Color,
    pub dim_text: Color,
    pub show_borders: bool,
}

impl Theme {
    /// Today's look, unpacked from the constants in `super::{ACCENT, BORDER_COLOR, ...}`.
    pub fn classic() -> Self {
        Self {
            border_color: super::BORDER_COLOR,
            panel_bg: Color::Rgb(18, 18, 18),
            sidebar_bg: Color::Rgb(18, 18, 18),
            selected_bg: super::BG_SELECTED,
            header_color: super::ACCENT,
            dim_text: super::DIM,
            show_borders: true,
        }
    }
}

impl Default for Theme {
    fn default() -> Self {
        Self::classic()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classic_matches_default() {
        assert_eq!(Theme::classic(), Theme::default());
    }

    #[test]
    fn classic_reproduces_current_constants() {
        let theme = Theme::classic();
        assert_eq!(theme.border_color, super::super::BORDER_COLOR);
        assert_eq!(theme.panel_bg, Color::Rgb(18, 18, 18));
        assert_eq!(theme.sidebar_bg, Color::Rgb(18, 18, 18));
        assert_eq!(theme.selected_bg, super::super::BG_SELECTED);
        assert_eq!(theme.header_color, super::super::ACCENT);
        assert_eq!(theme.dim_text, super::super::DIM);
        assert!(theme.show_borders);
    }
}
