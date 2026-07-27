use std::io::{self, Write};

enum BrowseAction {
    Confirm,
    Cancel,
    Continue,
}

enum MultiSelectAction {
    Confirm,
    Cancel,
    Continue,
}

fn filter_subdirs_from(current: &std::path::Path, filter: &str) -> Vec<String> {
    let all = list_subdirs(current);
    if filter.is_empty() {
        return all;
    }
    let f = filter.to_lowercase();
    all.into_iter()
        .filter(|name| name.to_lowercase().contains(&f))
        .collect()
}

fn adjust_cursor_bounds(cursor: &mut usize, len: usize) {
    if len > 0 && *cursor >= len {
        *cursor = len - 1;
    }
}

fn calculate_scroll(cursor: usize, visible: usize) -> usize {
    if cursor >= visible {
        cursor - visible + 1
    } else {
        0
    }
}

fn handle_browse_key(
    code: ratatui::crossterm::event::KeyCode,
    subdirs: &[String],
    cursor: &mut usize,
    current: &mut std::path::PathBuf,
    filter: &mut String,
) -> BrowseAction {
    use ratatui::crossterm::event::KeyCode;
    match code {
        KeyCode::Enter => BrowseAction::Confirm,
        KeyCode::Esc => BrowseAction::Cancel,
        KeyCode::Up => {
            *cursor = cursor.saturating_sub(1);
            BrowseAction::Continue
        }
        KeyCode::Down if !subdirs.is_empty() && *cursor + 1 < subdirs.len() => {
            *cursor += 1;
            BrowseAction::Continue
        }
        KeyCode::Right | KeyCode::Char('l') if filter.is_empty() => {
            if let Some(name) = subdirs.get(*cursor) {
                *current = current.join(name);
                *cursor = 0;
            }
            BrowseAction::Continue
        }
        KeyCode::Left | KeyCode::Char('h') if filter.is_empty() => {
            if let Some(parent) = current.parent() {
                *current = parent.to_path_buf();
                *cursor = 0;
            }
            BrowseAction::Continue
        }
        KeyCode::Right if !filter.is_empty() => {
            if let Some(name) = subdirs.get(*cursor) {
                *current = current.join(name);
                *cursor = 0;
                filter.clear();
            }
            BrowseAction::Continue
        }
        KeyCode::Backspace => {
            filter.pop();
            *cursor = 0;
            BrowseAction::Continue
        }
        KeyCode::Char(c) => {
            filter.push(c);
            *cursor = 0;
            BrowseAction::Continue
        }
        _ => BrowseAction::Continue,
    }
}

fn handle_multiselect_key(
    code: ratatui::crossterm::event::KeyCode,
    subdirs: &[String],
    cursor: &mut usize,
    current: &mut std::path::PathBuf,
    filter: &mut String,
    selected: &mut std::collections::HashSet<String>,
) -> MultiSelectAction {
    use ratatui::crossterm::event::KeyCode;
    match code {
        KeyCode::Enter => MultiSelectAction::Confirm,
        KeyCode::Esc => MultiSelectAction::Cancel,
        KeyCode::Up => {
            *cursor = cursor.saturating_sub(1);
            MultiSelectAction::Continue
        }
        KeyCode::Down if !subdirs.is_empty() && *cursor + 1 < subdirs.len() => {
            *cursor += 1;
            MultiSelectAction::Continue
        }
        KeyCode::Char(' ') if filter.is_empty() => {
            if let Some(name) = subdirs.get(*cursor) {
                let full_path = current.join(name).to_string_lossy().to_string();
                if selected.contains(&full_path) {
                    selected.remove(&full_path);
                } else {
                    selected.insert(full_path);
                }
            }
            MultiSelectAction::Continue
        }
        KeyCode::Right | KeyCode::Char('l') if filter.is_empty() => {
            if let Some(name) = subdirs.get(*cursor) {
                *current = current.join(name);
                *cursor = 0;
            }
            MultiSelectAction::Continue
        }
        KeyCode::Left | KeyCode::Char('h') if filter.is_empty() => {
            if let Some(parent) = current.parent() {
                *current = parent.to_path_buf();
                *cursor = 0;
            }
            MultiSelectAction::Continue
        }
        KeyCode::Right if !filter.is_empty() => {
            if let Some(name) = subdirs.get(*cursor) {
                *current = current.join(name);
                *cursor = 0;
                filter.clear();
            }
            MultiSelectAction::Continue
        }
        KeyCode::Char(c) if !c.is_control() && filter.len() < 50 => {
            filter.push(c);
            *cursor = 0;
            MultiSelectAction::Continue
        }
        KeyCode::Backspace => {
            filter.pop();
            *cursor = 0;
            MultiSelectAction::Continue
        }
        _ => MultiSelectAction::Continue,
    }
}

