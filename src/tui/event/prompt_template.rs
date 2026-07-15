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
    /// Schedule the prompt for later delivery at the given (hour, minute).
    ScheduleSend(String, u8, u8),
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

    if handle_section_picker_key(dialog, &db, &workdir, code)? {
        return Ok(());
    }

    // Determine the focused section name (None when send_at is focused).
    let section_name = focused_section_name(dialog);

    // Only expand collapsed pastes when entering text or doing edit operations
    // (not for navigation keys like arrows, tab, etc.)
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

    let action = handle_dialog_key(
        dialog,
        code,
        modifiers,
        section_name.as_deref(),
        field_width,
        &db,
        &workdir,
        app.keyboard_enhancement_active,
    )?;

    match action {
        PromptAction::None => {}
        PromptAction::Close => app.close_simple_prompt_dialog(),
        PromptAction::Send(prompt) => submit_prompt(app, &prompt),
        PromptAction::ScheduleSend(prompt, hour, minute) => {
            schedule_send_prompt(app, &prompt, hour, minute);
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
    let last_index = dialog.enabled_sections.len().checked_sub(1)?;

    // Adjust focused_section: section indices start at 1 (send_at is 0).
    let section_idx = dialog.focused_section.saturating_sub(1);
    let section_idx = section_idx.min(last_index);
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

    // ── send_at field editing (focus index 0) ───────────────────────────
    if is_send_at {
        return match code {
            KeyCode::Esc => Ok(PromptAction::Close),
            KeyCode::Up => {
                dialog.send_at_increment();
                Ok(PromptAction::None)
            }
            KeyCode::Down => {
                dialog.send_at_decrement();
                Ok(PromptAction::None)
            }
            KeyCode::Left => {
                dialog.send_at_move_focus(-1);
                Ok(PromptAction::None)
            }
            KeyCode::Right => {
                dialog.send_at_move_focus(1);
                Ok(PromptAction::None)
            }
            KeyCode::Backspace => {
                dialog.clear_send_at();
                Ok(PromptAction::None)
            }
            KeyCode::Char('k') if modifiers.contains(KeyModifiers::CONTROL) => {
                Ok(PromptAction::CancelNextScheduled)
            }
            KeyCode::Tab => {
                dialog.focus_next();
                Ok(PromptAction::None)
            }
            KeyCode::BackTab => {
                // wrap to last section
                dialog.focused_section = dialog.total_focusable() - 1;
                Ok(PromptAction::None)
            }
            _ => Ok(PromptAction::None),
        };
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

        // Navigation: Tab / Shift+Tab / Shift+Up/Down move between fields.
        KeyCode::Tab if dialog.focused_section < dialog.total_focusable() - 1 => {
            dialog.focus_next();
            Ok(PromptAction::None)
        }
        KeyCode::Tab => Ok(PromptAction::None),
        KeyCode::BackTab if dialog.focused_section > 0 => {
            dialog.focus_prev();
            Ok(PromptAction::None)
        }
        KeyCode::Up if is_shift && dialog.focused_section > 0 => {
            dialog.focus_prev();
            Ok(PromptAction::None)
        }
        KeyCode::Down if is_shift && dialog.focused_section < dialog.total_focusable() - 1 => {
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
    let Ok(prompt) = dialog.build_prompt_with_resolved_resources(db, workdir) else {
        return PromptAction::None;
    };

    match dialog.send_at {
        Some((hour, minute)) => PromptAction::ScheduleSend(prompt, hour, minute),
        None => PromptAction::Send(prompt),
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

fn schedule_send_prompt(app: &mut App, prompt: &str, hour: u8, minute: u8) {
    let session_key = app.current_prompt_session_key();

    // Resolve the target session ID/workdir from the currently selected agent.
    let (target_session_id, target_workdir) = selected_session_target(app).unwrap_or_default();

    // Compute fire time: today if the time hasn't passed, tomorrow otherwise.
    let now = chrono::Local::now();
    let fire_time = {
        let today = now.date_naive();
        let fire_naive = today.and_hms_opt(hour as u32, minute as u32, 0).unwrap();
        let fire_local = fire_naive.and_local_timezone(chrono::Local).unwrap();
        if fire_local <= now {
            // Time has passed today → schedule for tomorrow.
            (today + chrono::Duration::days(1))
                .and_hms_opt(hour as u32, minute as u32, 0)
                .unwrap()
                .and_local_timezone(chrono::Local)
                .unwrap()
                .with_timezone(&chrono::Utc)
        } else {
            fire_local.with_timezone(&chrono::Utc)
        }
    };

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
    use crate::tui::app::dialog::SimplePromptDialog;

    #[test]
    fn send_at_starts_unset() {
        let dialog = SimplePromptDialog::new();
        assert!(dialog.send_at.is_none());
        assert_eq!(dialog.send_at_display(), "now (unset)");
    }

    #[test]
    fn send_at_increment_creates_default_time() {
        let mut dialog = SimplePromptDialog::new();
        // Focused on send_at (focus 0), press Up to increment hour.
        dialog.focused_section = 0;
        dialog.send_at_increment();
        assert!(dialog.send_at.is_some());
        let (h, m) = dialog.send_at.unwrap();
        assert!(h < 24);
        assert_eq!(m, 0);
    }

    #[test]
    fn send_at_increment_hour_wraps() {
        let mut dialog = SimplePromptDialog::new();
        dialog.send_at = Some((23, 0));
        dialog.send_at_focus_unit = 0;
        dialog.send_at_increment();
        assert_eq!(dialog.send_at, Some((0, 0)));
    }

    #[test]
    fn send_at_increment_minute_wraps() {
        let mut dialog = SimplePromptDialog::new();
        dialog.send_at = Some((10, 59));
        dialog.send_at_focus_unit = 1;
        dialog.send_at_increment();
        assert_eq!(dialog.send_at, Some((10, 0)));
    }

    #[test]
    fn send_at_decrement_hour_wraps() {
        let mut dialog = SimplePromptDialog::new();
        dialog.send_at = Some((0, 0));
        dialog.send_at_focus_unit = 0;
        dialog.send_at_decrement();
        assert_eq!(dialog.send_at, Some((23, 0)));
    }

    #[test]
    fn send_at_decrement_minute_wraps() {
        let mut dialog = SimplePromptDialog::new();
        dialog.send_at = Some((10, 0));
        dialog.send_at_focus_unit = 1;
        dialog.send_at_decrement();
        assert_eq!(dialog.send_at, Some((10, 59)));
    }

    #[test]
    fn send_at_move_focus_between_hour_and_minute() {
        let mut dialog = SimplePromptDialog::new();
        dialog.send_at = Some((14, 30));
        dialog.send_at_focus_unit = 0; // hour
        dialog.send_at_move_focus(1); // → minute
        assert_eq!(dialog.send_at_focus_unit, 1);
        dialog.send_at_move_focus(1); // already at minute, no change
        assert_eq!(dialog.send_at_focus_unit, 1);
        dialog.send_at_move_focus(-1); // → hour
        assert_eq!(dialog.send_at_focus_unit, 0);
        dialog.send_at_move_focus(-1); // already at hour, no change
        assert_eq!(dialog.send_at_focus_unit, 0);
    }

    #[test]
    fn send_at_clear_resets() {
        let mut dialog = SimplePromptDialog::new();
        dialog.send_at = Some((14, 30));
        dialog.clear_send_at();
        assert!(dialog.send_at.is_none());
        assert_eq!(dialog.send_at_display(), "now (unset)");
    }

    #[test]
    fn send_at_display_formats_correctly() {
        let mut dialog = SimplePromptDialog::new();
        dialog.send_at = Some((9, 5));
        assert_eq!(dialog.send_at_display(), "09:05");
        dialog.send_at = Some((14, 30));
        assert_eq!(dialog.send_at_display(), "14:30");
    }

    #[test]
    fn focus_navigation_includes_send_at() {
        let mut dialog = SimplePromptDialog::new();
        // Start at send_at (focus 0).
        assert_eq!(dialog.focused_section, 0);
        assert_eq!(dialog.total_focusable(), 2); // send_at + instruction_1

        dialog.focus_next();
        assert_eq!(dialog.focused_section, 1); // instruction_1

        dialog.focus_next();
        assert_eq!(dialog.focused_section, 1); // already at end

        dialog.focus_prev();
        assert_eq!(dialog.focused_section, 0); // back to send_at

        dialog.focus_prev();
        assert_eq!(dialog.focused_section, 0); // already at start
    }

    #[test]
    fn focused_section_name_returns_none_for_send_at() {
        let mut dialog = SimplePromptDialog::new();
        dialog.focused_section = 0; // send_at
        assert!(dialog.focused_section_name().is_none());

        dialog.focused_section = 1; // instruction_1
        assert_eq!(dialog.focused_section_name(), Some("instruction_1"));
    }
}
