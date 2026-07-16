use anyhow::Result;
use ratatui::crossterm::event::{KeyCode, KeyModifiers};
use std::path::{Path, PathBuf};

use crate::db::Database;
use crate::tui::app::dialog::{AtPicker, SectionPickerMode, SimplePromptDialog};
use crate::tui::app::types::{AgentEntry, App, Focus};

enum PromptAction {
    None,
    Close,
    Send(String),
    /// Schedule the prompt for later delivery at the given local date-time
    /// (picked in the builder's send control, U11).
    ScheduleSend(String, chrono::NaiveDateTime),
    /// Cancel the soonest pending scheduled send for the current target session.
    CancelNextScheduled,
    /// Recall the current project's last prompt (Ctrl+L).
    RecallLastPrompt,
}

pub fn handle_prompt_template_key(
    app: &mut App,
    code: KeyCode,
    modifiers: KeyModifiers,
) -> Result<()> {
    let field_width = prompt_field_width(app);
    let db = app.db.clone();
    let workdir = resolve_picker_workdir(app);

    let Some(dialog) = app.simple_prompt_dialog.as_mut() else {
        app.focus = Focus::Agent;
        return Ok(());
    };

    // Recall confirm overlay intercepts all keys while it's up (standard
    // confirm pattern: y/Enter confirms, n/Esc cancels, anything else is a
    // no-op).
    if dialog.pending_recall.is_some() {
        match code {
            KeyCode::Char('y') | KeyCode::Enter => {
                if let Some(last) = dialog.pending_recall.take() {
                    apply_last_prompt(dialog, &last);
                }
            }
            KeyCode::Char('n') | KeyCode::Esc => {
                dialog.pending_recall = None;
            }
            _ => {}
        }
        return Ok(());
    }

    // Ctrl+E toggles between the Normal and Raw tabs from any focus. Entering
    // the Raw tab (re)computes the composed-prompt preview for its empty state.
    if code == KeyCode::Char('e') && modifiers.contains(KeyModifiers::CONTROL) {
        let entering_raw = dialog.active_tab == crate::tui::app::dialog::PromptTab::Normal;
        dialog.toggle_tab();
        if entering_raw {
            dialog.refresh_raw_preview(&db, &workdir);
        }
        return Ok(());
    }

    let action = if dialog.active_tab == crate::tui::app::dialog::PromptTab::Raw {
        handle_raw_tab_key(
            dialog,
            code,
            modifiers,
            field_width,
            &db,
            &workdir,
            app.keyboard_enhancement_active,
        )
    } else {
        if handle_section_picker_key(dialog, &db, &workdir, code)? {
            return Ok(());
        }

        // Determine the focused section name (None when send_at is focused).
        let section_name = focused_section_name(dialog);

        // Only expand collapsed pastes when entering text or doing edit
        // operations (not for navigation keys like arrows, tab, etc.)
        if should_expand_on_key(code, modifiers) {
            if let Some(ref name) = section_name {
                dialog.expand_collapsed_paste(name);
            }
        }

        if let Some(ref name) = section_name {
            if handle_at_picker_key(dialog, code, modifiers, name, field_width) {
                return Ok(());
            }
        }

        handle_dialog_key(
            dialog,
            code,
            modifiers,
            section_name.as_deref(),
            field_width,
            &db,
            &workdir,
            app.keyboard_enhancement_active,
        )?
    };

    match action {
        PromptAction::None => {}
        PromptAction::Close => app.close_simple_prompt_dialog(),
        PromptAction::Send(prompt) => submit_prompt(app, &prompt),
        PromptAction::ScheduleSend(prompt, when) => {
            schedule_send_prompt(app, &prompt, when);
        }
        PromptAction::CancelNextScheduled => cancel_next_scheduled_send(app),
        PromptAction::RecallLastPrompt => recall_last_prompt(app),
    }

    Ok(())
}

pub(crate) fn prompt_field_width(app: &App) -> usize {
    // Must mirror the render calculation in ui/dialogs/simple_prompt.rs exactly,
    // including the clamp to terminal width, or cursor/scroll math drifts from
    // what is drawn on narrow terminals.
    let term_width = app.term_width;
    let max_dialog_w = term_width.saturating_sub(2).max(1);
    let preferred_dialog_w = term_width.saturating_mul(65) / 100;
    let min_dialog_w = 40u16.min(max_dialog_w);
    let dialog_width = preferred_dialog_w.clamp(min_dialog_w, max_dialog_w);
    (dialog_width.saturating_sub(4) as usize).max(10)
}

fn resolve_picker_workdir(app: &App) -> PathBuf {
    app.selected_agent()
        .and_then(|agent| match agent {
            AgentEntry::Interactive(idx) => app
                .interactive_agents
                .get(*idx)
                .map(|interactive| PathBuf::from(&interactive.working_dir)),
            _ => None,
        })
        .unwrap_or_else(|| app.data_dir.parent().unwrap_or(&app.data_dir).to_path_buf())
}

fn focused_section_name(dialog: &mut SimplePromptDialog) -> Option<String> {
    // Focus index 0 is the send control — it is a real focus target, not a
    // section. The pre-U11 version of this function clamped focus 0 back to
    // section 1 on every keypress, which made the send control impossible
    // to operate (its advertised keys never fired).
    if dialog.focused_section == 0 {
        return None;
    }
    let last_index = dialog.enabled_sections.len().checked_sub(1)?;

    // Clamp a stale over-the-end focus (e.g. after removing a section).
    let section_idx = (dialog.focused_section - 1).min(last_index);
    dialog.focused_section = section_idx + 1; // keep the offset
    dialog.enabled_sections.get(section_idx).cloned()
}

/// Determine if a key press should trigger expansion of a collapsed paste.
/// Only text input and specific edit operations should expand; navigation keys should not.
fn should_expand_on_key(code: KeyCode, modifiers: KeyModifiers) -> bool {
    match code {
        // Text input characters should expand
        KeyCode::Char(_) => true,
        // Backspace and Delete should expand (to handle edit operations)
        KeyCode::Backspace | KeyCode::Delete => true,
        // Enter should expand for instruction sections
        KeyCode::Enter => !modifiers.is_empty(),
        // Navigation keys should NOT expand
        KeyCode::Up | KeyCode::Down | KeyCode::Left | KeyCode::Right => false,
        KeyCode::Tab | KeyCode::BackTab => false,
        // Everything else: don't expand
        _ => false,
    }
}

fn handle_section_picker_key(
    dialog: &mut SimplePromptDialog,
    db: &Database,
    workdir: &Path,
    code: KeyCode,
) -> Result<bool> {
    match &dialog.picker_mode {
        SectionPickerMode::None => Ok(false),
        SectionPickerMode::AddSection { selected } => {
            handle_add_section_picker_key(dialog, *selected, db, workdir, code)?;
            Ok(true)
        }
        SectionPickerMode::AddCustom { input } => {
            handle_add_custom_section_key(dialog, input.clone(), code);
            Ok(true)
        }
        SectionPickerMode::RemoveSection { selected } => {
            handle_remove_section_picker_key(dialog, *selected, code);
            Ok(true)
        }
        SectionPickerMode::SkillsPicker {
            selected, entries, ..
        } => {
            handle_skills_picker_key(dialog, *selected, entries.len(), code);
            Ok(true)
        }
        SectionPickerMode::ProjectPicker { selected, entries } => {
            handle_project_picker_key(dialog, *selected, entries.len(), code);
            Ok(true)
        }
    }
}

