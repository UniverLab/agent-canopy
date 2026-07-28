use anyhow::Result;
use ratatui::crossterm::event::{KeyCode, KeyModifiers};

use crate::tui::app::dialog::{BackgroundTrigger, NewAgentDialog, NewTaskMode, NewTaskType};
use crate::tui::app::types::App;
use crate::tui::ui::dialogs::new_agent_dialog::{prompt_visual_line_count, PROMPT_VISIBLE_ROWS};

// ── Dialog: new agent creation ──────────────────────────────────────
//
// Flow: ↑↓ switch fields, ←→ choose CLI/type/mode, ↑↓ in dir browser,
//       Space enter directory, Enter launch, Esc cancel.

pub fn handle_dialog_key(app: &mut App, code: KeyCode, modifiers: KeyModifiers) -> Result<()> {
    {
        let Some(dialog) = app.new_agent_dialog.as_mut() else {
            return Ok(());
        };

        if handle_picker_key(dialog, code) {
            return Ok(());
        }
    }

    match code {
        KeyCode::Esc => app.close_new_agent_dialog(),
        KeyCode::Enter if !modifiers.contains(KeyModifiers::SHIFT) => handle_dialog_enter(app),
        _ => {
            let term_width = app.term_width;
            let Some(dialog) = app.new_agent_dialog.as_mut() else {
                return Ok(());
            };
            handle_dialog_field_key(term_width, dialog, code, modifiers);
        }
    }

    Ok(())
}

#[derive(Clone, Copy)]
struct DialogFields {
    is_interactive: bool,
    is_terminal: bool,
    is_background: bool,
    cli_field: usize,
    identity_field: usize,
    model_field: usize,
    prompt_field: usize,
    extra_field: usize,
    dir_field: usize,
    yolo_field: usize,
}

impl DialogFields {
    fn from(dialog: &NewAgentDialog) -> Self {
        let is_interactive = matches!(dialog.task_type, NewTaskType::Interactive);
        let is_terminal = matches!(dialog.task_type, NewTaskType::Terminal);
        let is_background = matches!(dialog.task_type, NewTaskType::Background);

        Self {
            is_interactive,
            is_terminal,
            is_background,
            cli_field: if is_interactive || is_background {
                2
            } else {
                0
            },
            identity_field: if is_interactive { 5 } else { 0 },
            model_field: 3,
            prompt_field: 4,
            extra_field: 5,
            dir_field: if is_interactive {
                6
            } else if is_terminal {
                1
            } else {
                6
            },
            yolo_field: 4,
        }
    }

    fn is_watch_dir_field(self, dialog: &NewAgentDialog) -> bool {
        self.is_background
            && dialog.field == self.extra_field
            && matches!(dialog.background_trigger, BackgroundTrigger::Watch)
    }

    fn cli_up_target(self) -> usize {
        if self.is_interactive || self.is_background {
            1
        } else {
            0
        }
    }

    fn previous_dir_field(self, is_watch_dir: bool) -> usize {
        if is_watch_dir {
            self.prompt_field
        } else if self.is_interactive {
            self.yolo_field
        } else if self.is_terminal {
            0
        } else {
            self.extra_field
        }
    }

    fn next_dir_field(self, current_field: usize) -> usize {
        if self.is_interactive {
            self.yolo_field
        } else if self.is_terminal {
            2
        } else {
            current_field
        }
    }
}

fn handle_picker_key(dialog: &mut NewAgentDialog, code: KeyCode) -> bool {
    if dialog.session_picker_open {
        handle_session_picker_key(dialog, code);
        return true;
    }

    if dialog.cli_picker_open {
        handle_cli_picker_key(dialog, code);
        return true;
    }

    false
}

fn handle_session_picker_key(dialog: &mut NewAgentDialog, code: KeyCode) {
    match code {
        KeyCode::Down => move_session_picker(dialog, true),
        KeyCode::Up => move_session_picker(dialog, false),
        KeyCode::Enter => dialog.confirm_session_pick(),
        KeyCode::Esc | KeyCode::Backspace => dialog.session_picker_open = false,
        _ => {}
    }
}

fn move_session_picker(dialog: &mut NewAgentDialog, forward: bool) {
    let Some(next) = wrapped_index(
        dialog.session_picker_idx,
        dialog.session_entries.len(),
        forward,
    ) else {
        return;
    };
    dialog.session_picker_idx = next;
}

fn handle_cli_picker_key(dialog: &mut NewAgentDialog, code: KeyCode) {
    match code {
        KeyCode::Down => dialog.move_cli_picker_next(),
        KeyCode::Up => dialog.move_cli_picker_prev(),
        KeyCode::Enter => confirm_cli_picker(dialog),
        KeyCode::Esc => dialog.close_cli_picker(),
        KeyCode::Backspace => dialog.pop_cli_picker_filter(),
        KeyCode::Char(c) => dialog.push_cli_picker_filter(c),
        _ => {}
    }
}

fn confirm_cli_picker(dialog: &mut NewAgentDialog) {
    let filtered = dialog.filtered_cli_indices();
    let Some(&idx) = filtered.get(dialog.cli_picker_idx) else {
        dialog.close_cli_picker();
        return;
    };

    dialog.set_cli_index(idx);
    dialog.close_cli_picker();
}