#[allow(clippy::too_many_arguments)]
fn render_browse_display(
    total_rows: usize,
    current: &std::path::Path,
    subdirs: &[String],
    cursor: usize,
    scroll: usize,
    visible: usize,
    filter: &str,
    has_above: bool,
    has_below: bool,
) {
    print!("\x1b[{total_rows}A");
    let path_str = current.to_string_lossy();
    let box_width = 70usize;
    let top_content_len = 4 + path_str.len(); // "┌─» " + path + "┐"
    let top_dashes = box_width.saturating_sub(top_content_len);
    print!(
        "\r\x1b[2K\x1b[36m┌─»\x1b[0m \x1b[36m{}\x1b[0m\x1b[36m{}┐\x1b[0m\r\n",
        path_str,
        "─".repeat(top_dashes)
    );
    print!(
        "\r\x1b[2K\x1b[90m│\x1b[0m ↑↓ navigate → enter ← back Enter confirm Esc cancel type to filter \x1b[90m│\x1b[0m\r\n"
    );

    if filter.is_empty() {
        print!(
            "\r\x1b[2K\x1b[90m│\x1b[0m filter: _{} \x1b[90m│\x1b[0m\r\n",
            " ".repeat(59)
        );
    } else {
        let filter_content = format!("filter: {}", filter);
        let filter_pad = box_width.saturating_sub(filter_content.len() + 2);
        print!(
            "\r\x1b[2K\x1b[90m│\x1b[0m \x1b[33m{}\x1b[0m{} \x1b[90m│\x1b[0m\r\n",
            filter,
            " ".repeat(filter_pad)
        );
    }

    render_subdirs_section(subdirs, cursor, scroll, visible, filter);
    let up = if has_above { "↑" } else { " " };
    let dn = if has_below { "↓" } else { " " };
    let footer_content = format!(
        "{} {}/{} {}",
        up,
        cursor.saturating_add(1),
        subdirs.len(),
        dn
    );
    let bottom_dashes = box_width.saturating_sub(footer_content.len() + 2);
    print!(
        "\r\x1b[2K\x1b[90m└{} {}┘\x1b[0m\r\n",
        "─".repeat(bottom_dashes),
        footer_content
    );
}