fn handle_add_section_picker_key(
    dialog: &mut SimplePromptDialog,
    selected: usize,
    db: &Database,
    workdir: &Path,
    code: KeyCode,
) -> Result<()> {
    match code {
        KeyCode::Esc => dialog.picker_mode = SectionPickerMode::None,
        KeyCode::Up if selected > 0 => {
            dialog.picker_mode = SectionPickerMode::AddSection {
                selected: selected - 1,
            };
        }
        KeyCode::Down => move_add_section_picker_down(dialog, selected),
        KeyCode::Enter => select_addable_section(dialog, selected, db, workdir)?,
        KeyCode::Char('c') => {
            dialog.picker_mode = SectionPickerMode::AddCustom {
                input: String::new(),
            };
        }
        _ => {}
    }

    Ok(())
}

fn move_add_section_picker_down(dialog: &mut SimplePromptDialog, selected: usize) {
    let addable = dialog.get_addable_sections();
    if selected + 1 >= addable.len() {
        return;
    }

    dialog.picker_mode = SectionPickerMode::AddSection {
        selected: selected + 1,
    };
}

fn select_addable_section(
    dialog: &mut SimplePromptDialog,
    selected: usize,
    db: &Database,
    workdir: &Path,
) -> Result<()> {
    let addable = dialog.get_addable_sections();
    let Some((name, _)) = addable.get(selected).copied() else {
        return Ok(());
    };

    match name {
        "tools" => open_skills_picker(dialog, workdir),
        "project_context" => open_project_picker(dialog, db)?,
        _ => {
            dialog.add_section(name);
            dialog.picker_mode = SectionPickerMode::None;
        }
    }

    Ok(())
}

fn open_skills_picker(dialog: &mut SimplePromptDialog, workdir: &Path) {
    let entries = SimplePromptDialog::collect_skills_for_picker(workdir);
    dialog.picker_mode = SectionPickerMode::SkillsPicker {
        selected: 0,
        entries,
        replace_id: None,
    };
}

fn open_project_picker(dialog: &mut SimplePromptDialog, db: &Database) -> Result<()> {
    let entries = SimplePromptDialog::collect_projects_for_picker(db)?;
    dialog.picker_mode = SectionPickerMode::ProjectPicker {
        selected: 0,
        entries,
    };
    Ok(())
}

fn handle_add_custom_section_key(
    dialog: &mut SimplePromptDialog,
    mut input: String,
    code: KeyCode,
) {
    match code {
        KeyCode::Esc => dialog.picker_mode = SectionPickerMode::None,
        KeyCode::Enter => {
            if input.is_empty() || dialog.enabled_sections.contains(&input) {
                return;
            }

            dialog.add_section(&input);
            dialog.picker_mode = SectionPickerMode::None;
        }
        KeyCode::Char(c) => {
            input.push(c);
            dialog.picker_mode = SectionPickerMode::AddCustom { input };
        }
        KeyCode::Backspace => {
            input.pop();
            dialog.picker_mode = SectionPickerMode::AddCustom { input };
        }
        _ => {}
    }
}

fn handle_remove_section_picker_key(
    dialog: &mut SimplePromptDialog,
    selected: usize,
    code: KeyCode,
) {
    match code {
        KeyCode::Esc => dialog.picker_mode = SectionPickerMode::None,
        KeyCode::Up if selected > 0 => {
            dialog.picker_mode = SectionPickerMode::RemoveSection {
                selected: selected - 1,
            };
        }
        KeyCode::Down => move_remove_section_picker_down(dialog, selected),
        KeyCode::Enter => select_removable_section(dialog, selected),
        _ => {}
    }
}

fn move_remove_section_picker_down(dialog: &mut SimplePromptDialog, selected: usize) {
    let removable = dialog.get_removable_sections();
    if selected + 1 >= removable.len() {
        return;
    }

    dialog.picker_mode = SectionPickerMode::RemoveSection {
        selected: selected + 1,
    };
}

fn select_removable_section(dialog: &mut SimplePromptDialog, selected: usize) {
    let removable = dialog.get_removable_sections();
    let Some((section_id, _)) = removable.get(selected) else {
        return;
    };

    dialog.remove_section(section_id);
    dialog.picker_mode = SectionPickerMode::None;
}

fn handle_skills_picker_key(
    dialog: &mut SimplePromptDialog,
    selected: usize,
    count: usize,
    code: KeyCode,
) {
    match code {
        KeyCode::Esc => dialog.picker_mode = SectionPickerMode::None,
        KeyCode::Up if selected > 0 => set_skills_picker_selection(dialog, selected - 1),
        KeyCode::Down if selected + 1 < count => set_skills_picker_selection(dialog, selected + 1),
        KeyCode::Enter | KeyCode::Tab => confirm_skills_picker_selection(dialog),
        _ => {}
    }
}

fn set_skills_picker_selection(dialog: &mut SimplePromptDialog, selected: usize) {
    if let SectionPickerMode::SkillsPicker {
        selected: current, ..
    } = &mut dialog.picker_mode
    {
        *current = selected;
    }
}

fn confirm_skills_picker_selection(dialog: &mut SimplePromptDialog) {
    let SectionPickerMode::SkillsPicker {
        entries,
        selected,
        replace_id,
    } = std::mem::replace(&mut dialog.picker_mode, SectionPickerMode::None)
    else {
        return;
    };

    let Some((label, _, _)) = entries.get(selected) else {
        return;
    };

    match replace_id {
        Some(section_id) => dialog.set_tools_section_skill(&section_id, label),
        None => {
            dialog.add_section_with_content("tools", label.clone());
        }
    }
}

fn handle_project_picker_key(
    dialog: &mut SimplePromptDialog,
    selected: usize,
    count: usize,
    code: KeyCode,
) {
    match code {
        KeyCode::Esc => dialog.picker_mode = SectionPickerMode::None,
        KeyCode::Up if selected > 0 => set_project_picker_selection(dialog, selected - 1),
        KeyCode::Down if selected + 1 < count => set_project_picker_selection(dialog, selected + 1),
        KeyCode::Enter | KeyCode::Tab => confirm_project_picker_selection(dialog),
        _ => {}
    }
}

fn set_project_picker_selection(dialog: &mut SimplePromptDialog, selected: usize) {
    if let SectionPickerMode::ProjectPicker {
        selected: current, ..
    } = &mut dialog.picker_mode
    {
        *current = selected;
    }
}

fn confirm_project_picker_selection(dialog: &mut SimplePromptDialog) {
    let SectionPickerMode::ProjectPicker { entries, selected } =
        std::mem::replace(&mut dialog.picker_mode, SectionPickerMode::None)
    else {
        return;
    };

    let Some(project) = entries.get(selected) else {
        return;
    };

    dialog.add_section_with_content("project_context", project.path.clone());
}

fn handle_at_picker_key(
    dialog: &mut SimplePromptDialog,
    code: KeyCode,
    modifiers: KeyModifiers,
    section_name: &str,
    field_width: usize,
) -> bool {
    if dialog.at_picker.is_none() {
        return false;
    }

    match code {
        KeyCode::Esc => dialog.at_picker = None,
        KeyCode::Up => move_at_picker_up(dialog),
        KeyCode::Down => move_at_picker_down(dialog),
        KeyCode::Left => go_up_at_picker_dir(dialog),
        KeyCode::Right => enter_selected_at_picker_dir(dialog),
        KeyCode::Enter | KeyCode::Tab => {
            apply_at_picker_selection(dialog, section_name, field_width);
        }
        KeyCode::Backspace => handle_at_picker_backspace(dialog, section_name, field_width),
        KeyCode::Char(c) if modifiers.is_empty() || modifiers == KeyModifiers::SHIFT => {
            push_at_picker_query(dialog, c, modifiers);
        }
        _ => {}
    }

    true
}