fn handle_dialog_enter(app: &mut App) {
    {
        let Some(dialog) = app.new_agent_dialog.as_mut() else {
            return;
        };

        if should_open_session_picker(dialog) {
            dialog.open_session_picker();
            return;
        }
    }

    let _ = app.launch_new_agent();
}

fn should_open_session_picker(dialog: &NewAgentDialog) -> bool {
    matches!(dialog.task_type, NewTaskType::Interactive)
        && matches!(dialog.task_mode, NewTaskMode::Resume)
        && dialog.has_session_picker()
        && dialog.selected_session.is_none()
}

fn handle_dialog_field_key(
    term_width: u16,
    dialog: &mut NewAgentDialog,
    code: KeyCode,
    modifiers: KeyModifiers,
) {
    let fields = DialogFields::from(dialog);

    match dialog.field {
        0 => handle_type_field(dialog, code),
        1 if fields.is_interactive => handle_mode_field(dialog, code, fields),
        1 if fields.is_background => handle_trigger_field(dialog, code, fields),
        n if n == fields.cli_field && !fields.is_terminal => handle_cli_field(dialog, code, fields),
        n if n == fields.identity_field && fields.is_interactive => {
            handle_identity_field(dialog, code, fields);
        }
        n if n == fields.model_field && fields.is_background => {
            handle_model_field(dialog, code, fields);
        }
        4 if fields.is_background => {
            handle_prompt_field(term_width, dialog, code, fields, modifiers);
        }
        5 if fields.is_background
            && matches!(dialog.background_trigger, BackgroundTrigger::Cron) =>
        {
            handle_cron_field(dialog, code, fields);
        }
        n if n == fields.dir_field
            || (n == fields.extra_field && fields.is_watch_dir_field(dialog)) =>
        {
            handle_directory_field(dialog, code, fields, fields.is_watch_dir_field(dialog));
        }
        2 if fields.is_terminal => handle_shell_field(dialog, code, fields),
        n if n == fields.yolo_field && fields.is_interactive => {
            handle_yolo_field(dialog, code, fields);
        }
        _ => {}
    }
}

fn handle_type_field(dialog: &mut NewAgentDialog, code: KeyCode) {
    match code {
        KeyCode::Left => cycle_task_type(dialog, false),
        KeyCode::Right => cycle_task_type(dialog, true),
        KeyCode::Down | KeyCode::Tab => dialog.field = 1,
        _ => {}
    }
}

fn cycle_task_type(dialog: &mut NewAgentDialog, forward: bool) {
    dialog.task_type = match (dialog.task_type, forward) {
        (NewTaskType::Interactive, true) => NewTaskType::Terminal,
        (NewTaskType::Terminal, true) => NewTaskType::Background,
        (NewTaskType::Background, true) => NewTaskType::Interactive,
        (NewTaskType::Interactive, false) => NewTaskType::Background,
        (NewTaskType::Terminal, false) => NewTaskType::Interactive,
        (NewTaskType::Background, false) => NewTaskType::Terminal,
    };
    dialog.field = 0;
    dialog.refresh_dir_entries();
}

fn handle_mode_field(dialog: &mut NewAgentDialog, code: KeyCode, fields: DialogFields) {
    match code {
        KeyCode::Left | KeyCode::Right => toggle_task_mode(dialog),
        KeyCode::Delete | KeyCode::Backspace if matches!(dialog.task_mode, NewTaskMode::Resume) => {
            dialog.clear_selected_session();
        }
        KeyCode::Down | KeyCode::Tab => dialog.field = fields.cli_field,
        KeyCode::Up | KeyCode::BackTab => dialog.field = 0,
        _ => {}
    }
}

fn toggle_task_mode(dialog: &mut NewAgentDialog) {
    dialog.task_mode = match dialog.task_mode {
        NewTaskMode::Interactive => NewTaskMode::Resume,
        NewTaskMode::Resume => NewTaskMode::Interactive,
    };
    dialog.selected_session = None;
}

fn handle_trigger_field(dialog: &mut NewAgentDialog, code: KeyCode, fields: DialogFields) {
    match code {
        KeyCode::Left | KeyCode::Right => toggle_background_trigger(dialog),
        KeyCode::Down | KeyCode::Tab => dialog.field = fields.cli_field,
        KeyCode::Up | KeyCode::BackTab => dialog.field = 0,
        _ => {}
    }
}

fn toggle_background_trigger(dialog: &mut NewAgentDialog) {
    dialog.background_trigger = match dialog.background_trigger {
        BackgroundTrigger::Cron => BackgroundTrigger::Watch,
        BackgroundTrigger::Watch => BackgroundTrigger::Cron,
    };
    dialog.refresh_dir_entries();
}

fn handle_cli_field(dialog: &mut NewAgentDialog, code: KeyCode, fields: DialogFields) {
    match code {
        KeyCode::Char(' ') => dialog.open_cli_picker(),
        KeyCode::Char(c) => {
            dialog.open_cli_picker();
            dialog.push_cli_picker_filter(c);
        }
        KeyCode::Left | KeyCode::Right => {
            step_cli_selection(dialog, matches!(code, KeyCode::Right));
        }
        KeyCode::Down => {
            dialog.field = if fields.is_interactive {
                fields.identity_field
            } else {
                fields.model_field
            };
        }
        KeyCode::Up => dialog.field = fields.cli_up_target(),
        _ => {}
    }
}

