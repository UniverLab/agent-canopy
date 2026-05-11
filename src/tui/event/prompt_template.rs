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

    if handle_section_picker_key(dialog, &db, &workdir, code)? {
        return Ok(());
    }

    let Some(section_name) = focused_section_name(dialog) else {
        app.close_simple_prompt_dialog();
        return Ok(());
    };

    // Only expand collapsed pastes when entering text or doing edit operations
    // (not for navigation keys like arrows, tab, etc.)
    if should_expand_on_key(code, modifiers) {
        dialog.expand_collapsed_paste(&section_name);
    }

    if handle_at_picker_key(dialog, code, modifiers, &section_name, field_width) {
        return Ok(());
    }

    let action = handle_dialog_key(
        dialog,
        code,
        modifiers,
        &section_name,
        field_width,
        &db,
        &workdir,
    )?;

    match action {
        PromptAction::None => {}
        PromptAction::Close => app.close_simple_prompt_dialog(),
        PromptAction::Send(prompt) => submit_prompt(app, &prompt),
    }

    Ok(())
}

fn prompt_field_width(app: &App) -> usize {
    // Approximate instruction field width from terminal width
    // Must match the render calculation in dialogs.rs:
    //   dialog_width = (term_width * 65/100).max(40)
    //   inner_width  = dialog_width - 2 (borders)
    //   field_width  = inner_width - 2 (padding)
    ((app.term_width as usize * 65 / 100).max(40))
        .saturating_sub(4)
        .max(10)
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

    dialog.focused_section = dialog.focused_section.min(last_index);
    dialog.enabled_sections.get(dialog.focused_section).cloned()
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
        "memory_context" => {
            let memory = SimplePromptDialog::build_memory_context_block(db, workdir);
            let section_id = dialog.add_section_with_content("memory_context", memory);
            dialog.lock_section(&section_id);
            dialog.picker_mode = SectionPickerMode::None;
        }
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
    picker.refresh();
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
    picker.refresh();
}

fn handle_dialog_key(
    dialog: &mut SimplePromptDialog,
    code: KeyCode,
    modifiers: KeyModifiers,
    section_name: &str,
    field_width: usize,
    db: &Database,
    workdir: &Path,
) -> Result<PromptAction> {
    let is_shift = modifiers.contains(KeyModifiers::SHIFT);

    match code {
        KeyCode::Esc => Ok(PromptAction::Close),
        KeyCode::Char('s') if modifiers.contains(KeyModifiers::CONTROL) => {
            Ok(build_prompt_action(dialog, db, workdir))
        }
        KeyCode::Enter => {
            handle_enter_key(dialog, modifiers, section_name, field_width, db, workdir)
        }
        KeyCode::Tab if dialog.focused_section + 1 < dialog.enabled_sections.len() => {
            dialog.focused_section += 1;
            Ok(PromptAction::None)
        }
        KeyCode::Tab => Ok(PromptAction::None),
        KeyCode::BackTab if dialog.focused_section > 0 => {
            dialog.focused_section -= 1;
            Ok(PromptAction::None)
        }
        KeyCode::Up if is_shift && dialog.focused_section > 0 => {
            dialog.focused_section -= 1;
            Ok(PromptAction::None)
        }
        KeyCode::Down if is_shift && dialog.focused_section + 1 < dialog.enabled_sections.len() => {
            dialog.focused_section += 1;
            Ok(PromptAction::None)
        }
        KeyCode::Left => {
            dialog.move_cursor_left(section_name, field_width);
            Ok(PromptAction::None)
        }
        KeyCode::Right => {
            dialog.move_cursor_right(section_name, field_width);
            Ok(PromptAction::None)
        }
        KeyCode::Up => {
            dialog.move_cursor_up(section_name, field_width);
            Ok(PromptAction::None)
        }
        KeyCode::Down => {
            dialog.move_cursor_down(section_name, field_width);
            Ok(PromptAction::None)
        }
        KeyCode::Char('a') if modifiers.contains(KeyModifiers::CONTROL) => {
            open_add_section_picker_if_available(dialog);
            Ok(PromptAction::None)
        }
        KeyCode::Char('x') if modifiers.contains(KeyModifiers::CONTROL) => {
            open_remove_section_picker_if_available(dialog);
            Ok(PromptAction::None)
        }
        KeyCode::Char(c) => {
            if dialog.is_locked(section_name) {
                return Ok(PromptAction::None);
            }
            if let Some(ch) = normalize_prompt_char_input(c, modifiers) {
                handle_section_char_input(dialog, section_name, ch, field_width, workdir);
            }
            Ok(PromptAction::None)
        }
        KeyCode::Backspace => {
            if dialog.is_locked(section_name) {
                return Ok(PromptAction::None);
            }
            handle_section_backspace(dialog, section_name, field_width);
            Ok(PromptAction::None)
        }
        _ => Ok(PromptAction::None),
    }
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

    PromptAction::Send(prompt)
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

    write_prompt_to_selected_agent(app, prompt);
    app.prompt_builder_sessions.remove(&workdir);
    app.discard_simple_prompt_dialog();

    // Record that system block was sent for this workdir
    if had_system {
        let state = app.workdir_system_state.entry(workdir).or_default();
        state.sent = true;
        state.sent_as_solo = is_solo;
    }
}

fn write_prompt_to_selected_agent(app: &mut App, prompt: &str) {
    let Some(idx) = selected_interactive_index(app) else {
        return;
    };
    let Some(agent) = app.interactive_agents.get_mut(idx) else {
        return;
    };

    let pasted = format!("\x1b[200~{prompt}\x1b[201~");
    let _ = agent.write_to_pty(pasted.as_bytes());
    let _ = agent.write_to_pty(b"\r");
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