fn move_at_picker_up(dialog: &mut SimplePromptDialog) {
    let Some(picker) = dialog.at_picker.as_mut() else {
        return;
    };

    if picker.selected > 0 {
        picker.selected -= 1;
    } else {
        picker.selected = picker.entries.len().saturating_sub(1);
    }
}

fn move_at_picker_down(dialog: &mut SimplePromptDialog) {
    let Some(picker) = dialog.at_picker.as_mut() else {
        return;
    };

    if picker.selected + 1 < picker.entries.len() {
        picker.selected += 1;
    } else {
        picker.selected = 0;
    }
}

fn go_up_at_picker_dir(dialog: &mut SimplePromptDialog) {
    let Some(picker) = dialog.at_picker.as_mut() else {
        return;
    };

    picker.go_up();
}

fn enter_selected_at_picker_dir(dialog: &mut SimplePromptDialog) {
    let Some(picker) = dialog.at_picker.as_mut() else {
        return;
    };

    let is_dir = picker
        .entries
        .get(picker.selected)
        .map(|entry| entry.is_dir)
        .unwrap_or(false);
    if is_dir {
        picker.enter_dir();
    }
}

fn apply_at_picker_selection(
    dialog: &mut SimplePromptDialog,
    section_name: &str,
    field_width: usize,
) {
    let Some((rel_path, full_path)) = selected_at_picker_paths(dialog) else {
        dialog.at_picker = None;
        return;
    };

    let original_focus = dialog.focused_section;
    dialog.insert_at_completion(section_name, &rel_path, &full_path, field_width);
    dialog.focused_section = original_focus;
    dialog.at_picker = None;
}

fn selected_at_picker_paths(dialog: &SimplePromptDialog) -> Option<(String, String)> {
    let picker = dialog.at_picker.as_ref()?;
    let rel_path = picker.relative_path_of_selected()?;
    let full_path = picker.full_path_of_selected()?;
    Some((rel_path, full_path.to_string_lossy().to_string()))
}

fn handle_at_picker_backspace(
    dialog: &mut SimplePromptDialog,
    section_name: &str,
    field_width: usize,
) {
    let query_is_empty = dialog
        .at_picker
        .as_ref()
        .map(|picker| picker.query.is_empty())
        .unwrap_or(true);

    if query_is_empty {
        dialog.at_picker = None;
        dialog.backspace_at_cursor(section_name, field_width);
        return;
    }

    let Some(picker) = dialog.at_picker.as_mut() else {
        return;
    };

    picker.query.pop();
    picker.queue_search();
}

fn push_at_picker_query(dialog: &mut SimplePromptDialog, c: char, modifiers: KeyModifiers) {
    let Some(picker) = dialog.at_picker.as_mut() else {
        return;
    };

    let ch = if modifiers.contains(KeyModifiers::SHIFT) {
        c.to_uppercase().next().unwrap_or(c)
    } else {
        c
    };
    picker.query.push(ch);
    picker.queue_search();
}

/// The send control (focus index 0) key handling, shared by both tabs. When
/// the inline date-time picker is open, arrows edit and Enter confirms;
/// otherwise the selector toggles now/date and navigates back into the fields.
fn handle_send_control_key(
    dialog: &mut SimplePromptDialog,
    code: KeyCode,
    modifiers: KeyModifiers,
    is_shift: bool,
) -> PromptAction {
    // Inline date-time picker open: arrows edit, Enter confirms, Esc abandons.
    if dialog.send_edit.is_some() {
        return match code {
            KeyCode::Esc => {
                dialog.send_edit_cancel();
                PromptAction::None
            }
            KeyCode::Up => {
                dialog.send_edit_adjust(1);
                PromptAction::None
            }
            KeyCode::Down => {
                dialog.send_edit_adjust(-1);
                PromptAction::None
            }
            KeyCode::Left | KeyCode::BackTab => {
                dialog.send_edit_move(-1);
                PromptAction::None
            }
            KeyCode::Right | KeyCode::Tab => {
                dialog.send_edit_move(1);
                PromptAction::None
            }
            KeyCode::Enter => {
                dialog.send_edit_confirm();
                PromptAction::None
            }
            KeyCode::Backspace => {
                dialog.clear_send_at();
                PromptAction::None
            }
            // Typed digits write the focused field directly and auto-advance
            // when it fills (U11); arrows keep working.
            KeyCode::Char(c) if c.is_ascii_digit() => {
                if let Some(digit) = c.to_digit(10) {
                    dialog.send_edit_type_digit(digit);
                }
                PromptAction::None
            }
            _ => PromptAction::None,
        };
    }

    // Selector: lateral arrows toggle now ↔ date, Enter opens the picker on
    // date, Shift+↑/↓ and Tab return to the fields above.
    match code {
        KeyCode::Esc => PromptAction::Close,
        KeyCode::Left | KeyCode::Right => {
            dialog.send_toggle();
            PromptAction::None
        }
        KeyCode::Enter => {
            if dialog.send_choice == crate::tui::app::dialog::SendChoice::Date {
                dialog.send_begin_edit();
            }
            PromptAction::None
        }
        KeyCode::Backspace => {
            dialog.clear_send_at();
            PromptAction::None
        }
        KeyCode::Char('k') if modifiers.contains(KeyModifiers::CONTROL) => {
            PromptAction::CancelNextScheduled
        }
        KeyCode::Up if is_shift => {
            dialog.focus_prev();
            PromptAction::None
        }
        KeyCode::Down if is_shift => {
            dialog.focus_next();
            PromptAction::None
        }
        KeyCode::Tab => {
            dialog.focus_next();
            PromptAction::None
        }
        KeyCode::BackTab => {
            // wrap to last section
            dialog.focused_section = dialog.total_focusable() - 1;
            PromptAction::None
        }
        _ => PromptAction::None,
    }
}