fn step_cli_selection(dialog: &mut NewAgentDialog, forward: bool) {
    let Some(next) = wrapped_index(dialog.cli_index, dialog.available_clis.len(), forward) else {
        return;
    };
    dialog.set_cli_index(next);
}

fn handle_model_field(dialog: &mut NewAgentDialog, code: KeyCode, fields: DialogFields) {
    match code {
        KeyCode::Char(' ') => reopen_model_picker(dialog, true),
        KeyCode::Char(c) => {
            dialog.model.push(c);
            reopen_model_picker(dialog, true);
        }
        KeyCode::Backspace => {
            dialog.model.pop();
            reopen_model_picker(dialog, !dialog.model.is_empty());
        }
        KeyCode::Down if dialog.model_picker_open => move_model_picker(dialog, true),
        KeyCode::Up if dialog.model_picker_open => move_model_picker(dialog, false),
        KeyCode::Enter if dialog.model_picker_open => confirm_model_picker(dialog),
        KeyCode::Esc | KeyCode::Left if dialog.model_picker_open => {
            dialog.model_picker_open = false;
        }
        KeyCode::Up => {
            dialog.model_picker_open = false;
            dialog.field = fields.cli_field;
        }
        KeyCode::Down => {
            dialog.model_picker_open = false;
            dialog.field = fields.prompt_field;
        }
        _ => {}
    }
}

fn reopen_model_picker(dialog: &mut NewAgentDialog, open: bool) {
    dialog.model_picker_open = open;
    dialog.model_suggestion_idx = 0;
    dialog.refresh_model_suggestions();
}

fn move_model_picker(dialog: &mut NewAgentDialog, forward: bool) {
    let Some(next) = wrapped_index(
        dialog.model_suggestion_idx,
        dialog.model_suggestions.len(),
        forward,
    ) else {
        return;
    };
    dialog.model_suggestion_idx = next;
}

fn confirm_model_picker(dialog: &mut NewAgentDialog) {
    dialog.accept_model_suggestion();
    dialog.model_picker_open = false;
}

fn handle_prompt_field(
    term_width: u16,
    dialog: &mut NewAgentDialog,
    code: KeyCode,
    fields: DialogFields,
    modifiers: KeyModifiers,
) {
    // The prompt input is now multi-line + responsive: cursor can sit anywhere
    // in the string, scroll follows the cursor, and pasted multi-line text is
    // kept verbatim (rendered with hard breaks by the wrap helper).
    let field_width = prompt_field_width_for(term_width);
    let max_lines = prompt_visual_line_count(dialog, field_width);
    let char_len = dialog.prompt.chars().count();
    let cursor = dialog.prompt_cursor.min(char_len);

    match code {
        KeyCode::Char(c) => {
            insert_prompt_text(dialog, &c.to_string());
        }
        KeyCode::Backspace => backspace_prompt(dialog),
        KeyCode::Delete => delete_prompt_forward(dialog),
        KeyCode::Left => {
            if cursor > 0 {
                dialog.prompt_cursor = cursor - 1;
            }
        }
        KeyCode::Right => {
            if cursor < char_len {
                dialog.prompt_cursor = cursor + 1;
            }
        }
        KeyCode::Home => {
            dialog.prompt_cursor = start_of_visual_line(dialog, cursor, field_width);
        }
        KeyCode::End => {
            dialog.prompt_cursor = end_of_visual_line(dialog, cursor, field_width, char_len);
        }
        KeyCode::Enter if modifiers.contains(KeyModifiers::SHIFT) => {
            // Shift+Enter inserts a hard newline (Enter alone is reserved for
            // submitting the dialog from the top-level handler).
            insert_prompt_text(dialog, "\n");
        }
        KeyCode::Up => {
            if let Some(new_cursor) = move_prompt_visual(dialog, cursor, field_width, false) {
                dialog.prompt_cursor = new_cursor;
            } else {
                dialog.field = fields.model_field;
                return;
            }
        }
        KeyCode::Down => {
            if let Some(new_cursor) = move_prompt_visual(dialog, cursor, field_width, true) {
                dialog.prompt_cursor = new_cursor;
            } else {
                dialog.field = fields.extra_field;
                return;
            }
        }
        KeyCode::PageUp => {
            dialog.prompt_scroll = dialog.prompt_scroll.saturating_sub(PROMPT_VISIBLE_ROWS);
        }
        KeyCode::PageDown => {
            let max_scroll = max_lines.saturating_sub(PROMPT_VISIBLE_ROWS);
            dialog.prompt_scroll = (dialog.prompt_scroll + PROMPT_VISIBLE_ROWS).min(max_scroll);
        }
        _ => return,
    }

    // Recompute after mutation, then re-clamp the cursor and keep the
    // containing visual line in view.
    let char_len = dialog.prompt.chars().count();
    let max_lines = prompt_visual_line_count(dialog, field_width);
    dialog.prompt_cursor = dialog.prompt_cursor.min(char_len);
    let cursor_line = visual_line_of_char(dialog, dialog.prompt_cursor, field_width);
    let max_scroll = max_lines.saturating_sub(PROMPT_VISIBLE_ROWS);
    if cursor_line < dialog.prompt_scroll {
        dialog.prompt_scroll = cursor_line;
    } else if cursor_line >= dialog.prompt_scroll + PROMPT_VISIBLE_ROWS {
        dialog.prompt_scroll = cursor_line + 1 - PROMPT_VISIBLE_ROWS;
    }
    dialog.prompt_scroll = dialog.prompt_scroll.min(max_scroll);
    let _ = max_lines; // recomputed for max_scroll above
}

