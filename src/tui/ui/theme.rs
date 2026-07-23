//! Centralized color palette for the TUI. Every renderer (header, footer,
//! sidebar, panels, system dashboard, dialogs) consumes this as of T2-T4;
//! no renderer reads a hardcoded color constant directly anymore.

use ratatui::style::Color;

/// `panel_bg`, `sidebar_bg`, and `show_borders` stay unread until T5 wires
/// up the borderless modern theme.
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
    /// Today's look — the exact values the old hardcoded constants held
    /// (`ACCENT`, `BORDER_COLOR`, `DIM`, `BG_SELECTED` from 54f7b87) before
    /// T1 centralized them here.
    pub fn classic() -> Self {
        Self {
            border_color: Color::Rgb(50, 50, 50),
            panel_bg: Color::Rgb(18, 18, 18),
            sidebar_bg: Color::Rgb(18, 18, 18),
            selected_bg: Color::Rgb(45, 45, 45),
            header_color: Color::Rgb(76, 175, 80),
            dim_text: Color::Rgb(150, 150, 170),
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
        assert_eq!(theme.border_color, Color::Rgb(50, 50, 50));
        assert_eq!(theme.panel_bg, Color::Rgb(18, 18, 18));
        assert_eq!(theme.sidebar_bg, Color::Rgb(18, 18, 18));
        assert_eq!(theme.selected_bg, Color::Rgb(45, 45, 45));
        assert_eq!(theme.header_color, Color::Rgb(76, 175, 80));
        assert_eq!(theme.dim_text, Color::Rgb(150, 150, 170));
        assert!(theme.show_borders);
    }
}
