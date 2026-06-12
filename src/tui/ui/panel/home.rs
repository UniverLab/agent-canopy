use crate::shared::banner::BANNER_GRADIENT;
use crate::tui::app::types::App;
use crate::tui::brians_brain::{BriansBrain, CellState};
use ratatui::layout::Rect;
use ratatui::style::{Color, Style};
use ratatui::Frame;

pub(super) fn draw_canopy_banner_animation(frame: &mut Frame, area: Rect, app: &App) {
    let banner = crate::shared::banner::BANNER.trim_matches('\n');
    let banner_lines: Vec<&str> = banner.lines().collect();
    let total = banner_lines.len() as u16;

    let top_pad = if area.height > total {
        (area.height - total) / 2
    } else {
        0
    };

    let wave_offset = (app.animation_tick as f32 * 0.02) % 1.0;

    for (i, line) in banner_lines.iter().enumerate() {
        let row_pos = if total > 1 {
            i as f32 / (total - 1) as f32
        } else {
            0.0
        };
        let (r, g, b) = sample_mirrored_gradient((row_pos + wave_offset) % 1.0);
        let accent = Color::Rgb(r, g, b);
        let accent_dim = Color::Rgb(
            r.saturating_sub(40),
            g.saturating_sub(40),
            b.saturating_sub(40),
        );

        let y = area.y + top_pad + i as u16;
        if y >= area.y + area.height {
            break;
        }

        let line_start_x = area.x + area.width / 2 - (line.chars().count() as u16 / 2);

        for (col_idx, ch) in line.chars().enumerate() {
            let x = line_start_x + col_idx as u16;
            if x >= area.x + area.width {
                break;
            }
            if x < area.x {
                continue;
            }

            let buf_cell = &mut frame.buffer_mut()[(x, y)];
            let symbol = match ch {
                '█' => "█",
                '░' => "░",
                _ => continue,
            };
            buf_cell.set_symbol(symbol);
            buf_cell.set_style(Style::default().fg(if ch == '█' { accent } else { accent_dim }));
        }
    }
}

fn sample_mirrored_gradient(phase: f32) -> (u8, u8, u8) {
    let len = BANNER_GRADIENT.len();
    if len == 0 {
        return (255, 255, 255);
    }
    if len == 1 {
        return BANNER_GRADIENT[0];
    }

    // Mirror map 0..1 -> 0..1..0, so neighbors always follow the same gradient order.
    let mirrored = if phase < 0.5 {
        phase * 2.0
    } else {
        (1.0 - phase) * 2.0
    };

    let max_idx = (len - 1) as f32;
    let scaled = mirrored * max_idx;
    let i0 = scaled.floor() as usize;
    let i1 = (i0 + 1).min(len - 1);
    let t = (scaled - i0 as f32).clamp(0.0, 1.0);

    let (r0, g0, b0) = BANNER_GRADIENT[i0];
    let (r1, g1, b1) = BANNER_GRADIENT[i1];

    let lerp = |a: u8, b: u8| -> u8 { ((a as f32) + (b as f32 - a as f32) * t).round() as u8 };
    (lerp(r0, r1), lerp(g0, g1), lerp(b0, b1))
}

pub(crate) fn draw_brians_brain(frame: &mut Frame, area: Rect, brain: &BriansBrain) {
    const BRAIN_ON_GLYPHS: [&str; 5] = ["⠆", "⠒", "⠶", "⡷", "⣿"];
    const BRAIN_DYING_GLYPHS: [&str; 4] = ["⠖", "⠒", "⠂", "·"];
    let buf = frame.buffer_mut();

    for (r, row) in brain.grid.iter().enumerate() {
        if r as u16 >= area.height {
            break;
        }
        for (c, cell) in row.iter().enumerate() {
            if c as u16 >= area.width {
                break;
            }
            let x = area.x + c as u16;
            let y = area.y + r as u16;
            let g = brain.green_grid[r][c];
            let (ch, color) = match cell {
                CellState::On => {
                    let idx = ((g as usize * (BRAIN_ON_GLYPHS.len() - 1)) / 255)
                        .min(BRAIN_ON_GLYPHS.len() - 1);
                    (BRAIN_ON_GLYPHS[idx], Color::Rgb(0, g, 0))
                }
                CellState::Dying => {
                    let dim_g = (g as u16 * 6 / 10) as u8;
                    let idx = ((dim_g as usize * (BRAIN_DYING_GLYPHS.len() - 1)) / 255)
                        .min(BRAIN_DYING_GLYPHS.len() - 1);
                    (
                        BRAIN_DYING_GLYPHS[idx],
                        Color::Rgb(dim_g / 3, dim_g, dim_g / 3),
                    )
                }
                CellState::Off => (" ", Color::Reset),
            };
            let buf_cell = &mut buf[(x, y)];
            buf_cell.set_symbol(ch);
            buf_cell.set_style(Style::default().fg(color));
        }
    }
}