/// Insert `text` at the current cursor. Newlines are kept as hard breaks
/// (the renderer flushes the current line on `\n`); spaces are stored verbatim
/// so a paste that ends with a trailing space survives.
pub(crate) fn insert_prompt_text(dialog: &mut NewAgentDialog, text: &str) {
    if text.is_empty() {
        return;
    }
    let byte_index = char_to_byte_index(&dialog.prompt, dialog.prompt_cursor);
    dialog.prompt.insert_str(byte_index, text);
    dialog.prompt_cursor += text.chars().count();
}

fn backspace_prompt(dialog: &mut NewAgentDialog) {
    if dialog.prompt_cursor == 0 {
        return;
    }
    let start = char_to_byte_index(&dialog.prompt, dialog.prompt_cursor - 1);
    let end = char_to_byte_index(&dialog.prompt, dialog.prompt_cursor);
    dialog.prompt.replace_range(start..end, "");
    dialog.prompt_cursor -= 1;
}

fn delete_prompt_forward(dialog: &mut NewAgentDialog) {
    let char_len = dialog.prompt.chars().count();
    if dialog.prompt_cursor >= char_len {
        return;
    }
    let start = char_to_byte_index(&dialog.prompt, dialog.prompt_cursor);
    let end = char_to_byte_index(&dialog.prompt, dialog.prompt_cursor + 1);
    dialog.prompt.replace_range(start..end, "");
}

fn char_to_byte_index(s: &str, char_index: usize) -> usize {
    s.char_indices()
        .nth(char_index)
        .map(|(byte, _)| byte)
        .unwrap_or_else(|| s.len())
}

fn visual_line_of_char(dialog: &NewAgentDialog, char_index: usize, field_width: usize) -> usize {
    let field_width = field_width.max(1);
    let mut line = 0usize;
    let mut col = 0usize;
    for (i, ch) in dialog.prompt.chars().enumerate() {
        if i == char_index {
            // The char at `i` is the first on its line; resolve the wrap
            // that landed it on this line (if any) before returning.
            if col >= field_width {
                return line + 1;
            }
            return line;
        }
        if ch == '\n' {
            line += 1;
            col = 0;
            continue;
        }
        if col >= field_width {
            line += 1;
            col = 1;
        } else {
            col += 1;
        }
    }
    line
}

fn start_of_visual_line(dialog: &NewAgentDialog, char_index: usize, field_width: usize) -> usize {
    let field_width = field_width.max(1);
    let mut line_start = 0usize;
    let mut col = 0usize;
    for (i, ch) in dialog.prompt.chars().enumerate() {
        if i == char_index {
            return line_start;
        }
        if ch == '\n' {
            line_start = i + 1;
            col = 0;
            continue;
        }
        if col >= field_width {
            line_start = i;
            col = 1;
        } else {
            col += 1;
        }
    }
    line_start
}

fn end_of_visual_line(
    dialog: &NewAgentDialog,
    char_index: usize,
    field_width: usize,
    char_len: usize,
) -> usize {
    let field_width = field_width.max(1);
    let mut line_end = char_len;
    let mut col = 0usize;
    for (i, ch) in dialog.prompt.chars().enumerate() {
        if ch == '\n' {
            line_end = i;
            if i >= char_index {
                return line_end;
            }
            col = 0;
            line_end = char_len;
            continue;
        }
        if col >= field_width {
            line_end = i;
            if i >= char_index {
                return line_end;
            }
            col = 1;
            line_end = char_len;
        } else {
            col += 1;
        }
    }
    line_end
}

fn move_prompt_visual(
    dialog: &NewAgentDialog,
    cursor: usize,
    field_width: usize,
    forward: bool,
) -> Option<usize> {
    let char_len = dialog.prompt.chars().count();
    let current_line = visual_line_of_char(dialog, cursor, field_width);
    let total_lines = prompt_visual_line_count(dialog, field_width);
    if !forward && current_line == 0 {
        return None;
    }
    if forward && current_line + 1 >= total_lines {
        return None;
    }
    let target_line = if forward {
        current_line + 1
    } else {
        current_line - 1
    };
    let target_start = start_of_visual_line_at(dialog, target_line, field_width);
    let target_end = end_of_visual_line_at(dialog, target_line, field_width, char_len);
    let col_in_current =
        cursor.saturating_sub(start_of_visual_line_at(dialog, current_line, field_width));
    let target_width = target_end.saturating_sub(target_start);
    Some(target_start + col_in_current.min(target_width))
}

fn start_of_visual_line_at(
    dialog: &NewAgentDialog,
    target_line: usize,
    field_width: usize,
) -> usize {
    let field_width = field_width.max(1);
    let mut line = 0usize;
    let mut line_start = 0usize;
    let mut col = 0usize;
    for (i, ch) in dialog.prompt.chars().enumerate() {
        if line == target_line {
            return line_start;
        }
        if ch == '\n' {
            line += 1;
            line_start = i + 1;
            col = 0;
            continue;
        }
        if col >= field_width {
            line += 1;
            line_start = i;
            col = 1;
        } else {
            col += 1;
        }
    }
    line_start
}