/// Key handling for the Raw tab. Two focus targets only: the raw buffer
/// (focus index ≥ 1) and the send control (focus index 0). No sections,
/// pickers, or @-resources — just a plain multi-line text field plus the
/// shared send control. Sending resolves through `resolve_outgoing_prompt`,
/// so a non-empty buffer is sent verbatim and an empty one sends the composed
/// Normal-form prompt.
#[allow(clippy::too_many_arguments)]
fn handle_raw_tab_key(
    dialog: &mut SimplePromptDialog,
    code: KeyCode,
    modifiers: KeyModifiers,
    field_width: usize,
    db: &Database,
    workdir: &Path,
    keyboard_enhancement: bool,
) -> PromptAction {
    let is_shift = modifiers.contains(KeyModifiers::SHIFT);

    // Send control focus. After delegating, clamp focus back into {0, 1}
    // because the raw tab only has those two stops (Normal-tab navigation may
    // have parked the shared handler on a higher section index).
    if dialog.focused_section == 0 {
        let action = handle_send_control_key(dialog, code, modifiers, is_shift);
        if dialog.focused_section > 1 {
            dialog.focused_section = 1;
        }
        return action;
    }

    let raw = crate::tui::app::dialog::RAW_SECTION_ID;
    match code {
        KeyCode::Esc => PromptAction::Close,

        // Ctrl+S always sends; Shift+Enter sends when the terminal can report it.
        KeyCode::Char('s') if modifiers.contains(KeyModifiers::CONTROL) => {
            build_prompt_action(dialog, db, workdir)
        }
        KeyCode::Enter if should_send_on_shift_enter(keyboard_enhancement, is_shift) => {
            build_prompt_action(dialog, db, workdir)
        }
        // Plain Enter inserts a newline — the raw field is multi-line.
        KeyCode::Enter if modifiers.is_empty() => {
            dialog.insert_newline_at_cursor(raw, field_width);
            PromptAction::None
        }

        // Navigation: move to the send control (the only other stop).
        KeyCode::Tab | KeyCode::BackTab => {
            dialog.focused_section = 0;
            PromptAction::None
        }
        KeyCode::Up if is_shift => {
            dialog.focused_section = 0;
            PromptAction::None
        }
        KeyCode::Down if is_shift => {
            dialog.focused_section = 0;
            PromptAction::None
        }

        // Cursor movement inside the raw buffer.
        KeyCode::Left => {
            dialog.move_cursor_left(raw, field_width);
            PromptAction::None
        }
        KeyCode::Right => {
            dialog.move_cursor_right(raw, field_width);
            PromptAction::None
        }
        KeyCode::Up => {
            dialog.move_cursor_up(raw, field_width);
            PromptAction::None
        }
        KeyCode::Down => {
            dialog.move_cursor_down(raw, field_width);
            PromptAction::None
        }

        // Text input.
        KeyCode::Char(c) => {
            if let Some(ch) = normalize_prompt_char_input(c, modifiers) {
                dialog.insert_char_at_cursor(raw, ch, field_width);
            }
            PromptAction::None
        }
        KeyCode::Backspace => {
            dialog.backspace_at_cursor(raw, field_width);
            PromptAction::None
        }
        _ => PromptAction::None,
    }
}

#[allow(clippy::too_many_arguments)]
fn handle_dialog_key(
    dialog: &mut SimplePromptDialog,
    code: KeyCode,
    modifiers: KeyModifiers,
    section_name: Option<&str>,
    field_width: usize,
    db: &Database,
    workdir: &Path,
    keyboard_enhancement: bool,
) -> Result<PromptAction> {
    // Ctrl+L recalls the project's last prompt regardless of which field is
    // currently focused.
    if code == KeyCode::Char('l') && modifiers.contains(KeyModifiers::CONTROL) {
        return Ok(PromptAction::RecallLastPrompt);
    }

    let is_shift = modifiers.contains(KeyModifiers::SHIFT);
    let is_send_at = dialog.focused_section == 0 && !dialog.enabled_sections.is_empty();

    // ── send control (focus index 0, U11) ───────────────────────────────
    if is_send_at {
        return Ok(handle_send_control_key(dialog, code, modifiers, is_shift));
    }

    // ── Section field keys ──────────────────────────────────────────────
    let section_name = section_name.unwrap_or("");

    match code {
        KeyCode::Esc => Ok(PromptAction::Close),

        // Ctrl+S always works as send (legacy fallback).
        KeyCode::Char('s') if modifiers.contains(KeyModifiers::CONTROL) => {
            Ok(build_prompt_action(dialog, db, workdir))
        }

        // Shift+Enter sends when keyboard enhancement is active.
        KeyCode::Enter if should_send_on_shift_enter(keyboard_enhancement, is_shift) => {
            Ok(build_prompt_action(dialog, db, workdir))
        }

        // Plain Enter: newline in instruction sections, send in others.
        KeyCode::Enter => {
            handle_enter_key(dialog, modifiers, section_name, field_width, db, workdir)
        }

        // Navigation: Tab / Shift+Tab / Shift+Up/Down move between fields in
        // visual order, wrapping through the send control at the bottom.
        KeyCode::Tab => {
            dialog.focus_next();
            Ok(PromptAction::None)
        }
        KeyCode::BackTab => {
            dialog.focus_prev();
            Ok(PromptAction::None)
        }
        KeyCode::Up if is_shift => {
            dialog.focus_prev();
            Ok(PromptAction::None)
        }
        KeyCode::Down if is_shift => {
            dialog.focus_next();
            Ok(PromptAction::None)
        }

        // Cursor movement within the section.
        KeyCode::Left => {
            if let Some(name) = dialog.focused_section_name().map(str::to_string) {
                dialog.move_cursor_left(&name, field_width);
            }
            Ok(PromptAction::None)
        }
        KeyCode::Right => {
            if let Some(name) = dialog.focused_section_name().map(str::to_string) {
                dialog.move_cursor_right(&name, field_width);
            }
            Ok(PromptAction::None)
        }
        KeyCode::Up => {
            if let Some(name) = dialog.focused_section_name().map(str::to_string) {
                dialog.move_cursor_up(&name, field_width);
            }
            Ok(PromptAction::None)
        }
        KeyCode::Down => {
            if let Some(name) = dialog.focused_section_name().map(str::to_string) {
                dialog.move_cursor_down(&name, field_width);
            }
            Ok(PromptAction::None)
        }

        // Section management.
        KeyCode::Char('a') if modifiers.contains(KeyModifiers::CONTROL) => {
            open_add_section_picker_if_available(dialog);
            Ok(PromptAction::None)
        }
        KeyCode::Char('x') if modifiers.contains(KeyModifiers::CONTROL) => {
            open_remove_section_picker_if_available(dialog);
            Ok(PromptAction::None)
        }

        // Text input.
        KeyCode::Char(c) => {
            if let Some(name) = dialog.focused_section_name().map(str::to_string) {
                if dialog.is_locked(&name) {
                    return Ok(PromptAction::None);
                }
                if let Some(ch) = normalize_prompt_char_input(c, modifiers) {
                    handle_section_char_input(dialog, &name, ch, field_width, workdir);
                }
            }
            Ok(PromptAction::None)
        }
        KeyCode::Backspace => {
            if let Some(name) = dialog.focused_section_name().map(str::to_string) {
                if dialog.is_locked(&name) {
                    return Ok(PromptAction::None);
                }
                handle_section_backspace(dialog, &name, field_width);
            }
            Ok(PromptAction::None)
        }
        _ => Ok(PromptAction::None),
    }
}

/// Whether Enter should act as "send" rather than a plain keypress.
/// Most terminals cannot distinguish Shift+Enter from plain Enter without
/// the Kitty keyboard enhancement protocol, so this only fires once that
/// protocol is confirmed active (see `run_tui`'s startup capability probe);
/// otherwise Ctrl+S remains the fallback send key regardless of Shift state.
fn should_send_on_shift_enter(keyboard_enhancement: bool, is_shift: bool) -> bool {
    keyboard_enhancement && is_shift
}

fn normalize_prompt_char_input(c: char, modifiers: KeyModifiers) -> Option<char> {
    if modifiers.is_empty() || modifiers == KeyModifiers::SHIFT {
        return Some(c);
    }

    // AltGr is typically reported as Ctrl+Alt; on some layouts crossterm surfaces
    // the base key (for example 'q'/'2') instead of the produced character.
    if modifiers.contains(KeyModifiers::CONTROL) && modifiers.contains(KeyModifiers::ALT) {
        return Some(normalize_altgr_char(c));
    }

    None
}

