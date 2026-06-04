use crate::tui::app::types::App;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};
use ratatui::Frame;

pub fn draw_warp_input_box(frame: &mut Frame, area: Rect, app: &App, idx: usize) {
    let Some(agent) = app.terminal_agents.get(idx) else {
        return;
    };

    let cwd = compact_cwd(&agent.working_dir);
    let raw_input_text = agent
        .input_buffer
        .lock()
        .map(|b| b.clone())
        .unwrap_or_default();
    let sensitive_input = agent.is_sensitive_input_active();
    let input_text = if sensitive_input {
        String::new()
    } else {
        raw_input_text
    };
    let cursor_pos = agent.warp_cursor.min(input_text.len());

    let accent = agent.accent_color;
    let block = Block::default()
        .borders(Borders::TOP)
        .border_style(Style::default().fg(accent));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    if inner.height == 0 || inner.width < 4 {
        return;
    }

    // Prompt indicator: compact cwd + chevron
    let prompt = format!("{} ❯ ", cwd);
    let prompt_len = prompt.chars().count() as u16;
    let available_width = inner.width.saturating_sub(prompt_len) as usize;

    // Build the line: [prompt] [input_text]
    let mut spans = vec![Span::styled(
        &prompt,
        Style::default().fg(accent).add_modifier(Modifier::BOLD),
    )];

    if sensitive_input {
        spans.push(Span::styled(
            "[hidden input]",
            Style::default().fg(Color::Rgb(180, 180, 120)),
        ));
    } else if input_text.is_empty() {
        spans.push(Span::styled(
            "type a command…",
            Style::default().fg(Color::Rgb(80, 80, 100)),
        ));
    } else {
        // Horizontal scroll: keep cursor visible
        let cursor_char_idx = input_text[..cursor_pos].chars().count();
        let input_chars: Vec<char> = input_text.chars().collect();
        let input_len = input_chars.len();

        let visible_text = if input_len > available_width {
            // Reserve space for ellipsis indicators
            let has_leading_ellipsis = cursor_char_idx >= available_width;
            let has_trailing_ellipsis = {
                let scroll_start = if has_leading_ellipsis {
                    cursor_char_idx.saturating_sub((available_width.saturating_sub(2)) / 2)
                } else {
                    0
                };
                scroll_start + available_width.saturating_sub(2) < input_len
            };

            let ellipsis_space = (has_leading_ellipsis as usize) + (has_trailing_ellipsis as usize);
            let text_width = available_width.saturating_sub(ellipsis_space);

            // Calculate scroll offset to keep cursor visible
            let scroll_start = if has_leading_ellipsis {
                cursor_char_idx.saturating_sub(text_width / 2)
            } else {
                0
            };
            let scroll_end = (scroll_start + text_width).min(input_len);
            let visible: String = input_chars[scroll_start..scroll_end].iter().collect();

            // Add ellipsis indicators (now accounted for in width calculation)
            let mut result = String::new();
            if has_leading_ellipsis {
                result.push('…');
            }
            result.push_str(&visible);
            if has_trailing_ellipsis {
                result.push('…');
            }
            result
        } else {
            input_text.clone()
        };

        spans.push(Span::styled(
            visible_text,
            Style::default().fg(Color::White),
        ));
    }

    let line = Line::from(spans);
    let para = Paragraph::new(line);
    frame.render_widget(para, inner);

    // Position cursor inside the input box (accounting for scroll)
    let cursor_char_offset = input_text[..cursor_pos].chars().count();
    let input_chars_len = input_text.chars().count();

    let visible_cursor = if input_chars_len > available_width {
        let has_leading_ellipsis = cursor_char_offset >= available_width;
        let ellipsis_space = if has_leading_ellipsis { 1 } else { 0 };
        let text_width = available_width.saturating_sub(ellipsis_space * 2); // Reserve for both ellipsis

        let scroll_start = if has_leading_ellipsis {
            cursor_char_offset.saturating_sub(text_width / 2)
        } else {
            0
        };

        // Account for leading ellipsis in cursor position
        let ellipsis_offset = if has_leading_ellipsis { 1 } else { 0 };
        cursor_char_offset.saturating_sub(scroll_start) + ellipsis_offset
    } else {
        cursor_char_offset
    };

    let cx = inner.x + prompt_len + visible_cursor as u16;
    let cy = inner.y;
    if cx < inner.x + inner.width {
        frame.set_cursor_position((cx, cy));
    }
}

pub fn render_command_chips(frame: &mut Frame, area: Rect, app: &App, session_name: &str) {
    let hist = match app.terminal_histories.get(session_name) {
        Some(h) if !h.commands.is_empty() => h,
        _ => return,
    };

    // Get last 5 unique commands, most recent first
    let mut recent: Vec<&str> = Vec::new();
    let mut sorted: Vec<&crate::tui::terminal_history::CommandEntry> =
        hist.commands.iter().collect();
    sorted.sort_by_key(|entry| std::cmp::Reverse(entry.last_run));
    for entry in &sorted {
        if !recent.contains(&entry.cmd.as_str()) {
            recent.push(&entry.cmd);
        }
        if recent.len() >= 5 {
            break;
        }
    }
    if recent.is_empty() {
        return;
    }

    // Build chip spans that fit in the available width
    let bar_y = area.y + area.height.saturating_sub(1);
    let max_w = area.width as usize;
    let mut spans: Vec<Span> = Vec::new();
    let mut used = 0;

    for cmd in &recent {
        let chip = format!(" ✓ {} ", cmd);
        let chip_len = chip.chars().count() + 1; // +1 for gap
        if used + chip_len > max_w {
            break;
        }
        spans.push(Span::styled(
            chip,
            Style::default()
                .fg(Color::Rgb(180, 220, 180))
                .bg(Color::Rgb(20, 40, 20)),
        ));
        spans.push(Span::raw(" "));
        used += chip_len;
    }

    if !spans.is_empty() {
        let bar = Paragraph::new(Line::from(spans));
        let bar_area = Rect::new(area.x, bar_y, area.width, 1);
        frame.render_widget(bar, bar_area);
    }
}

pub fn compact_cwd(cwd: &str) -> String {
    let mut path = cwd.to_string();

    // Replace home dir with ~
    if let Some(home) = dirs::home_dir() {
        let home_str = home.to_string_lossy();
        if let Some(rest) = path.strip_prefix(home_str.as_ref()) {
            path = format!("~{rest}");
        }
    }

    let parts: Vec<&str> = path.split('/').filter(|p| !p.is_empty()).collect();
    if parts.len() <= 3 {
        return path;
    }

    // Show first + last segment with … in between
    let first = parts[0];
    let last = parts[parts.len() - 1];
    if first.starts_with('~') {
        format!("{first}/…/{last}")
    } else {
        format!("/{first}/…/{last}")
    }
}