fn end_of_visual_line_at(
    dialog: &NewAgentDialog,
    target_line: usize,
    field_width: usize,
    char_len: usize,
) -> usize {
    let field_width = field_width.max(1);
    let mut line = 0usize;
    let mut line_end = char_len;
    let mut col = 0usize;
    for (i, ch) in dialog.prompt.chars().enumerate() {
        if ch == '\n' {
            if line == target_line {
                return i;
            }
            line += 1;
            col = 0;
            line_end = char_len;
            continue;
        }
        if col >= field_width {
            if line == target_line {
                return i;
            }
            line += 1;
            col = 1;
            line_end = char_len;
        } else {
            col += 1;
        }
    }
    line_end
}

/// Mirrors the clamp the renderer uses (`prompt_field_width` in
/// `ui/dialogs/new_agent_dialog.rs`) so the input handler's cursor / scroll
/// math agrees with what the user sees on the screen.
pub(crate) fn prompt_field_width_for(term_width: u16) -> usize {
    let term_width = term_width.max(1);
    let max_dialog_w = term_width.saturating_sub(2).max(1);
    let preferred_dialog_w = term_width.saturating_mul(65) / 100;
    let min_dialog_w = 40u16.min(max_dialog_w);
    let dialog_width = preferred_dialog_w.clamp(min_dialog_w, max_dialog_w);
    (dialog_width.saturating_sub(5) as usize).max(10)
}

fn handle_cron_field(dialog: &mut NewAgentDialog, code: KeyCode, fields: DialogFields) {
    match code {
        KeyCode::Char(c) => dialog.cron_expr.push(c),
        KeyCode::Backspace => {
            dialog.cron_expr.pop();
        }
        KeyCode::Up => dialog.field = fields.prompt_field,
        KeyCode::Down => dialog.field = fields.dir_field,
        _ => {}
    }
}

fn handle_directory_field(
    dialog: &mut NewAgentDialog,
    code: KeyCode,
    fields: DialogFields,
    is_watch_dir: bool,
) {
    match code {
        KeyCode::Up => move_directory_up(dialog, fields, is_watch_dir),
        KeyCode::Down => move_directory_down(dialog),
        KeyCode::BackTab => dialog.field = fields.previous_dir_field(is_watch_dir),
        KeyCode::Tab => dialog.field = fields.next_dir_field(dialog.field),
        KeyCode::Right => dialog.navigate_to_selected(),
        KeyCode::Left => dialog.go_up(),
        KeyCode::Backspace if !dialog.dir_filter.is_empty() => {
            dialog.dir_filter.pop();
            dialog.dir_selected = 0;
        }
        KeyCode::Char(c) => {
            dialog.dir_filter.push(c);
            dialog.dir_selected = 0;
        }
        _ => {}
    }
}

fn move_directory_up(dialog: &mut NewAgentDialog, fields: DialogFields, is_watch_dir: bool) {
    if dialog.dir_selected > 0 {
        dialog.dir_selected -= 1;
        dialog.update_dir_preview();
        return;
    }

    dialog.field = fields.previous_dir_field(is_watch_dir);
}

fn move_directory_down(dialog: &mut NewAgentDialog) {
    let filtered_len = dialog.filtered_dir_entries().len();
    if filtered_len > 0 && dialog.dir_selected + 1 < filtered_len {
        dialog.dir_selected += 1;
        dialog.update_dir_preview();
    }
}

fn handle_shell_field(dialog: &mut NewAgentDialog, code: KeyCode, fields: DialogFields) {
    match code {
        KeyCode::Left | KeyCode::Right => {
            step_shell_selection(dialog, matches!(code, KeyCode::Right));
        }
        KeyCode::Up | KeyCode::BackTab => dialog.field = fields.dir_field,
        _ => {}
    }
}

fn step_shell_selection(dialog: &mut NewAgentDialog, forward: bool) {
    let Some(next) = wrapped_index(dialog.shell_index, dialog.available_shells.len(), forward)
    else {
        return;
    };
    dialog.shell_index = next;
}

fn handle_yolo_field(dialog: &mut NewAgentDialog, code: KeyCode, fields: DialogFields) {
    match code {
        KeyCode::Char(' ') if dialog.selected_yolo_flag().is_some() => {
            dialog.yolo_mode = !dialog.yolo_mode;
        }
        KeyCode::Char(' ') => {}
        KeyCode::Up | KeyCode::BackTab => dialog.field = fields.identity_field,
        KeyCode::Down | KeyCode::Tab => dialog.field = fields.dir_field,
        _ => {}
    }
}

fn handle_identity_field(dialog: &mut NewAgentDialog, code: KeyCode, fields: DialogFields) {
    match code {
        KeyCode::Left => step_seed_selection(dialog, false),
        KeyCode::Right => step_seed_selection(dialog, true),
        KeyCode::Up | KeyCode::BackTab => dialog.field = fields.cli_field,
        KeyCode::Down | KeyCode::Tab => dialog.field = fields.yolo_field,
        _ => {}
    }
}