fn normalize_altgr_char(c: char) -> char {
    match c {
        'q' | 'Q' | '2' => '@',
        _ => c,
    }
}

fn handle_enter_key(
    dialog: &mut SimplePromptDialog,
    modifiers: KeyModifiers,
    section_name: &str,
    field_width: usize,
    db: &Database,
    workdir: &Path,
) -> Result<PromptAction> {
    if !modifiers.is_empty() {
        return Ok(PromptAction::None);
    }

    if is_instruction_section(section_name) {
        dialog.insert_newline_at_cursor(section_name, field_width);
        return Ok(PromptAction::None);
    }

    Ok(build_prompt_action(dialog, db, workdir))
}

fn is_instruction_section(section_name: &str) -> bool {
    section_name == "instruction" || section_name.starts_with("instruction_")
}

fn build_prompt_action(dialog: &SimplePromptDialog, db: &Database, workdir: &Path) -> PromptAction {
    // Raw tab with a non-empty buffer sends the text verbatim; otherwise the
    // composed Normal-form prompt (see `resolve_outgoing_prompt`).
    let Ok(prompt) = dialog.resolve_outgoing_prompt(db, workdir) else {
        return PromptAction::None;
    };

    match (dialog.send_choice, dialog.send_at) {
        (crate::tui::app::dialog::SendChoice::Date, Some(when)) => {
            PromptAction::ScheduleSend(prompt, when)
        }
        _ => PromptAction::Send(prompt),
    }
}

fn open_add_section_picker_if_available(dialog: &mut SimplePromptDialog) {
    if dialog.get_addable_sections().is_empty() {
        return;
    }

    dialog.picker_mode = SectionPickerMode::AddSection { selected: 0 };
}

fn open_remove_section_picker_if_available(dialog: &mut SimplePromptDialog) {
    if dialog.get_removable_sections().is_empty() {
        return;
    }

    dialog.picker_mode = SectionPickerMode::RemoveSection { selected: 0 };
}

fn handle_section_char_input(
    dialog: &mut SimplePromptDialog,
    section_name: &str,
    c: char,
    field_width: usize,
    workdir: &Path,
) {
    if SimplePromptDialog::is_tools_section(section_name) {
        return;
    }

    dialog.insert_char_at_cursor(section_name, c, field_width);
    if c != '@' || dialog.at_picker.is_some() {
        return;
    }

    let trigger_pos = dialog.cursor(section_name).saturating_sub(1);
    dialog.at_picker = Some(AtPicker::new(workdir.to_path_buf(), trigger_pos));
}

fn handle_section_backspace(
    dialog: &mut SimplePromptDialog,
    section_name: &str,
    field_width: usize,
) {
    if SimplePromptDialog::is_tools_section(section_name) {
        return;
    }

    // Check if cursor is inside a collapsed paste block
    if dialog.cursor_in_collapsed_placeholder(section_name) {
        dialog.backspace_collapsed_paste(section_name, field_width);
    } else {
        dialog.backspace_at_cursor(section_name, field_width);
    }
}

fn submit_prompt(app: &mut App, prompt: &str) {
    // Capture whether system content was included before discarding
    let had_system = app
        .simple_prompt_dialog
        .as_ref()
        .and_then(|d| d.system_content.as_ref())
        .is_some();
    let is_solo = !app.sync_available();
    let workdir = app.current_workdir();
    let session_key = app.current_prompt_session_key();

    persist_last_prompt(app, &workdir, prompt);
    write_prompt_to_selected_agent(app, prompt);
    app.prompt_builder_sessions.remove(&session_key);
    app.discard_simple_prompt_dialog();

    // Record that system block was sent for this workdir
    if had_system {
        let state = app.workdir_system_state.entry(workdir).or_default();
        state.sent = true;
        state.sent_as_solo = is_solo;
    }
}

/// Persist `prompt` (and, if the builder is still open, its structured
/// field state) as the last prompt sent for `workdir`. Best-effort: a
/// failure to persist must never block the send itself.
fn persist_last_prompt(app: &mut App, workdir: &Path, prompt: &str) {
    let builder_state = app
        .simple_prompt_dialog
        .as_ref()
        .map(crate::tui::app::dialog::PersistedBuilderState::from_dialog)
        .and_then(|state| serde_json::to_string(&state).ok());

    let id = format!("lp-{}", uuid::Uuid::new_v4());
    let workdir_str = workdir.to_string_lossy().to_string();
    if let Err(e) = app.db.insert_last_prompt(
        &id,
        &workdir_str,
        prompt,
        builder_state.as_deref(),
        chrono::Utc::now(),
    ) {
        tracing::warn!("Failed to persist last prompt for '{workdir_str}': {e}");
    }
}

/// Load the current project's last prompt into the builder (Ctrl+L). Shows a
/// one-line hint if nothing has been sent from this project yet; otherwise
/// applies immediately for an empty builder, or stages a confirm for a
/// non-empty one so the user doesn't lose in-progress work.
fn recall_last_prompt(app: &mut App) {
    let workdir = app.current_workdir().to_string_lossy().to_string();
    let last = match app.db.get_last_prompt_for_workdir(&workdir) {
        Ok(Some(last)) => last,
        Ok(None) => {
            crate::domain::notification::send_notification(
                "No prompt to recall",
                "Nothing has been sent from this project yet.",
                crate::domain::notification::NotificationLevel::Info,
            );
            return;
        }
        Err(e) => {
            tracing::warn!("Failed to load last prompt for '{workdir}': {e}");
            return;
        }
    };

    let Some(dialog) = app.simple_prompt_dialog.as_mut() else {
        return;
    };

    if dialog.is_empty() {
        apply_last_prompt(dialog, &last);
    } else {
        dialog.pending_recall = Some(last);
    }
}

/// Apply a recalled prompt to `dialog`: a faithful structured restore when
/// the send captured the builder's field state, otherwise the flattened
/// prompt text in a single instruction section (see
/// `SimplePromptDialog::load_flat_text`).
fn apply_last_prompt(dialog: &mut SimplePromptDialog, last: &crate::db::last_prompts::LastPrompt) {
    let restored = last.builder_state.as_deref().and_then(|json| {
        serde_json::from_str::<crate::tui::app::dialog::PersistedBuilderState>(json).ok()
    });

    match restored {
        Some(state) => state.restore_into(dialog),
        None => dialog.load_flat_text(&last.prompt_text),
    }
}

/// Resolve the currently selected interactive agent's session id and workdir,
/// if any — the target for a scheduled send or its cancellation.
fn selected_session_target(app: &App) -> Option<(String, String)> {
    let idx = selected_interactive_index(app)?;
    let agent = app.interactive_agents.get(idx)?;
    Some((agent.id.clone(), agent.working_dir.clone()))
}

