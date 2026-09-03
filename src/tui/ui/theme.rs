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
    /// Dialog/modal chrome background (C4 audit: `Color::Rgb(15, 25, 15)`
    /// was hardcoded identically across every file in `ui/dialogs/`).
    pub dialog_bg: Color,
    /// Default readable body/value text (C4 audit: bare `Color::White` at
    /// dozens of call sites across the sidebar and dialogs).
    pub text_primary: Color,
    /// Foreground for text drawn on top of an accent/header-colored
    /// background — e.g. a focused, selected row (C4 audit: `Color::Black`).
    pub accent_fg: Color,
    /// Muted secondary text distinct from `dim_text` — empty-state hints
    /// and similar (C4 audit: `Color::DarkGray`).
    pub muted_text: Color,
    /// Pending/in-progress/caution state (C4 audit: `Color::Yellow`).
    pub warning: Color,
    /// Failed/unavailable state (C4 audit: `Color::Red`).
    pub error: Color,
    /// Active/success indicator distinct from the status-role constants in
    /// `ui/mod.rs` (C4 audit: `Color::Green`).
    pub success: Color,
    /// Input-field background, unfocused (C4 audit: `Color::Rgb(30, 30, 30)`
    /// in the prompt builder's section fields).
    pub field_bg: Color,
    /// Input-field background, focused — the theme-driven pair for
    /// `field_bg` (C4 audit: `Color::Rgb(40, 40, 40)`).
    pub field_bg_focused: Color,
    /// Graph status palette — routed through Theme so the drawing never
    /// ships with fixed colors (CT2).
    pub status_running: Color,
    pub status_ok: Color,
    pub status_fail: Color,
    pub status_disabled: Color,
    pub status_interrupted: Color,
    pub kind_router: Color,
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
            dialog_bg: Color::Rgb(15, 25, 15),
            text_primary: Color::White,
            accent_fg: Color::Black,
            muted_text: Color::DarkGray,
            warning: Color::Yellow,
            error: Color::Red,
            success: Color::Green,
            field_bg: Color::Rgb(30, 30, 30),
            field_bg_focused: Color::Rgb(40, 40, 40),
            status_running: Color::Rgb(76, 175, 80),
            status_ok: Color::Rgb(66, 165, 245),
            status_fail: Color::Rgb(229, 57, 53),
            status_disabled: Color::Rgb(120, 120, 120),
            status_interrupted: Color::Rgb(255, 179, 0),
            kind_router: Color::Rgb(171, 71, 188),
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
            dialog_bg: panel_bg,
            text_primary: Color::Rgb(225, 225, 230),
            accent_fg: Color::Rgb(20, 20, 25),
            muted_text: Color::Rgb(75, 75, 85),
            warning: Color::Rgb(215, 180, 90),
            error: Color::Rgb(195, 95, 95),
            success: Color::Rgb(120, 190, 145),
            field_bg: Color::Rgb(35, 35, 45),
            field_bg_focused: Color::Rgb(45, 45, 60),
            status_running: Color::Rgb(76, 175, 80),
            status_ok: Color::Rgb(66, 165, 245),
            status_fail: Color::Rgb(229, 57, 53),
            status_disabled: Color::Rgb(120, 120, 120),
            status_interrupted: Color::Rgb(255, 179, 0),
            kind_router: Color::Rgb(171, 71, 188),
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
        assert_eq!(theme.dialog_bg, Color::Rgb(15, 25, 15));
        assert_eq!(theme.text_primary, Color::White);
        assert_eq!(theme.accent_fg, Color::Black);
        assert_eq!(theme.muted_text, Color::DarkGray);
        assert_eq!(theme.warning, Color::Yellow);
        assert_eq!(theme.error, Color::Red);
        assert_eq!(theme.success, Color::Green);
        assert_eq!(theme.status_running, Color::Rgb(76, 175, 80));
        assert_eq!(theme.status_ok, Color::Rgb(66, 165, 245));
        assert_eq!(theme.status_fail, Color::Rgb(229, 57, 53));
        assert_eq!(theme.status_disabled, Color::Rgb(120, 120, 120));
        assert_eq!(theme.status_interrupted, Color::Rgb(255, 179, 0));
        assert_eq!(theme.kind_router, Color::Rgb(171, 71, 188));
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

    /// Every colour role `Theme` declares must have a distinct value between
    /// `classic` and `modern` — otherwise a role was added and never given
    /// its own modern treatment, silently falling back to the classic look
    /// (the exact defect this spec fixes, reintroduced one field at a time).
    /// Semantic status colors (and router tag) are intentionally identical
    /// across themes — they encode pass/fail/running, not chrome — so they
    /// are excluded from this distinctness check (see CT2).
    #[test]
    fn classic_and_modern_differ_in_every_color_field() {
        let classic = Theme::classic();
        let modern = Theme::modern();
        assert_ne!(classic.border_color, modern.border_color);
        assert_ne!(classic.panel_bg, modern.panel_bg);
        assert_ne!(classic.sidebar_bg, modern.sidebar_bg);
        assert_ne!(classic.selected_bg, modern.selected_bg);
        assert_ne!(classic.header_color, modern.header_color);
        assert_ne!(classic.dim_text, modern.dim_text);
        assert_ne!(classic.dialog_bg, modern.dialog_bg);
        assert_ne!(classic.text_primary, modern.text_primary);
        assert_ne!(classic.accent_fg, modern.accent_fg);
        assert_ne!(classic.muted_text, modern.muted_text);
        assert_ne!(classic.warning, modern.warning);
        assert_ne!(classic.error, modern.error);
        assert_ne!(classic.success, modern.success);
        assert_ne!(classic.field_bg, modern.field_bg);
        assert_ne!(classic.field_bg_focused, modern.field_bg_focused);
    }

    #[test]
    fn classic_has_status_colors() {
        let theme = Theme::classic();
        assert_eq!(theme.status_running, Color::Rgb(76, 175, 80));
        assert_eq!(theme.status_ok, Color::Rgb(66, 165, 245));
        assert_eq!(theme.status_fail, Color::Rgb(229, 57, 53));
        assert_eq!(theme.status_disabled, Color::Rgb(120, 120, 120));
        assert_eq!(theme.status_interrupted, Color::Rgb(255, 179, 0));
        assert_eq!(theme.kind_router, Color::Rgb(171, 71, 188));
        // Modern intentionally preserves the same semantic values.
        let modern = Theme::modern();
        assert_eq!(modern.status_running, Color::Rgb(76, 175, 80));
        assert_eq!(modern.status_ok, Color::Rgb(66, 165, 245));
        assert_eq!(modern.status_fail, Color::Rgb(229, 57, 53));
        assert_eq!(modern.status_disabled, Color::Rgb(120, 120, 120));
        assert_eq!(modern.status_interrupted, Color::Rgb(255, 179, 0));
        assert_eq!(modern.kind_router, Color::Rgb(171, 71, 188));
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

    /// The prioritised C4 surfaces: every colour they draw must be resolved
    /// through `&Theme`, never re-derived from a hardcoded literal. A
    /// deliberately-kept exception (single-use, not a recurring role —
    /// see decision #2 in the C4 spec) must carry a `THEME-EXEMPT` comment
    /// on its own line or the line above, so this scan can tell "still a
    /// bug" apart from "documented and intentional."
    const SCANNED_SURFACES: &[&str] = &[
        "src/tui/ui/header.rs",
        "src/tui/ui/sidebar.rs",
        "src/tui/app/dialog/prompt.rs",
        "src/tui/ui/dialogs/simple_prompt.rs",
        "src/tui/ui/dialogs/mod.rs",
    ];

    /// Production code only — `#[cfg(test)]` fixtures routinely pass an
    /// arbitrary `Color` literal into a generic helper (e.g. `Color::Black`
    /// as an accent in a unit test) and that is not the defect this spec
    /// fixes. Every file here puts its whole test module after a single
    /// `#[cfg(test)]` line, so truncating there is exact, not a heuristic
    /// that could silently swallow production code.
    fn strip_test_module(source: &str) -> &str {
        match source.find("#[cfg(test)]") {
            Some(idx) => &source[..idx],
            None => source,
        }
    }

    #[test]
    fn prioritised_surfaces_have_no_undocumented_color_literals() {
        let literal = regex::Regex::new(
            r"Color::(Rgb|Indexed|Yellow|Cyan|Green|Red|Blue|Magenta|White|Black|Gray|DarkGray|LightRed|LightGreen|LightYellow|LightBlue|LightMagenta|LightCyan)",
        )
        .unwrap();

        let manifest_dir = env!("CARGO_MANIFEST_DIR");
        let mut violations = Vec::new();

        for rel_path in SCANNED_SURFACES {
            let full_path = std::path::Path::new(manifest_dir).join(rel_path);
            let source = std::fs::read_to_string(&full_path)
                .unwrap_or_else(|e| panic!("failed to read {rel_path}: {e}"));
            let production = strip_test_module(&source);

            let lines: Vec<&str> = production.lines().collect();
            for (i, line) in lines.iter().enumerate() {
                if !literal.is_match(line) {
                    continue;
                }
                let exempt_here = line.contains("THEME-EXEMPT");
                let exempt_above = i > 0 && lines[i - 1].contains("THEME-EXEMPT");
                if !exempt_here && !exempt_above {
                    violations.push(format!("{rel_path}:{} — {}", i + 1, line.trim()));
                }
            }
        }

        assert!(
            violations.is_empty(),
            "undocumented hardcoded colour literal(s) in prioritised C4 surfaces \
             (route through &Theme, or mark a genuine single-use exception with \
             a THEME-EXEMPT comment):\n{}",
            violations.join("\n")
        );
    }
}
