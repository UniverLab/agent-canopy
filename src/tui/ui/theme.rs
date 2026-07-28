//! Centralized color palette for the TUI. Every renderer (header, footer,
//! sidebar, panels, system dashboard, dialogs) consumes this as of T2-T4;
//! no renderer reads a hardcoded color constant directly anymore.

use ratatui::style::Color;

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

    /// Borderless, background-contrast look (T5): panels are separated by
    /// differing background colors instead of box-drawing borders.
    pub fn modern() -> Self {
        let panel_bg = Color::Rgb(25, 25, 35);
        Self {
            border_color: panel_bg,
            panel_bg,
            sidebar_bg: Color::Rgb(18, 18, 25),
            selected_bg: Color::Rgb(40, 40, 55),
            header_color: Color::Rgb(160, 160, 170),
            dim_text: Color::Rgb(90, 90, 100),
            show_borders: false,
        }
    }

    /// Resolve a `Theme` from the persisted `CanopyConfig::theme` string
    /// (T6): `"modern"` -> [`Theme::modern`], anything else -> classic.
    /// An unrecognized value (a config from a newer binary, a typo, etc.)
    /// falls back to classic with a warning instead of failing to start.
    pub fn resolve(config_value: &str) -> Self {
        match config_value {
            "classic" => Self::classic(),
            "modern" => Self::modern(),
            other => {
                tracing::warn!(theme = other, "unknown theme in config, using classic");
                Self::classic()
            }
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

    #[test]
    fn modern_is_borderless() {
        assert!(!Theme::modern().show_borders);
    }

    #[test]
    fn modern_reproduces_spec_values() {
        let theme = Theme::modern();
        assert_eq!(theme.panel_bg, Color::Rgb(25, 25, 35));
        assert_eq!(theme.border_color, theme.panel_bg);
        assert_eq!(theme.sidebar_bg, Color::Rgb(18, 18, 25));
        assert_eq!(theme.selected_bg, Color::Rgb(40, 40, 55));
        assert_eq!(theme.header_color, Color::Rgb(160, 160, 170));
        assert_eq!(theme.dim_text, Color::Rgb(90, 90, 100));
    }

    #[test]
    fn resolve_classic_returns_classic() {
        assert_eq!(Theme::resolve("classic"), Theme::classic());
    }

    #[test]
    fn resolve_modern_returns_modern() {
        assert_eq!(Theme::resolve("modern"), Theme::modern());
    }

    #[test]
    fn resolve_unknown_value_falls_back_to_classic_without_panicking() {
        assert_eq!(Theme::resolve("bogus"), Theme::classic());
        assert_eq!(Theme::resolve(""), Theme::classic());
    }

    #[test]
    fn borders_for_classic_draws_all_sides() {
        use ratatui::widgets::Borders;
        let borders = crate::tui::ui::borders_for(&Theme::classic());
        assert_eq!(borders, Borders::ALL);
        assert!(!borders.is_empty());
    }

    #[test]
    fn borders_for_modern_draws_no_sides() {
        use ratatui::widgets::Borders;
        let borders = crate::tui::ui::borders_for(&Theme::modern());
        assert_eq!(borders, Borders::NONE);
        assert!(borders.is_empty());
    }

    #[test]
    fn test_backend_classic_draws_box_glyphs_modern_does_not() {
        use ratatui::backend::TestBackend;
        use ratatui::widgets::{Block, Borders};
        use ratatui::Terminal;

        let render = |borders: Borders| -> String {
            let backend = TestBackend::new(10, 5);
            let mut terminal = Terminal::new(backend).unwrap();
            terminal
                .draw(|frame| {
                    let block = Block::default()
                        .borders(borders)
                        .border_style(ratatui::style::Style::default().fg(Color::White));
                    frame.render_widget(block, frame.area());
                })
                .unwrap();
            let buffer = terminal.backend().buffer().clone();
            let mut text = String::new();
            for y in 0..buffer.area.height {
                for x in 0..buffer.area.width {
                    text.push_str(buffer[(x, y)].symbol());
                }
                text.push('\n');
            }
            text
        };

        let classic = render(Borders::ALL);
        let modern = render(Borders::NONE);

        // Borders::ALL paints corners on the top-left of a 10-wide frame.
        assert!(
            classic.starts_with('┌'),
            "Borders::ALL should paint a top-left corner glyph\n{classic}"
        );
        // Borders::NONE paints no glyphs at all in the frame interior.
        for line in modern.lines() {
            assert!(
                !line.contains('─')
                    && !line.contains('│')
                    && !line.contains('┌')
                    && !line.contains('┐')
                    && !line.contains('└')
                    && !line.contains('┘'),
                "Borders::NONE must not contain any box-drawing glyphs\nline: {line:?}\nfull:\n{modern}"
            );
        }
    }
}