fn step_seed_selection(dialog: &mut NewAgentDialog, forward: bool) {
    let Some(next) = wrapped_index(dialog.seed_index, dialog.seed_options.len(), forward) else {
        return;
    };
    dialog.seed_index = next;
}

fn wrapped_index(current: usize, len: usize, forward: bool) -> Option<usize> {
    if len == 0 {
        return None;
    }
    Some(if forward {
        (current + 1) % len
    } else {
        current.checked_sub(1).unwrap_or(len - 1)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::app::dialog::new_agent::NewAgentDialog;

    fn dialog_with(prompt: &str) -> NewAgentDialog {
        let mut d = NewAgentDialog::new(Some("."));
        d.prompt = prompt.to_string();
        d.prompt_cursor = prompt.chars().count();
        d
    }

    #[test]
    fn insert_text_appends_and_moves_cursor() {
        let mut d = dialog_with("foo");
        insert_prompt_text(&mut d, "bar");
        assert_eq!(d.prompt, "foobar");
        assert_eq!(d.prompt_cursor, 6);
    }

    #[test]
    fn insert_text_at_middle_splits_string() {
        let mut d = dialog_with("fooo"); // cursor at end (4)
        d.prompt_cursor = 2;
        insert_prompt_text(&mut d, "X");
        assert_eq!(d.prompt, "foXoo");
        assert_eq!(d.prompt_cursor, 3);
    }

    #[test]
    fn backspace_removes_previous_char() {
        let mut d = dialog_with("foo");
        backspace_prompt(&mut d);
        assert_eq!(d.prompt, "fo");
        assert_eq!(d.prompt_cursor, 2);
    }

    #[test]
    fn backspace_at_start_is_noop() {
        let mut d = dialog_with("foo");
        d.prompt_cursor = 0;
        backspace_prompt(&mut d);
        assert_eq!(d.prompt, "foo");
        assert_eq!(d.prompt_cursor, 0);
    }

    #[test]
    fn delete_forward_removes_next_char() {
        let mut d = dialog_with("foo");
        d.prompt_cursor = 1;
        delete_prompt_forward(&mut d);
        assert_eq!(d.prompt, "fo");
        assert_eq!(d.prompt_cursor, 1);
    }

    #[test]
    fn paste_with_newlines_is_preserved() {
        let mut d = dialog_with("");
        insert_prompt_text(&mut d, "line1\nline2\nline3");
        assert_eq!(d.prompt, "line1\nline2\nline3");
        assert_eq!(d.prompt_cursor, 17);
    }

    #[test]
    fn paste_with_trailing_space_is_preserved() {
        let mut d = dialog_with("");
        insert_prompt_text(&mut d, "hello ");
        assert_eq!(d.prompt, "hello ");
        assert_eq!(d.prompt_cursor, 6);
    }

    #[test]
    fn visual_line_of_char_handles_soft_wrap() {
        let d = dialog_with("abcdefghij");
        assert_eq!(visual_line_of_char(&d, 0, 4), 0);
        assert_eq!(visual_line_of_char(&d, 4, 4), 1);
        assert_eq!(visual_line_of_char(&d, 9, 4), 2);
    }

    #[test]
    fn visual_line_of_char_handles_hard_newline() {
        let d = dialog_with("abc\ndef");
        assert_eq!(visual_line_of_char(&d, 2, 30), 0); // 'c' on line 0
        assert_eq!(visual_line_of_char(&d, 3, 30), 0); // '\n' position still on line 0
        assert_eq!(visual_line_of_char(&d, 4, 30), 1); // 'd' on line 1
    }

    #[test]
    fn move_visual_clamps_to_shorter_line() {
        let d = dialog_with("abcdefghij\nab");
        // cursor at 'j' on line 2 (pos 9, col 1 of line 2)
        // moving down should clamp to col 2 of line 3 (end of "ab")
        let new_cursor = move_prompt_visual(&d, 9, 4, true).expect("should move down");
        assert_eq!(new_cursor, 12); // "ab" ends at char index 12
    }

    #[test]
    fn move_visual_returns_none_at_first_line_up() {
        let d = dialog_with("hello");
        assert!(move_prompt_visual(&d, 3, 30, false).is_none());
    }

    #[test]
    fn move_visual_returns_none_at_last_line_down() {
        let d = dialog_with("hello");
        assert!(move_prompt_visual(&d, 3, 30, true).is_none());
    }

    #[test]
    fn home_and_end_jump_to_line_bounds() {
        let d = dialog_with("abcdefghij");
        // at pos 7, line 1, col 3
        let start = start_of_visual_line(&d, 7, 4);
        assert_eq!(start, 4);
        let end = end_of_visual_line(&d, 7, 4, 10);
        assert_eq!(end, 8);
    }

    #[test]
    fn char_to_byte_index_ascii() {
        assert_eq!(char_to_byte_index("hello", 0), 0);
        assert_eq!(char_to_byte_index("hello", 3), 3);
        assert_eq!(char_to_byte_index("hello", 5), 5);
    }

    #[test]
    fn char_to_byte_index_beyond_end() {
        assert_eq!(char_to_byte_index("hi", 10), 2);
    }

    #[test]
    fn char_to_byte_index_empty() {
        assert_eq!(char_to_byte_index("", 0), 0);
    }

    #[test]
    fn char_to_byte_index_multibyte() {
        let s = "café"; // é is 2 bytes in UTF-8
        assert_eq!(char_to_byte_index(s, 0), 0);
        assert_eq!(char_to_byte_index(s, 3), 3); // start of é
        assert_eq!(char_to_byte_index(s, 4), 5); // after é
    }

    #[test]
    fn visual_line_of_char_single_line() {
        let d = dialog_with("hello");
        assert_eq!(visual_line_of_char(&d, 0, 10), 0);
        assert_eq!(visual_line_of_char(&d, 4, 10), 0);
    }

    #[test]
    fn visual_line_of_char_at_end() {
        let d = dialog_with("abc");
        assert_eq!(visual_line_of_char(&d, 3, 10), 0);
    }

    #[test]
    fn visual_line_of_char_empty() {
        let d = dialog_with("");
        assert_eq!(visual_line_of_char(&d, 0, 10), 0);
    }

    #[test]
    fn visual_line_of_char_exact_boundary() {
        let d = dialog_with("abcdefgh");
        // field_width=4: chars 0-3 on line 0, chars 4-7 on line 1
        assert_eq!(visual_line_of_char(&d, 3, 4), 0);
        assert_eq!(visual_line_of_char(&d, 4, 4), 1);
    }

    #[test]
    fn start_of_visual_line_first_line() {
        let d = dialog_with("hello");
        assert_eq!(start_of_visual_line(&d, 2, 10), 0);
    }

    #[test]
    fn start_of_visual_line_wrapped() {
        let d = dialog_with("abcdefghij");
        // field_width=4: line 0 starts at 0, line 1 starts at 4
        assert_eq!(start_of_visual_line(&d, 5, 4), 4);
    }

    #[test]
    fn start_of_visual_line_at_beginning() {
        let d = dialog_with("hello");
        assert_eq!(start_of_visual_line(&d, 0, 10), 0);
    }

    #[test]
    fn end_of_visual_line_first_line() {
        let d = dialog_with("hello");
        assert_eq!(end_of_visual_line(&d, 2, 10, 5), 5);
    }

    #[test]
    fn end_of_visual_line_wrapped() {
        let d = dialog_with("abcdefghij");
        // field_width=4: line 0 ends at 4, line 1 ends at 8
        assert_eq!(end_of_visual_line(&d, 5, 4, 10), 8);
    }

    #[test]
    fn end_of_visual_line_at_end() {
        let d = dialog_with("abc");
        assert_eq!(end_of_visual_line(&d, 0, 10, 3), 3);
    }

    #[test]
    fn move_visual_down_within_line() {
        let d = dialog_with("abcdefghij");
        // cursor at col 0 of line 0, move down goes to col 0 of line 1
        let new_cursor = move_prompt_visual(&d, 0, 4, true).expect("should move");
        assert_eq!(new_cursor, 4);
    }

    #[test]
    fn move_visual_up_from_second_line() {
        let d = dialog_with("abcdefghij");
        // cursor at col 0 of line 1 (pos 4), move up goes to col 0 of line 0
        let new_cursor = move_prompt_visual(&d, 4, 4, false).expect("should move");
        assert_eq!(new_cursor, 0);
    }

    #[test]
    fn prompt_field_width_for_wide_terminal() {
        let width = prompt_field_width_for(200);
        assert!(width > 40);
        assert!(width < 200);
    }

    #[test]
    fn prompt_field_width_for_narrow_terminal() {
        let width = prompt_field_width_for(20);
        assert!(width >= 10);
    }

    #[test]
    fn prompt_field_width_for_minimum() {
        let width = prompt_field_width_for(0);
        assert!(width >= 10);
    }

    #[test]
    fn insert_text_empty_into_empty() {
        let mut d = dialog_with("");
        insert_prompt_text(&mut d, "");
        assert_eq!(d.prompt, "");
        assert_eq!(d.prompt_cursor, 0);
    }

    #[test]
    fn delete_forward_at_end() {
        let mut d = dialog_with("abc");
        d.prompt_cursor = 3;
        delete_prompt_forward(&mut d);
        assert_eq!(d.prompt, "abc");
    }

    #[test]
    fn delete_forward_empty() {
        let mut d = dialog_with("");
        delete_prompt_forward(&mut d);
        assert_eq!(d.prompt, "");
    }

    #[test]
    fn backspace_empty() {
        let mut d = dialog_with("");
        backspace_prompt(&mut d);
        assert_eq!(d.prompt, "");
    }

    #[test]
    fn insert_text_unicode() {
        let mut d = dialog_with("");
        insert_prompt_text(&mut d, "café");
        assert_eq!(d.prompt, "café");
        assert_eq!(d.prompt_cursor, 4);
    }

    #[test]
    fn visual_line_of_char_multibyte() {
        let d = dialog_with("caféxyz");
        // field_width=4: c(0) a(1) f(2) é(3) on line 0, x(4) y(5) z(6) on line 1
        assert_eq!(visual_line_of_char(&d, 4, 4), 1); // x is on line 1
    }

    #[test]
    fn wrapped_index_empty_len() {
        assert!(wrapped_index(0, 0, true).is_none());
        assert!(wrapped_index(0, 0, false).is_none());
    }

    #[test]
    fn wrapped_index_single_element() {
        assert_eq!(wrapped_index(0, 1, true), Some(0));
        assert_eq!(wrapped_index(0, 1, false), Some(0));
    }

    #[test]
    fn wrapped_index_forward_wraps() {
        assert_eq!(wrapped_index(2, 3, true), Some(0));
        assert_eq!(wrapped_index(0, 3, true), Some(1));
    }

    #[test]
    fn wrapped_index_backward_wraps() {
        assert_eq!(wrapped_index(0, 3, false), Some(2));
        assert_eq!(wrapped_index(2, 3, false), Some(1));
    }

    #[test]
    fn visual_line_of_char_at_line_zero() {
        let d = dialog_with("hello world");
        assert_eq!(start_of_visual_line_at(&d, 0, 30), 0);
    }

    #[test]
    fn visual_line_of_char_at_with_wrap() {
        let d = dialog_with("abcdefghij");
        // field_width=4: line 0 starts at 0, line 1 starts at 4, line 2 starts at 8
        assert_eq!(start_of_visual_line_at(&d, 0, 4), 0);
        assert_eq!(start_of_visual_line_at(&d, 1, 4), 4);
        assert_eq!(start_of_visual_line_at(&d, 2, 4), 8);
    }

    #[test]
    fn end_of_visual_line_at_line_zero() {
        let d = dialog_with("hello world");
        assert_eq!(end_of_visual_line_at(&d, 0, 30, 11), 11);
    }

    #[test]
    fn end_of_visual_line_at_with_wrap() {
        let d = dialog_with("abcdefghij");
        assert_eq!(end_of_visual_line_at(&d, 0, 4, 10), 4);
        assert_eq!(end_of_visual_line_at(&d, 1, 4, 10), 8);
        assert_eq!(end_of_visual_line_at(&d, 2, 4, 10), 10);
    }

    #[test]
    fn start_of_visual_line_at_beyond_last_line() {
        let d = dialog_with("abc");
        // field_width=30 → 1 line. Asking for line 1 returns start of string (no wrapping).
        assert_eq!(start_of_visual_line_at(&d, 1, 30), 0);
    }

    #[test]
    fn end_of_visual_line_at_beyond_last_line() {
        let d = dialog_with("abc");
        assert_eq!(end_of_visual_line_at(&d, 1, 30, 3), 3);
    }

    #[test]
    fn prompt_field_width_for_80_col() {
        let width = prompt_field_width_for(80);
        // Should be reasonable: between 10 and 80
        assert!(width >= 10);
        assert!(width < 80);
    }

    #[test]
    fn prompt_field_width_for_40_col() {
        let width = prompt_field_width_for(40);
        assert!(width >= 10);
    }

    #[test]
    fn prompt_field_width_for_1_col() {
        let width = prompt_field_width_for(1);
        assert!(width >= 10);
    }

    #[test]
    fn prompt_field_width_for_100_col() {
        let width = prompt_field_width_for(100);
        assert!(width >= 10);
        assert!(width < 100);
    }

    #[test]
    fn visual_line_of_char_with_hard_newlines_and_wrap() {
        let d = dialog_with("ab\ncdef");
        // field_width=3: "ab" on line 0, "\n" at pos 2, "c" at pos 3 is line 1
        assert_eq!(visual_line_of_char(&d, 0, 3), 0); // 'a'
        assert_eq!(visual_line_of_char(&d, 2, 3), 0); // '\n'
        assert_eq!(visual_line_of_char(&d, 3, 3), 1); // 'c'
        assert_eq!(visual_line_of_char(&d, 4, 3), 1); // 'd'
    }

    #[test]
    fn move_prompt_visual_up_from_first_line_returns_none() {
        let d = dialog_with("hello");
        assert!(move_prompt_visual(&d, 0, 30, false).is_none());
    }

    #[test]
    fn move_prompt_visual_down_from_last_line_returns_none() {
        let d = dialog_with("ab");
        assert!(move_prompt_visual(&d, 1, 30, true).is_none());
    }

    #[test]
    fn move_prompt_visual_down_through_wrap() {
        let d = dialog_with("abcdefghij");
        // cursor at col 2 of line 0 (pos 2), move down → line 1 col 2 (pos 6)
        let new_cursor = move_prompt_visual(&d, 2, 4, true).expect("should move");
        assert_eq!(new_cursor, 6);
    }

    #[test]
    fn move_prompt_visual_up_through_wrap() {
        let d = dialog_with("abcdefghij");
        // cursor at col 2 of line 1 (pos 6), move up → line 0 col 2 (pos 2)
        let new_cursor = move_prompt_visual(&d, 6, 4, false).expect("should move");
        assert_eq!(new_cursor, 2);
    }

    #[test]
    fn end_of_visual_line_multiline() {
        let d = dialog_with("line1\nline2");
        // at char 7 (pos in "line2"), field_width=30
        let end = end_of_visual_line(&d, 7, 30, 11);
        assert_eq!(end, 11);
    }

    #[test]
    fn start_of_visual_line_multiline() {
        let d = dialog_with("line1\nline2");
        // at char 7 (in "line2")
        let start = start_of_visual_line(&d, 7, 30);
        assert_eq!(start, 6); // after the '\n'
    }
}