fn render_subdirs_section(
    subdirs: &[String],
    cursor: usize,
    scroll: usize,
    visible: usize,
    filter: &str,
) {
    if subdirs.is_empty() {
        let msg = if filter.is_empty() {
            "(empty — Enter to confirm, ← to go up)"
        } else {
            "(no matches for \"{filter}\")"
        };
        print!("\r\x1b[2K \x1b[90m{msg}\x1b[0m\r\n");
        for _ in 1..visible {
            print!("\r\x1b[2K\r\n");
        }
    } else {
        let mut drawn = 0;
        for (i, name) in subdirs.iter().enumerate().skip(scroll).take(visible) {
            if i == cursor {
                print!("\r\x1b[2K \x1b[1;32m▶\x1b[0m \x1b[7m {name} \x1b[0m\r\n");
            } else {
                print!("\r\x1b[2K {name}\r\n");
            }
            drawn += 1;
        }
        for _ in drawn..visible {
            print!("\r\x1b[2K\r\n");
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn render_multiselect_display(
    total_rows: usize,
    current: &std::path::Path,
    subdirs: &[String],
    cursor: usize,
    scroll: usize,
    visible: usize,
    filter: &str,
    has_above: bool,
    has_below: bool,
    selected: &std::collections::HashSet<String>,
) {
    print!("\x1b[{total_rows}A");
    let selected_count = selected.len();
    let path_str = current.to_string_lossy();
    let box_width = 70usize;
    let top_content_len = 4 + path_str.len(); // "┌─» " + path + "┐"
    let top_dashes = box_width.saturating_sub(top_content_len);
    print!(
        "\r\x1b[2K\x1b[36m┌─»\x1b[0m \x1b[36m{}\x1b[0m\x1b[36m{}┐\x1b[0m\r\n",
        path_str,
        "─".repeat(top_dashes)
    );
    let help_text = format!(
        "↑↓ navigate Space mark → enter ← back Enter confirm Esc cancel ({} selected)",
        selected_count
    );
    print!(
        "\r\x1b[2K\x1b[90m│\x1b[0m {} \x1b[90m│\x1b[0m\r\n",
        help_text
    );

    if filter.is_empty() {
        print!(
            "\r\x1b[2K\x1b[90m│\x1b[0m filter: _{} \x1b[90m│\x1b[0m\r\n",
            " ".repeat(59)
        );
    } else {
        let filter_content = format!("filter: {}", filter);
        let filter_pad = box_width.saturating_sub(filter_content.len() + 2);
        print!(
            "\r\x1b[2K\x1b[90m│\x1b[0m \x1b[33m{}\x1b[0m{} \x1b[90m│\x1b[0m\r\n",
            filter,
            " ".repeat(filter_pad)
        );
    }

    render_multiselect_subdirs(subdirs, cursor, scroll, visible, filter, current, selected);
    let up = if has_above { "↑" } else { " " };
    let dn = if has_below { "↓" } else { " " };
    let footer_content = format!(
        "{} {}/{} {}",
        up,
        cursor.saturating_add(1),
        subdirs.len(),
        dn
    );
    let bottom_dashes = box_width.saturating_sub(footer_content.len() + 2);
    print!(
        "\r\x1b[2K\x1b[90m└{} {}┘\x1b[0m\r\n",
        "─".repeat(bottom_dashes),
        footer_content
    );
}

fn render_multiselect_subdirs(
    subdirs: &[String],
    cursor: usize,
    scroll: usize,
    visible: usize,
    filter: &str,
    current: &std::path::Path,
    selected: &std::collections::HashSet<String>,
) {
    if subdirs.is_empty() {
        let msg = if filter.is_empty() {
            "(empty — Enter to confirm, ← to go up)"
        } else {
            "(no matches for \"{filter}\")"
        };
        print!("\r\x1b[2K \x1b[90m{msg}\x1b[0m\r\n");
        for _ in 1..visible {
            print!("\r\x1b[2K\r\n");
        }
    } else {
        let mut drawn = 0;
        for (i, name) in subdirs.iter().enumerate().skip(scroll).take(visible) {
            let full_path = current.join(name).to_string_lossy().to_string();
            let is_selected = selected.contains(&full_path);
            let marker = if is_selected { "☑" } else { "☐" };

            if i == cursor {
                print!("\r\x1b[2K \x1b[1;32m▶\x1b[0m \x1b[7m {marker} {name} \x1b[0m\r\n");
            } else {
                print!("\r\x1b[2K {marker} {name}\r\n");
            }
            drawn += 1;
        }
        for _ in drawn..visible {
            print!("\r\x1b[2K\r\n");
        }
    }
}

fn list_subdirs(path: &std::path::Path) -> Vec<String> {
    let Ok(entries) = std::fs::read_dir(path) else {
        return Vec::new();
    };
    let mut dirs: Vec<String> = entries
        .filter_map(|e| e.ok())
        .filter(|e| {
            e.file_type().map(|t| t.is_dir()).unwrap_or(false)
                || (e.file_type().map(|t| t.is_symlink()).unwrap_or(false) && e.path().is_dir())
        })
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().to_string();
            if name.starts_with('.') {
                None
            } else {
                Some(name)
            }
        })
        .collect();
    dirs.sort();
    dirs
}

pub(crate) fn browse_directory(start_dir: &str) -> String {
    use ratatui::crossterm::event::{read, Event, KeyEventKind};
    use ratatui::crossterm::terminal::{disable_raw_mode, enable_raw_mode};

    let mut current = std::path::PathBuf::from(start_dir);
    if !current.is_dir() {
        current = dirs::home_dir().unwrap_or_else(|| std::path::PathBuf::from("/"));
    }

    let mut cursor: usize = 0;
    let mut filter = String::new();
    let visible: usize = 10;
    let total_rows = 3 + visible + 1;

    let _ = enable_raw_mode();
    for _ in 0..total_rows {
        print!("\r\n");
    }

    loop {
        let subdirs = filter_subdirs_from(&current, &filter);
        adjust_cursor_bounds(&mut cursor, subdirs.len());
        let scroll = calculate_scroll(cursor, visible);
        let has_above = scroll > 0;
        let has_below = !subdirs.is_empty() && scroll + visible < subdirs.len();

        render_browse_display(
            total_rows, &current, &subdirs, cursor, scroll, visible, &filter, has_above, has_below,
        );
        let _ = io::stdout().flush();

        if let Ok(Event::Key(k)) = read() {
            if k.kind == KeyEventKind::Press {
                match handle_browse_key(k.code, &subdirs, &mut cursor, &mut current, &mut filter) {
                    BrowseAction::Confirm => {
                        let _ = disable_raw_mode();
                        print!("\r\n");
                        let _ = io::stdout().flush();
                        return current.to_string_lossy().to_string();
                    }
                    BrowseAction::Cancel => {
                        let _ = disable_raw_mode();
                        print!("\r\n");
                        let _ = io::stdout().flush();
                        return start_dir.to_string();
                    }
                    BrowseAction::Continue => {}
                }
            }
        }
    }
}

#[allow(dead_code)]
pub(crate) fn browse_directories_multiselect(start_dir: &str) -> Vec<String> {
    browse_directories_multiselect_with_preselected(start_dir, std::collections::HashSet::new())
}

pub(crate) fn browse_directories_multiselect_with_preselected(
    start_dir: &str,
    pre_selected: std::collections::HashSet<String>,
) -> Vec<String> {
    use ratatui::crossterm::event::{read, Event, KeyEventKind};
    use ratatui::crossterm::terminal::{disable_raw_mode, enable_raw_mode};

    let mut current = std::path::PathBuf::from(start_dir);
    if !current.is_dir() {
        if let Some(parent) = current.parent() {
            if parent.is_dir() {
                current = parent.to_path_buf();
            } else {
                current = dirs::home_dir().unwrap_or_else(|| std::path::PathBuf::from("/"));
            }
        } else {
            current = dirs::home_dir().unwrap_or_else(|| std::path::PathBuf::from("/"));
        }
    }

    let mut cursor: usize = 0;
    let mut filter = String::new();
    let mut selected = pre_selected;
    let visible: usize = 10;
    let total_rows = 4 + visible;

    let _ = enable_raw_mode();
    for _ in 0..total_rows {
        print!("\r\n");
    }

    loop {
        let subdirs = filter_subdirs_from(&current, &filter);
        adjust_cursor_bounds(&mut cursor, subdirs.len());
        let scroll = calculate_scroll(cursor, visible);
        let has_above = scroll > 0;
        let has_below = !subdirs.is_empty() && scroll + visible < subdirs.len();

        render_multiselect_display(
            total_rows, &current, &subdirs, cursor, scroll, visible, &filter, has_above, has_below,
            &selected,
        );
        let _ = io::stdout().flush();

        if let Ok(Event::Key(k)) = read() {
            if k.kind == KeyEventKind::Press {
                match handle_multiselect_key(
                    k.code,
                    &subdirs,
                    &mut cursor,
                    &mut current,
                    &mut filter,
                    &mut selected,
                ) {
                    MultiSelectAction::Confirm => {
                        let _ = disable_raw_mode();
                        print!("\r\n");
                        let _ = io::stdout().flush();
                        return selected.into_iter().collect();
                    }
                    MultiSelectAction::Cancel => {
                        let _ = disable_raw_mode();
                        print!("\r\n");
                        let _ = io::stdout().flush();
                        return Vec::new();
                    }
                    MultiSelectAction::Continue => {}
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::crossterm::event::KeyCode;
    use std::path::PathBuf;

    #[test]
    fn adjust_cursor_bounds_clamps_to_last() {
        let mut cursor = 10;
        adjust_cursor_bounds(&mut cursor, 5);
        assert_eq!(cursor, 4);
    }

    #[test]
    fn adjust_cursor_bounds_no_change_when_within_range() {
        let mut cursor = 2;
        adjust_cursor_bounds(&mut cursor, 5);
        assert_eq!(cursor, 2);
    }

    #[test]
    fn adjust_cursor_bounds_zero_length() {
        let mut cursor = 3;
        adjust_cursor_bounds(&mut cursor, 0);
        assert_eq!(cursor, 3);
    }

    #[test]
    fn adjust_cursor_bounds_at_boundary() {
        let mut cursor = 4;
        adjust_cursor_bounds(&mut cursor, 5);
        assert_eq!(cursor, 4);
    }

    #[test]
    fn adjust_cursor_bounds_cursor_exactly_at_len() {
        let mut cursor = 5;
        adjust_cursor_bounds(&mut cursor, 5);
        assert_eq!(cursor, 4);
    }

    #[test]
    fn calculate_scroll_returns_zero_when_cursor_below_visible() {
        assert_eq!(calculate_scroll(3, 10), 0);
    }

    #[test]
    fn calculate_scroll_offsets_when_cursor_exceeds_visible() {
        assert_eq!(calculate_scroll(10, 5), 6);
    }

    #[test]
    fn calculate_scroll_at_boundary() {
        assert_eq!(calculate_scroll(5, 5), 1);
    }

    #[test]
    fn calculate_scroll_zero_cursor() {
        assert_eq!(calculate_scroll(0, 10), 0);
    }

    #[test]
    fn calculate_scroll_cursor_just_below_visible() {
        assert_eq!(calculate_scroll(4, 5), 0);
    }

    #[test]
    fn handle_browse_key_enter_returns_confirm() {
        let subdirs = vec!["a".to_string(), "b".to_string()];
        let mut cursor = 0;
        let mut current = PathBuf::from("/tmp");
        let mut filter = String::new();
        assert!(matches!(
            handle_browse_key(KeyCode::Enter, &subdirs, &mut cursor, &mut current, &mut filter),
            BrowseAction::Confirm
        ));
    }

    #[test]
    fn handle_browse_key_esc_returns_cancel() {
        let subdirs = vec![];
        let mut cursor = 0;
        let mut current = PathBuf::from("/tmp");
        let mut filter = String::new();
        assert!(matches!(
            handle_browse_key(KeyCode::Esc, &subdirs, &mut cursor, &mut current, &mut filter),
            BrowseAction::Cancel
        ));
    }

    #[test]
    fn handle_browse_key_up_decrements_cursor() {
        let subdirs = vec!["a".to_string(), "b".to_string()];
        let mut cursor = 1;
        let mut current = PathBuf::from("/tmp");
        let mut filter = String::new();
        let _ = handle_browse_key(KeyCode::Up, &subdirs, &mut cursor, &mut current, &mut filter);
        assert_eq!(cursor, 0);
    }

    #[test]
    fn handle_browse_key_up_saturates_at_zero() {
        let subdirs = vec!["a".to_string()];
        let mut cursor = 0;
        let mut current = PathBuf::from("/tmp");
        let mut filter = String::new();
        let _ = handle_browse_key(KeyCode::Up, &subdirs, &mut cursor, &mut current, &mut filter);
        assert_eq!(cursor, 0);
    }

    #[test]
    fn handle_browse_key_down_increments_cursor() {
        let subdirs = vec!["a".to_string(), "b".to_string()];
        let mut cursor = 0;
        let mut current = PathBuf::from("/tmp");
        let mut filter = String::new();
        let _ = handle_browse_key(
            KeyCode::Down,
            &subdirs,
            &mut cursor,
            &mut current,
            &mut filter,
        );
        assert_eq!(cursor, 1);
    }

    #[test]
    fn handle_browse_key_down_no_move_at_end() {
        let subdirs = vec!["a".to_string()];
        let mut cursor = 0;
        let mut current = PathBuf::from("/tmp");
        let mut filter = String::new();
        let _ = handle_browse_key(
            KeyCode::Down,
            &subdirs,
            &mut cursor,
            &mut current,
            &mut filter,
        );
        assert_eq!(cursor, 0);
    }

    #[test]
    fn handle_browse_key_right_navigates_into_subdir() {
        let subdirs = vec!["child".to_string()];
        let mut cursor = 0;
        let mut current = PathBuf::from("/tmp");
        let mut filter = String::new();
        let _ = handle_browse_key(
            KeyCode::Right,
            &subdirs,
            &mut cursor,
            &mut current,
            &mut filter,
        );
        assert_eq!(current, PathBuf::from("/tmp/child"));
        assert_eq!(cursor, 0);
    }

    #[test]
    fn handle_browse_key_left_navigates_to_parent() {
        let subdirs = vec![];
        let mut cursor = 0;
        let mut current = PathBuf::from("/tmp/child");
        let mut filter = String::new();
        let _ = handle_browse_key(
            KeyCode::Left,
            &subdirs,
            &mut cursor,
            &mut current,
            &mut filter,
        );
        assert_eq!(current, PathBuf::from("/tmp"));
    }

    #[test]
    fn handle_browse_key_char_appends_to_filter() {
        let subdirs = vec![];
        let mut cursor = 0;
        let mut current = PathBuf::from("/tmp");
        let mut filter = String::new();
        let _ = handle_browse_key(
            KeyCode::Char('x'),
            &subdirs,
            &mut cursor,
            &mut current,
            &mut filter,
        );
        assert_eq!(filter, "x");
        assert_eq!(cursor, 0);
    }

    #[test]
    fn handle_browse_key_backspace_removes_from_filter() {
        let subdirs = vec![];
        let mut cursor = 0;
        let mut current = PathBuf::from("/tmp");
        let mut filter = "abc".to_string();
        let _ = handle_browse_key(
            KeyCode::Backspace,
            &subdirs,
            &mut cursor,
            &mut current,
            &mut filter,
        );
        assert_eq!(filter, "ab");
        assert_eq!(cursor, 0);
    }

    #[test]
    fn handle_browse_key_right_with_filter_navigates_and_clears() {
        let subdirs = vec!["child".to_string()];
        let mut cursor = 0;
        let mut current = PathBuf::from("/tmp");
        let mut filter = "ch".to_string();
        let _ = handle_browse_key(
            KeyCode::Right,
            &subdirs,
            &mut cursor,
            &mut current,
            &mut filter,
        );
        assert_eq!(current, PathBuf::from("/tmp/child"));
        assert!(filter.is_empty());
    }

    #[test]
    fn handle_browse_key_l_navigates_like_right() {
        let subdirs = vec!["child".to_string()];
        let mut cursor = 0;
        let mut current = PathBuf::from("/tmp");
        let mut filter = String::new();
        let _ = handle_browse_key(
            KeyCode::Char('l'),
            &subdirs,
            &mut cursor,
            &mut current,
            &mut filter,
        );
        assert_eq!(current, PathBuf::from("/tmp/child"));
    }

    #[test]
    fn handle_browse_key_h_navigates_like_left() {
        let subdirs = vec![];
        let mut cursor = 0;
        let mut current = PathBuf::from("/tmp/child");
        let mut filter = String::new();
        let _ = handle_browse_key(
            KeyCode::Char('h'),
            &subdirs,
            &mut cursor,
            &mut current,
            &mut filter,
        );
        assert_eq!(current, PathBuf::from("/tmp"));
    }

    #[test]
    fn handle_multiselect_key_space_toggles_selection() {
        let subdirs = vec!["a".to_string()];
        let mut cursor = 0;
        let mut current = PathBuf::from("/tmp");
        let mut filter = String::new();
        let mut selected = std::collections::HashSet::new();
        let _ = handle_multiselect_key(
            KeyCode::Char(' '),
            &subdirs,
            &mut cursor,
            &mut current,
            &mut filter,
            &mut selected,
        );
        assert!(selected.contains("/tmp/a"));
    }

    #[test]
    fn handle_multiselect_key_space_deselects() {
        let subdirs = vec!["a".to_string()];
        let mut cursor = 0;
        let mut current = PathBuf::from("/tmp");
        let mut filter = String::new();
        let mut selected: std::collections::HashSet<String> =
            ["/tmp/a".to_string()].into_iter().collect();
        let _ = handle_multiselect_key(
            KeyCode::Char(' '),
            &subdirs,
            &mut cursor,
            &mut current,
            &mut filter,
            &mut selected,
        );
        assert!(!selected.contains("/tmp/a"));
    }

    #[test]
    fn handle_multiselect_key_space_ignored_with_filter() {
        let subdirs = vec!["a".to_string()];
        let mut cursor = 0;
        let mut current = PathBuf::from("/tmp");
        let mut filter = "x".to_string();
        let mut selected = std::collections::HashSet::new();
        let _ = handle_multiselect_key(
            KeyCode::Char(' '),
            &subdirs,
            &mut cursor,
            &mut current,
            &mut filter,
            &mut selected,
        );
        assert!(selected.is_empty());
    }

    #[test]
    fn handle_multiselect_key_enter_returns_confirm() {
        let subdirs = vec![];
        let mut cursor = 0;
        let mut current = PathBuf::from("/tmp");
        let mut filter = String::new();
        let mut selected = std::collections::HashSet::new();
        assert!(matches!(
            handle_multiselect_key(
                KeyCode::Enter,
                &subdirs,
                &mut cursor,
                &mut current,
                &mut filter,
                &mut selected,
            ),
            MultiSelectAction::Confirm
        ));
    }

    #[test]
    fn handle_multiselect_key_esc_returns_cancel() {
        let subdirs = vec![];
        let mut cursor = 0;
        let mut current = PathBuf::from("/tmp");
        let mut filter = String::new();
        let mut selected = std::collections::HashSet::new();
        assert!(matches!(
            handle_multiselect_key(
                KeyCode::Esc,
                &subdirs,
                &mut cursor,
                &mut current,
                &mut filter,
                &mut selected,
            ),
            MultiSelectAction::Cancel
        ));
    }

    #[test]
    fn handle_multiselect_key_char_appends_to_filter() {
        let subdirs = vec![];
        let mut cursor = 0;
        let mut current = PathBuf::from("/tmp");
        let mut filter = String::new();
        let mut selected = std::collections::HashSet::new();
        let _ = handle_multiselect_key(
            KeyCode::Char('a'),
            &subdirs,
            &mut cursor,
            &mut current,
            &mut filter,
            &mut selected,
        );
        assert_eq!(filter, "a");
    }

    #[test]
    fn handle_multiselect_key_control_char_ignored() {
        let subdirs = vec![];
        let mut cursor = 0;
        let mut current = PathBuf::from("/tmp");
        let mut filter = String::new();
        let mut selected = std::collections::HashSet::new();
        let _ = handle_multiselect_key(
            KeyCode::Char('\x01'),
            &subdirs,
            &mut cursor,
            &mut current,
            &mut filter,
            &mut selected,
        );
        assert!(filter.is_empty());
    }

    #[test]
    fn handle_multiselect_key_right_navigates_into_subdir() {
        let subdirs = vec!["child".to_string()];
        let mut cursor = 0;
        let mut current = PathBuf::from("/tmp");
        let mut filter = String::new();
        let mut selected = std::collections::HashSet::new();
        let _ = handle_multiselect_key(
            KeyCode::Right,
            &subdirs,
            &mut cursor,
            &mut current,
            &mut filter,
            &mut selected,
        );
        assert_eq!(current, PathBuf::from("/tmp/child"));
        assert_eq!(cursor, 0);
    }

    #[test]
    fn handle_multiselect_key_left_navigates_to_parent() {
        let subdirs = vec![];
        let mut cursor = 0;
        let mut current = PathBuf::from("/tmp/child");
        let mut filter = String::new();
        let mut selected = std::collections::HashSet::new();
        let _ = handle_multiselect_key(
            KeyCode::Left,
            &subdirs,
            &mut cursor,
            &mut current,
            &mut filter,
            &mut selected,
        );
        assert_eq!(current, PathBuf::from("/tmp"));
    }

    #[test]
    fn handle_multiselect_key_down_increments_cursor() {
        let subdirs = vec!["a".to_string(), "b".to_string()];
        let mut cursor = 0;
        let mut current = PathBuf::from("/tmp");
        let mut filter = String::new();
        let mut selected = std::collections::HashSet::new();
        let _ = handle_multiselect_key(
            KeyCode::Down,
            &subdirs,
            &mut cursor,
            &mut current,
            &mut filter,
            &mut selected,
        );
        assert_eq!(cursor, 1);
    }

    #[test]
    fn handle_multiselect_key_up_decrements_cursor() {
        let subdirs = vec!["a".to_string(), "b".to_string()];
        let mut cursor = 1;
        let mut current = PathBuf::from("/tmp");
        let mut filter = String::new();
        let mut selected = std::collections::HashSet::new();
        let _ = handle_multiselect_key(
            KeyCode::Up,
            &subdirs,
            &mut cursor,
            &mut current,
            &mut filter,
            &mut selected,
        );
        assert_eq!(cursor, 0);
    }

    #[test]
    fn filter_subdirs_empty_filter_returns_all() {
        let subdirs = ["a".to_string(), "b".to_string(), "c".to_string()];
        // We can't test filter_subdirs_from directly (calls list_subdirs which does I/O),
        // but we test the filtering logic inline.
        let filter = "";
        let f = filter.to_lowercase();
        let result: Vec<&String> = subdirs
            .iter()
            .filter(|name| f.is_empty() || name.to_lowercase().contains(&f))
            .collect();
        assert_eq!(result.len(), 3);
    }

    #[test]
    fn filter_subdirs_case_insensitive() {
        let subdirs = ["Foo".to_string(), "bar".to_string(), "Baz".to_string()];
        let filter = "foo";
        let f = filter.to_lowercase();
        let result: Vec<&String> = subdirs
            .iter()
            .filter(|name| name.to_lowercase().contains(&f))
            .collect();
        assert_eq!(result, vec![&"Foo".to_string()]);
    }

    #[test]
    fn filter_subdirs_no_matches() {
        let subdirs = ["a".to_string(), "b".to_string()];
        let filter = "zzz";
        let f = filter.to_lowercase();
        let result: Vec<&String> = subdirs
            .iter()
            .filter(|name| name.to_lowercase().contains(&f))
            .collect();
        assert!(result.is_empty());
    }
}