fn schedule_send_prompt(app: &mut App, prompt: &str, when: chrono::NaiveDateTime) {
    let session_key = app.current_prompt_session_key();

    // Resolve the target session ID/workdir from the currently selected agent.
    let (target_session_id, target_workdir) = selected_session_target(app).unwrap_or_default();

    // `when` is a full local date-time picked in the builder (U11) and was
    // validated as future by the picker; resolve DST ambiguity leniently.
    let fire_time = when
        .and_local_timezone(chrono::Local)
        .earliest()
        .unwrap_or_else(chrono::Local::now)
        .with_timezone(&chrono::Utc);

    // Persist the scheduled send.
    let id = format!("ss-{}", uuid::Uuid::new_v4());
    let workdir_opt = (!target_workdir.is_empty()).then_some(target_workdir.as_str());
    if let Err(e) =
        app.db
            .insert_scheduled_send(&id, prompt, &target_session_id, workdir_opt, fire_time)
    {
        tracing::error!("Failed to persist scheduled send: {e}");
        crate::domain::notification::send_notification(
            "Schedule failed",
            &format!("Could not save scheduled send: {e}"),
            crate::domain::notification::NotificationLevel::Error,
        );
        return;
    }

    // Persist as the project's last prompt before discarding — a scheduled
    // send is still a "sent" prompt from the builder's perspective, and this
    // also covers the U7 dead-target path (see `deliver_due_scheduled_sends`),
    // which recovers only the flattened text once the builder is long gone.
    let workdir = app.current_workdir();
    persist_last_prompt(app, &workdir, prompt);

    // Discard the dialog (don't persist builder session — prompt is scheduled).
    app.prompt_builder_sessions.remove(&session_key);
    app.discard_simple_prompt_dialog();

    // Show confirmation notification.
    let fire_local = fire_time.with_timezone(&chrono::Local);
    crate::domain::notification::send_notification(
        "Prompt scheduled",
        &format!("Will be delivered at {}", fire_local.format("%H:%M")),
        crate::domain::notification::NotificationLevel::Info,
    );
}

/// Cancel the soonest pending scheduled send targeting the currently
/// selected interactive session. No-op if there is none.
fn cancel_next_scheduled_send(app: &mut App) {
    let Some((target_session_id, _)) = selected_session_target(app) else {
        return;
    };

    let pending = match app
        .db
        .list_pending_scheduled_sends_for_session(&target_session_id)
    {
        Ok(pending) => pending,
        Err(e) => {
            tracing::warn!("Failed to list pending scheduled sends: {e}");
            return;
        }
    };

    let Some(next) = pending.first() else {
        return;
    };

    match app.db.delete_scheduled_send(&next.id) {
        Ok(true) => {
            crate::domain::notification::send_notification(
                "Scheduled send canceled",
                "The scheduled prompt was canceled.",
                crate::domain::notification::NotificationLevel::Info,
            );
        }
        Ok(false) => {}
        Err(e) => tracing::warn!("Failed to cancel scheduled send '{}': {e}", next.id),
    }
}

fn write_prompt_to_selected_agent(app: &mut App, prompt: &str) {
    let Some(idx) = selected_interactive_index(app) else {
        return;
    };
    let Some(agent) = app.interactive_agents.get_mut(idx) else {
        return;
    };

    // Register the prompt in the session history so context transfer can
    // pair it with the agent's response — builder prompts are the main
    // conversation driver, not just direct typing.
    agent.record_prompt(prompt);

    // Delivery must mean submission: the paste and the submit keystroke are
    // written as separate events (see `write_submitted_prompt`), with any
    // per-platform override (delay/key/extra presses) coming from the CLI
    // registry rather than being hardcoded here.
    let spec = agent.cli.paste_submit_spec();
    let _ = agent.submit_prompt_to_pty(prompt, spec);
}

