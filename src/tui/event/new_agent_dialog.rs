use anyhow::Result;
use ratatui::crossterm::event::KeyCode;

use crate::tui::app::dialog::{BackgroundTrigger, NewAgentDialog, NewTaskMode, NewTaskType};
use crate::tui::app::types::App;

// ── Dialog: new agent creation ──────────────────────────────────────
//
// Flow: ↑↓ switch fields, ←→ choose CLI/type/mode, ↑↓ in dir browser,
//       Space enter directory, Enter launch, Esc cancel.

pub fn handle_dialog_key(app: &mut App, code: KeyCode) -> Result<()> {
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
        KeyCode::Enter => handle_dialog_enter(app),
        _ => {
            let Some(dialog) = app.new_agent_dialog.as_mut() else {
                return Ok(());
            };
            handle_dialog_field_key(dialog, code);
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
            model_field: 3,
            prompt_field: 4,
            extra_field: 5,
            dir_field: if is_interactive {
                3
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
            self.cli_field
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

fn handle_dialog_field_key(dialog: &mut NewAgentDialog, code: KeyCode) {
    let fields = DialogFields::from(dialog);

    match dialog.field {
        0 => handle_type_field(dialog, code),
        1 if fields.is_interactive => handle_mode_field(dialog, code, fields),
        1 if fields.is_background => handle_trigger_field(dialog, code, fields),
        n if n == fields.cli_field && !fields.is_terminal => handle_cli_field(dialog, code, fields),
        n if n == fields.model_field && fields.is_background => {
            handle_model_field(dialog, code, fields);
        }
        4 if fields.is_background => handle_prompt_field(dialog, code, fields),
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
                fields.yolo_field
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
        KeyCode::Right if dialog.model_picker_open => dialog.accept_model_suggestion(),
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

fn handle_prompt_field(dialog: &mut NewAgentDialog, code: KeyCode, fields: DialogFields) {
    match code {
        KeyCode::Char(c) => dialog.prompt.push(c),
        KeyCode::Backspace => {
            dialog.prompt.pop();
        }
        KeyCode::Up => dialog.field = fields.model_field,
        KeyCode::Down => dialog.field = fields.extra_field,
        _ => {}
    }
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
        KeyCode::Up | KeyCode::BackTab => dialog.field = fields.cli_field,
        KeyCode::Down | KeyCode::Tab => dialog.field = fields.dir_field,
        _ => {}
    }
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