fn selected_interactive_index(app: &App) -> Option<usize> {
    match app.selected_agent() {
        Some(AgentEntry::Interactive(idx)) => Some(*idx),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::normalize_prompt_char_input;
    use ratatui::crossterm::event::KeyModifiers;

    #[test]
    fn normalize_prompt_char_input_accepts_plain_and_shift_chars() {
        assert_eq!(
            normalize_prompt_char_input('@', KeyModifiers::NONE),
            Some('@')
        );
        assert_eq!(
            normalize_prompt_char_input('A', KeyModifiers::SHIFT),
            Some('A')
        );
    }

    #[test]
    fn normalize_prompt_char_input_maps_common_altgr_at_variants() {
        let altgr = KeyModifiers::CONTROL | KeyModifiers::ALT;
        assert_eq!(normalize_prompt_char_input('q', altgr), Some('@'));
        assert_eq!(normalize_prompt_char_input('2', altgr), Some('@'));
        assert_eq!(normalize_prompt_char_input('@', altgr), Some('@'));
    }

    #[test]
    fn normalize_prompt_char_input_rejects_control_shortcuts() {
        assert_eq!(
            normalize_prompt_char_input('a', KeyModifiers::CONTROL),
            None
        );
    }
}

#[cfg(test)]
mod recall_last_prompt_tests {
    use super::handle_prompt_template_key;
    use crate::db::Database;
    use crate::tui::app::types::App;
    use ratatui::crossterm::event::{KeyCode, KeyModifiers};
    use std::sync::Arc;
    use tempfile::{tempdir, NamedTempFile};

    fn test_app() -> (App, tempfile::TempDir) {
        let tmp = NamedTempFile::new().expect("create temp file");
        let path = tmp.path().to_path_buf();
        std::mem::forget(tmp);
        let db = Arc::new(Database::new(&path).expect("create test db"));
        let data_dir = tempdir().expect("create data dir");
        let app = App::new(db, data_dir.path()).expect("create app");
        (app, data_dir)
    }

    fn press(app: &mut App, code: KeyCode) {
        handle_prompt_template_key(app, code, KeyModifiers::NONE).expect("key handled");
    }

    fn ctrl_l(app: &mut App) {
        handle_prompt_template_key(app, KeyCode::Char('l'), KeyModifiers::CONTROL)
            .expect("ctrl+l handled");
    }

    fn ctrl_e(app: &mut App) {
        handle_prompt_template_key(app, KeyCode::Char('e'), KeyModifiers::CONTROL)
            .expect("ctrl+e handled");
    }

    #[test]
    fn ctrl_e_toggles_between_normal_and_raw_tabs() {
        use crate::tui::app::dialog::PromptTab;
        let (mut app, _dir) = test_app();
        app.open_simple_prompt_dialog(None);
        assert_eq!(
            app.simple_prompt_dialog.as_ref().unwrap().active_tab,
            PromptTab::Normal
        );

        ctrl_e(&mut app);
        assert_eq!(
            app.simple_prompt_dialog.as_ref().unwrap().active_tab,
            PromptTab::Raw
        );

        ctrl_e(&mut app);
        assert_eq!(
            app.simple_prompt_dialog.as_ref().unwrap().active_tab,
            PromptTab::Normal
        );
    }

    #[test]
    fn typing_in_the_raw_tab_fills_the_raw_buffer_only() {
        use crate::tui::app::dialog::PromptTab;
        let (mut app, _dir) = test_app();
        app.open_simple_prompt_dialog(None);
        ctrl_e(&mut app); // → Raw

        for ch in "/compact".chars() {
            press(&mut app, KeyCode::Char(ch));
        }

        let dialog = app.simple_prompt_dialog.as_ref().unwrap();
        assert_eq!(dialog.active_tab, PromptTab::Raw);
        assert_eq!(dialog.raw_text(), "/compact");
        // The Normal-form instruction section is untouched by raw typing.
        assert_eq!(dialog.get_section_content("instruction_1"), "");
    }

    #[test]
    fn ctrl_l_with_nothing_stored_leaves_empty_builder_untouched() {
        let (mut app, _dir) = test_app();
        app.open_simple_prompt_dialog(None);

        ctrl_l(&mut app);

        let dialog = app.simple_prompt_dialog.as_ref().unwrap();
        assert!(dialog.is_empty());
        assert!(dialog.pending_recall.is_none());
    }

    #[test]
    fn fresh_open_focuses_the_first_section() {
        let (mut app, _dir) = test_app();
        app.open_simple_prompt_dialog(None);
        let dialog = app.simple_prompt_dialog.as_ref().unwrap();
        assert_eq!(dialog.focused_section, 1);
        assert!(dialog.focused_section_name().is_some());
    }

    #[test]
    fn open_with_initial_content_focuses_first_section_not_send_control() {
        let (mut app, _dir) = test_app();
        let mut content = std::collections::HashMap::new();
        content.insert("instruction".to_string(), "prefilled task".to_string());
        app.open_simple_prompt_dialog(Some(content));

        let dialog = app.simple_prompt_dialog.as_ref().unwrap();
        // Previously this path parked focus on the send control (index 0).
        assert_eq!(dialog.focused_section, 1);
        assert!(dialog.focused_section_name().is_some());
    }

    #[test]
    fn ctrl_l_recalls_immediately_into_an_empty_builder() {
        let (mut app, _dir) = test_app();
        let workdir = app.current_workdir().to_string_lossy().to_string();
        app.db
            .insert_last_prompt(
                "lp-1",
                &workdir,
                "recovered prompt",
                None,
                chrono::Utc::now(),
            )
            .expect("seed last prompt");

        app.open_simple_prompt_dialog(None);
        assert!(app.simple_prompt_dialog.as_ref().unwrap().is_empty());

        ctrl_l(&mut app);

        let dialog = app.simple_prompt_dialog.as_ref().unwrap();
        assert!(dialog.pending_recall.is_none());
        assert_eq!(
            dialog.get_section_content("instruction_1"),
            "recovered prompt"
        );
    }

    #[test]
    fn ctrl_l_on_a_non_empty_builder_stages_a_confirm_instead_of_overwriting() {
        let (mut app, _dir) = test_app();
        let workdir = app.current_workdir().to_string_lossy().to_string();
        app.db
            .insert_last_prompt(
                "lp-1",
                &workdir,
                "recovered prompt",
                None,
                chrono::Utc::now(),
            )
            .expect("seed last prompt");

        app.open_simple_prompt_dialog(None);
        app.simple_prompt_dialog
            .as_mut()
            .unwrap()
            .set_section_content("instruction_1", "work in progress".to_string());

        ctrl_l(&mut app);

        // Content must survive until the confirm is answered.
        let dialog = app.simple_prompt_dialog.as_ref().unwrap();
        assert!(dialog.pending_recall.is_some());
        assert_eq!(
            dialog.get_section_content("instruction_1"),
            "work in progress"
        );

        // 'n' cancels the recall and leaves the in-progress draft intact.
        press(&mut app, KeyCode::Char('n'));
        let dialog = app.simple_prompt_dialog.as_ref().unwrap();
        assert!(dialog.pending_recall.is_none());
        assert_eq!(
            dialog.get_section_content("instruction_1"),
            "work in progress"
        );

        // Recall again and this time confirm with 'y'.
        ctrl_l(&mut app);
        press(&mut app, KeyCode::Char('y'));
        let dialog = app.simple_prompt_dialog.as_ref().unwrap();
        assert!(dialog.pending_recall.is_none());
        assert_eq!(
            dialog.get_section_content("instruction_1"),
            "recovered prompt"
        );
    }
}

#[cfg(test)]
mod keyboard_capability_tests {
    use super::should_send_on_shift_enter;

    #[test]
    fn shift_enter_sends_when_enhancement_active() {
        assert!(should_send_on_shift_enter(true, true));
    }

    #[test]
    fn plain_enter_never_sends_even_with_enhancement_active() {
        assert!(!should_send_on_shift_enter(true, false));
    }

    #[test]
    fn shift_enter_does_not_send_without_enhancement() {
        // Terminal can't disambiguate Shift+Enter from Enter here — Ctrl+S
        // (handled unconditionally elsewhere) is the fallback send key.
        assert!(!should_send_on_shift_enter(false, true));
    }

    #[test]
    fn plain_enter_without_enhancement_does_not_send() {
        assert!(!should_send_on_shift_enter(false, false));
    }
}

#[cfg(test)]
mod send_at_tests {
    use crate::tui::app::dialog::{SendChoice, SimplePromptDialog};

    #[test]
    fn send_control_starts_on_now() {
        let dialog = SimplePromptDialog::new();
        assert_eq!(dialog.send_choice, SendChoice::Now);
        assert!(dialog.send_at.is_none());
        assert!(dialog.send_edit.is_none());
        assert_eq!(dialog.send_display(), "now");
    }

    #[test]
    fn lateral_toggle_switches_now_and_date() {
        let mut dialog = SimplePromptDialog::new();
        dialog.send_toggle();
        assert_eq!(dialog.send_choice, SendChoice::Date);
        assert_eq!(dialog.send_display(), "date");
        // Toggling back to now discards any picked time.
        dialog.send_at = Some(chrono::Local::now().naive_local() + chrono::Duration::hours(2));
        dialog.send_toggle();
        assert_eq!(dialog.send_choice, SendChoice::Now);
        assert!(dialog.send_at.is_none());
        assert_eq!(dialog.send_display(), "now");
    }

    #[test]
    fn begin_edit_preseeds_current_local_time() {
        let mut dialog = SimplePromptDialog::new();
        dialog.send_toggle(); // → date
        dialog.send_begin_edit();
        let edit = dialog.send_edit.expect("picker open");
        assert_eq!(edit.field, 0);
        let now = chrono::Local::now().naive_local();
        let delta = (now - edit.value).num_seconds().abs();
        assert!(delta < 120, "picker must preseed the current time");
    }

    #[test]
    fn edit_adjust_uses_calendar_math() {
        let mut dialog = SimplePromptDialog::new();
        dialog.send_toggle();
        dialog.send_begin_edit();
        let base = chrono::NaiveDate::from_ymd_opt(2026, 1, 31)
            .unwrap()
            .and_hms_opt(10, 0, 0)
            .unwrap();
        dialog.send_edit.as_mut().unwrap().value = base;
        // +1 month from Jan 31 clamps to Feb 28 instead of panicking.
        dialog.send_edit.as_mut().unwrap().field = 1;
        dialog.send_edit_adjust(1);
        let v = dialog.send_edit.unwrap().value;
        assert_eq!(v.date().to_string(), "2026-02-28");
        // Minute adjustment carries across the hour.
        dialog.send_edit.as_mut().unwrap().field = 4;
        dialog.send_edit.as_mut().unwrap().value = base;
        dialog.send_edit_adjust(-1);
        let v = dialog.send_edit.unwrap().value;
        assert_eq!(v.format("%H:%M").to_string(), "09:59");
    }

    #[test]
    fn confirm_rejects_past_times_with_inline_hint() {
        let mut dialog = SimplePromptDialog::new();
        dialog.send_toggle();
        dialog.send_begin_edit();
        dialog.send_edit.as_mut().unwrap().value =
            chrono::Local::now().naive_local() - chrono::Duration::hours(1);
        assert!(!dialog.send_edit_confirm());
        assert!(dialog.send_error.is_some());
        assert!(dialog.send_at.is_none());
        assert!(dialog.send_edit.is_some(), "picker stays open to fix it");
    }

    #[test]
    fn confirm_accepts_future_time_and_displays_it() {
        let mut dialog = SimplePromptDialog::new();
        dialog.send_toggle();
        dialog.send_begin_edit();
        let future = chrono::Local::now().naive_local() + chrono::Duration::hours(3);
        dialog.send_edit.as_mut().unwrap().value = future;
        assert!(dialog.send_edit_confirm());
        assert_eq!(dialog.send_at, Some(future));
        assert!(dialog.send_edit.is_none());
        assert!(dialog.send_error.is_none());
        assert_eq!(
            dialog.send_display(),
            future.format("%Y-%m-%d %H:%M").to_string()
        );
    }

    #[test]
    fn cancel_without_confirmed_time_returns_to_now() {
        let mut dialog = SimplePromptDialog::new();
        dialog.send_toggle();
        dialog.send_begin_edit();
        dialog.send_edit_cancel();
        assert!(dialog.send_edit.is_none());
        assert_eq!(dialog.send_choice, SendChoice::Now);
        // But a previously confirmed time survives a later canceled edit.
        dialog.send_toggle();
        dialog.send_begin_edit();
        let future = chrono::Local::now().naive_local() + chrono::Duration::hours(3);
        dialog.send_edit.as_mut().unwrap().value = future;
        assert!(dialog.send_edit_confirm());
        dialog.send_begin_edit();
        dialog.send_edit_cancel();
        assert_eq!(dialog.send_choice, SendChoice::Date);
        assert_eq!(dialog.send_at, Some(future));
    }

    #[test]
    fn clear_send_at_resets_everything() {
        let mut dialog = SimplePromptDialog::new();
        dialog.send_toggle();
        dialog.send_begin_edit();
        let future = chrono::Local::now().naive_local() + chrono::Duration::hours(3);
        dialog.send_edit.as_mut().unwrap().value = future;
        dialog.send_edit_confirm();
        dialog.clear_send_at();
        assert_eq!(dialog.send_choice, SendChoice::Now);
        assert!(dialog.send_at.is_none());
        assert!(dialog.send_edit.is_none());
        assert_eq!(dialog.send_display(), "now");
    }

    #[test]
    fn focus_starts_on_first_section_and_wraps_through_send_control() {
        let mut dialog = SimplePromptDialog::new();
        // Opens on the first section, not on the send control.
        assert_eq!(dialog.focused_section, 1);
        assert_eq!(dialog.total_focusable(), 2); // send control + instruction_1

        // Down from the last section reaches the send control (visually below).
        dialog.focus_next();
        assert_eq!(dialog.focused_section, 0);
        // Down from the send control wraps to the first section.
        dialog.focus_next();
        assert_eq!(dialog.focused_section, 1);
        // Up from the first section wraps down to the send control.
        dialog.focus_prev();
        assert_eq!(dialog.focused_section, 0);
        // Up from the send control goes to the last section.
        dialog.focus_prev();
        assert_eq!(dialog.focused_section, 1);
    }

    #[test]
    fn focused_section_name_returns_none_for_send_control() {
        let mut dialog = SimplePromptDialog::new();
        dialog.focused_section = 0; // send control
        assert!(dialog.focused_section_name().is_none());

        dialog.focused_section = 1; // instruction_1
        assert_eq!(dialog.focused_section_name(), Some("instruction_1"));
    }

    use chrono::{Datelike, NaiveDate};

    fn picker_at(dialog: &mut SimplePromptDialog, base: NaiveDate) {
        dialog.send_toggle(); // → date
        dialog.send_begin_edit();
        dialog.send_edit.as_mut().unwrap().value = base.and_hms_opt(10, 30, 0).unwrap();
    }

    fn type_digits(dialog: &mut SimplePromptDialog, digits: &str) {
        for c in digits.chars() {
            dialog.send_edit_type_digit(c.to_digit(10).unwrap());
        }
    }

    #[test]
    fn typing_four_digits_sets_year_and_auto_advances_to_month() {
        let mut dialog = SimplePromptDialog::new();
        picker_at(&mut dialog, NaiveDate::from_ymd_opt(2000, 5, 15).unwrap());
        type_digits(&mut dialog, "2026");
        let edit = dialog.send_edit.unwrap();
        assert_eq!(edit.value.year(), 2026);
        // Year is 4 digits wide → focus moves on to the month field.
        assert_eq!(edit.field, 1);
    }

    #[test]
    fn typing_two_digits_sets_month_and_advances() {
        let mut dialog = SimplePromptDialog::new();
        picker_at(&mut dialog, NaiveDate::from_ymd_opt(2026, 12, 15).unwrap());
        // Jump straight to the month field, then type "07".
        dialog.send_edit.as_mut().unwrap().field = 1;
        dialog.send_edit.as_mut().unwrap().typed = 0;
        dialog.send_edit.as_mut().unwrap().typed_len = 0;
        type_digits(&mut dialog, "07");
        let edit = dialog.send_edit.unwrap();
        assert_eq!(edit.value.month(), 7);
        assert_eq!(edit.field, 2); // advanced to day
    }

    #[test]
    fn partial_digit_then_arrow_still_adjusts() {
        let mut dialog = SimplePromptDialog::new();
        picker_at(&mut dialog, NaiveDate::from_ymd_opt(2020, 5, 15).unwrap());
        // One digit of the 4-wide year: 2020 → 2 (field not full yet).
        type_digits(&mut dialog, "2");
        assert_eq!(dialog.send_edit.unwrap().value.year(), 2);
        // Arrow up on the year field adds a year — arrows keep working.
        dialog.send_edit_adjust(1);
        assert_eq!(dialog.send_edit.unwrap().value.year(), 3);
        // A digit after the arrow starts a fresh number (accumulator cleared).
        type_digits(&mut dialog, "9");
        assert_eq!(dialog.send_edit.unwrap().value.year(), 9);
    }

    #[test]
    fn invalid_month_thirteen_clamps_to_twelve() {
        // Documented choice: an out-of-range typed component is CLAMPED into
        // its valid range (month 13 → 12), matching the arrow adjust math.
        let mut dialog = SimplePromptDialog::new();
        picker_at(&mut dialog, NaiveDate::from_ymd_opt(2026, 6, 15).unwrap());
        dialog.send_edit.as_mut().unwrap().field = 1;
        dialog.send_edit.as_mut().unwrap().typed = 0;
        dialog.send_edit.as_mut().unwrap().typed_len = 0;
        type_digits(&mut dialog, "13");
        assert_eq!(dialog.send_edit.unwrap().value.month(), 12);
    }

    #[test]
    fn typing_full_date_time_sequence_fills_every_field() {
        let mut dialog = SimplePromptDialog::new();
        picker_at(&mut dialog, NaiveDate::from_ymd_opt(2000, 1, 1).unwrap());
        // year(4) month(2) day(2) hour(2) minute(2), auto-advancing each time.
        type_digits(&mut dialog, "2026"); // year
        type_digits(&mut dialog, "07"); // month
        type_digits(&mut dialog, "20"); // day
        type_digits(&mut dialog, "14"); // hour
        type_digits(&mut dialog, "35"); // minute
        let v = dialog.send_edit.unwrap().value;
        assert_eq!(
            v,
            NaiveDate::from_ymd_opt(2026, 7, 20)
                .unwrap()
                .and_hms_opt(14, 35, 0)
                .unwrap()
        );
    }
}
