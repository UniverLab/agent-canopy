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

    // Scheduled-sends list interactions (B33) need app-level DB access, so they
    // run before the dialog is borrowed mutably. They never fire while the
    // recall confirm overlay is up — that overlay owns every key.
    let recall_open = app
        .simple_prompt_dialog
        .as_ref()
        .is_some_and(|d| d.pending_recall.is_some());
    if !recall_open {
        let list_focused = app
            .simple_prompt_dialog
            .as_ref()
            .is_some_and(|d| d.scheduled_list_selected.is_some());
        if list_focused {
            handle_scheduled_list_key(app, code, modifiers);
            return Ok(());
        }
        // Ctrl+P moves focus INTO the pending-sends list (no-op when empty).
        if code == KeyCode::Char('p') && modifiers.contains(KeyModifiers::CONTROL) {
            enter_scheduled_list(app);
            return Ok(());
        }
    }

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

    // Ctrl+E and Shift+←/→ toggle between the Normal and Raw tabs from any
    // focus — except while the date-time picker is open, where lateral arrows
    // move between its fields. Entering the Raw tab (re)computes the
    // composed-prompt preview for its empty state.
    let shift_tab_switch = matches!(code, KeyCode::Left | KeyCode::Right)
        && modifiers.contains(KeyModifiers::SHIFT)
        && dialog.send_edit.is_none();
    if (code == KeyCode::Char('e') && modifiers.contains(KeyModifiers::CONTROL)) || shift_tab_switch
    {
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
        PromptAction::CancelNextScheduled => cancel_scheduled_send(app),
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
        SectionPickerMode::PresetPicker { .. } => {
            handle_preset_picker_key(dialog, code);
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
        "preset" => open_preset_picker(dialog),
        _ => {
            dialog.add_section(name);
            dialog.picker_mode = SectionPickerMode::None;
        }
    }

    Ok(())
}

/// Open the Preset picker (P2): lists `~/.canopy/prompts/*.md`, read fresh
/// so an external edit shows up immediately. An empty/missing directory
/// still opens the picker — it just shows the "no presets" hint (rendered
/// in `ui::dialogs::section_picker`) instead of erroring.
fn open_preset_picker(dialog: &mut SimplePromptDialog) {
    let dir = crate::domain::prompts::prompts_dir(&crate::domain::prompts::canopy_dir());
    let entries = SimplePromptDialog::collect_presets_for_picker(&dir);
    dialog.picker_mode = SectionPickerMode::PresetPicker {
        selected: 0,
        entries,
        filter: String::new(),
    };
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

/// Indices into the Preset picker's `entries` that currently pass its typed
/// filter, or `Vec::new()` if `dialog.picker_mode` isn't `PresetPicker`.
fn preset_filtered_indices(dialog: &SimplePromptDialog) -> Vec<usize> {
    let SectionPickerMode::PresetPicker {
        entries, filter, ..
    } = &dialog.picker_mode
    else {
        return Vec::new();
    };
    SimplePromptDialog::filtered_preset_indices(entries, filter)
}

fn handle_preset_picker_key(dialog: &mut SimplePromptDialog, code: KeyCode) {
    match code {
        KeyCode::Esc => dialog.picker_mode = SectionPickerMode::None,
        KeyCode::Up => move_preset_picker(dialog, false),
        KeyCode::Down => move_preset_picker(dialog, true),
        KeyCode::Enter | KeyCode::Tab => confirm_preset_picker_selection(dialog),
        KeyCode::Backspace => pop_preset_picker_filter(dialog),
        KeyCode::Char(c) => push_preset_picker_filter(dialog, c),
        _ => {}
    }
}

fn move_preset_picker(dialog: &mut SimplePromptDialog, forward: bool) {
    let filtered_len = preset_filtered_indices(dialog).len();
    if filtered_len == 0 {
        return;
    }
    let SectionPickerMode::PresetPicker { selected, .. } = &mut dialog.picker_mode else {
        return;
    };
    *selected = if forward {
        (*selected + 1) % filtered_len
    } else {
        selected.checked_sub(1).unwrap_or(filtered_len - 1)
    };
}

fn push_preset_picker_filter(dialog: &mut SimplePromptDialog, c: char) {
    if let SectionPickerMode::PresetPicker {
        filter, selected, ..
    } = &mut dialog.picker_mode
    {
        filter.push(c);
        *selected = 0;
    }
}

fn pop_preset_picker_filter(dialog: &mut SimplePromptDialog) {
    if let SectionPickerMode::PresetPicker {
        filter, selected, ..
    } = &mut dialog.picker_mode
    {
        filter.pop();
        *selected = 0;
    }
}

/// Insert the selected preset's full content as a new instruction section
/// (mirrors the Skills picker's un-replace_id path: `add_section_with_content`)
/// so the preset becomes a normal, freely-editable field — never a locked
/// reference — and closes the picker either way.
fn confirm_preset_picker_selection(dialog: &mut SimplePromptDialog) {
    let filtered = preset_filtered_indices(dialog);
    let SectionPickerMode::PresetPicker {
        selected, entries, ..
    } = &dialog.picker_mode
    else {
        return;
    };
    let content = filtered
        .get(*selected)
        .and_then(|&idx| entries.get(idx))
        .map(|(_, _, content)| content.clone());

    dialog.picker_mode = SectionPickerMode::None;
    if let Some(content) = content {
        dialog.add_section_with_content("instruction", content);
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
    db: &Database,
    workdir: &Path,
) -> PromptAction {
    // Inline date-time picker open: arrows edit, Enter confirms, Esc abandons.
    if dialog.send_edit.is_some() {
        return match code {
            KeyCode::Esc => {
                dialog.send_edit_cancel();
                PromptAction::None
            }
            // Ctrl+S sends without leaving the picker: the in-progress edit is
            // confirmed first; an invalid time keeps the picker open showing
            // its error instead of sending.
            KeyCode::Char('s') if modifiers.contains(KeyModifiers::CONTROL) => {
                dialog.send_edit_confirm();
                if dialog.send_edit.is_some() {
                    PromptAction::None
                } else {
                    build_prompt_action(dialog, db, workdir)
                }
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
        // Ctrl+S sends from the send control too — it must work from every
        // focus, not force the user back into a section first.
        KeyCode::Char('s') if modifiers.contains(KeyModifiers::CONTROL) => {
            build_prompt_action(dialog, db, workdir)
        }
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
        let action = handle_send_control_key(dialog, code, modifiers, is_shift, db, workdir);
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

        // With an empty buffer the tab shows the read-only preview: vertical
        // keys scroll it instead of moving a cursor there is nothing to edit.
        KeyCode::Up if dialog.raw_is_empty() => {
            dialog.scroll_raw_preview(-1);
            PromptAction::None
        }
        KeyCode::Down if dialog.raw_is_empty() => {
            dialog.scroll_raw_preview(1);
            PromptAction::None
        }
        KeyCode::PageUp if dialog.raw_is_empty() => {
            dialog.scroll_raw_preview(-10);
            PromptAction::None
        }
        KeyCode::PageDown if dialog.raw_is_empty() => {
            dialog.scroll_raw_preview(10);
            PromptAction::None
        }

        // Cursor movement inside the raw buffer.
        KeyCode::Left => {
            dialog.move_cursor_left(raw, field_width);
            dialog.raw_edit_scroll = None;
            PromptAction::None
        }
        KeyCode::Right => {
            dialog.move_cursor_right(raw, field_width);
            dialog.raw_edit_scroll = None;
            PromptAction::None
        }
        KeyCode::Up => {
            dialog.move_cursor_up(raw, field_width);
            dialog.raw_edit_scroll = None;
            PromptAction::None
        }
        KeyCode::Down => {
            dialog.move_cursor_down(raw, field_width);
            dialog.raw_edit_scroll = None;
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
        return Ok(handle_send_control_key(
            dialog, code, modifiers, is_shift, db, workdir,
        ));
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

    // Edit-in-place (B33): when the builder is editing an existing scheduled
    // send, re-confirming REPLACES it — delete the old row first, then insert
    // the edited one below, so the list count stays the same (no duplicate).
    let editing_id = app
        .simple_prompt_dialog
        .as_ref()
        .and_then(|d| d.editing_scheduled_id.clone());
    if let Some(old_id) = editing_id.as_deref() {
        if let Err(e) = app.db.delete_scheduled_send(old_id) {
            tracing::warn!("Failed to replace scheduled send '{old_id}': {e}");
        }
    }

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

/// Cancel a pending scheduled send targeting the currently selected
/// interactive session (Ctrl+K). When a list entry is selected (the user is
/// browsing the scheduled-list panel, B33) that entry is canceled; otherwise it
/// falls back to the soonest pending send. No-op if there is none.
fn cancel_scheduled_send(app: &mut App) {
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

    if pending.is_empty() {
        return;
    }

    // A live list selection targets that row; with nothing selected the list is
    // ordered soonest-first, so index 0 is the soonest send.
    let selected = app
        .simple_prompt_dialog
        .as_ref()
        .and_then(|d| d.scheduled_list_selected);
    let idx = selected.unwrap_or(0).min(pending.len() - 1);
    let target_id = pending[idx].id.clone();

    match app.db.delete_scheduled_send(&target_id) {
        Ok(true) => {
            crate::domain::notification::send_notification(
                "Scheduled send canceled",
                "The scheduled prompt was canceled.",
                crate::domain::notification::NotificationLevel::Info,
            );
        }
        Ok(false) => {}
        Err(e) => tracing::warn!("Failed to cancel scheduled send '{target_id}': {e}"),
    }

    // Re-clamp the list selection against the shortened list (or drop out of
    // list-browse mode when the last entry was just canceled).
    if selected.is_some() {
        let remaining = pending.len().saturating_sub(1);
        if let Some(dialog) = app.simple_prompt_dialog.as_mut() {
            dialog.scheduled_list_selected = (remaining > 0).then(|| idx.min(remaining - 1));
        }
    }
}

/// Move keyboard focus into the pending-scheduled-sends list (B33), selecting
/// the first (soonest) entry. No-op when the selected session has no pending
/// sends — there is nothing to browse.
fn enter_scheduled_list(app: &mut App) {
    let Some((target_session_id, _)) = selected_session_target(app) else {
        return;
    };
    let has_pending = app
        .db
        .list_pending_scheduled_sends_for_session(&target_session_id)
        .map(|pending| !pending.is_empty())
        .unwrap_or(false);
    if !has_pending {
        return;
    }
    if let Some(dialog) = app.simple_prompt_dialog.as_mut() {
        dialog.scheduled_list_selected = Some(0);
    }
}

/// Handle a key while the scheduled-sends list panel has focus (B33): arrows
/// move the selection, Enter loads the selected send into the Raw tab for
/// in-place editing, Ctrl+K cancels the selected send, and Esc/Ctrl+P leave the
/// list. The list content is read live from the DB so it always reflects state.
fn handle_scheduled_list_key(app: &mut App, code: KeyCode, modifiers: KeyModifiers) {
    let Some((target_session_id, _)) = selected_session_target(app) else {
        clear_scheduled_list_focus(app);
        return;
    };
    let pending = app
        .db
        .list_pending_scheduled_sends_for_session(&target_session_id)
        .unwrap_or_default();
    if pending.is_empty() {
        clear_scheduled_list_focus(app);
        return;
    }

    let sel = app
        .simple_prompt_dialog
        .as_ref()
        .and_then(|d| d.scheduled_list_selected)
        .unwrap_or(0)
        .min(pending.len() - 1);
    let ctrl = modifiers.contains(KeyModifiers::CONTROL);

    match code {
        KeyCode::Up => set_scheduled_list_selection(app, sel.saturating_sub(1)),
        KeyCode::Down => set_scheduled_list_selection(app, (sel + 1).min(pending.len() - 1)),
        KeyCode::Esc => clear_scheduled_list_focus(app),
        KeyCode::Char('p') if ctrl => clear_scheduled_list_focus(app),
        KeyCode::Char('k') if ctrl => cancel_scheduled_send(app),
        KeyCode::Enter => {
            let send = &pending[sel];
            let fire_local = send.fire_at.with_timezone(&chrono::Local).naive_local();
            let (id, prompt) = (send.id.clone(), send.prompt.clone());
            if let Some(dialog) = app.simple_prompt_dialog.as_mut() {
                dialog.load_scheduled_for_edit(&id, &prompt, fire_local);
            }
        }
        _ => {}
    }
}

fn set_scheduled_list_selection(app: &mut App, index: usize) {
    if let Some(dialog) = app.simple_prompt_dialog.as_mut() {
        dialog.scheduled_list_selected = Some(index);
    }
}

fn clear_scheduled_list_focus(app: &mut App) {
    if let Some(dialog) = app.simple_prompt_dialog.as_mut() {
        dialog.scheduled_list_selected = None;
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
mod preset_picker_tests {
    use super::{handle_preset_picker_key, preset_filtered_indices};
    use crate::tui::app::dialog::{SectionPickerMode, SimplePromptDialog};
    use ratatui::crossterm::event::KeyCode;

    fn dialog_with_presets(entries: Vec<(&str, &str, &str)>) -> SimplePromptDialog {
        let mut dialog = SimplePromptDialog::new();
        dialog.picker_mode = SectionPickerMode::PresetPicker {
            selected: 0,
            entries: entries
                .into_iter()
                .map(|(n, p, c)| (n.to_string(), p.to_string(), c.to_string()))
                .collect(),
            filter: String::new(),
        };
        dialog
    }

    #[test]
    fn typing_filters_the_list_and_resets_selection_to_the_top() {
        let mut dialog = dialog_with_presets(vec![
            ("implementer", "role a", "implementer content"),
            ("reviewer", "role b", "reviewer content"),
            ("resilience", "role c", "resilience content"),
        ]);

        // Move off the top so the filter's reset-to-0 is actually exercised.
        handle_preset_picker_key(&mut dialog, KeyCode::Down);
        handle_preset_picker_key(&mut dialog, KeyCode::Down);

        handle_preset_picker_key(&mut dialog, KeyCode::Char('r'));
        handle_preset_picker_key(&mut dialog, KeyCode::Char('e'));

        let filtered = preset_filtered_indices(&dialog);
        let SectionPickerMode::PresetPicker {
            entries, selected, ..
        } = &dialog.picker_mode
        else {
            panic!("expected PresetPicker");
        };
        let names: Vec<&str> = filtered.iter().map(|&i| entries[i].0.as_str()).collect();
        assert_eq!(names, vec!["reviewer", "resilience"]);
        assert_eq!(*selected, 0);
    }

    #[test]
    fn backspace_removes_a_filter_char_and_widens_the_match() {
        let mut dialog = dialog_with_presets(vec![
            ("implementer", "role a", "implementer content"),
            ("reviewer", "role b", "reviewer content"),
        ]);

        handle_preset_picker_key(&mut dialog, KeyCode::Char('x'));
        assert!(preset_filtered_indices(&dialog).is_empty());

        handle_preset_picker_key(&mut dialog, KeyCode::Backspace);
        assert_eq!(preset_filtered_indices(&dialog).len(), 2);
    }

    #[test]
    fn enter_inserts_the_selected_preset_as_a_new_instruction_section_and_closes() {
        let mut dialog = dialog_with_presets(vec![
            ("implementer", "role a", "implementer content"),
            ("reviewer", "role b", "reviewer content"),
        ]);
        dialog.set_section_content("instruction_1", "existing task".to_string());

        handle_preset_picker_key(&mut dialog, KeyCode::Down); // select "reviewer"
        handle_preset_picker_key(&mut dialog, KeyCode::Enter);

        assert_eq!(dialog.picker_mode, SectionPickerMode::None);
        // The existing instruction is left alone — the preset lands in a
        // fresh, freely-editable section rather than clobbering it.
        assert_eq!(dialog.get_section_content("instruction_1"), "existing task");
        assert_eq!(
            dialog.get_section_content("instruction_2"),
            "reviewer content"
        );
        assert!(dialog
            .enabled_sections
            .contains(&"instruction_2".to_string()));
    }

    #[test]
    fn esc_closes_the_picker_without_inserting_anything() {
        let mut dialog =
            dialog_with_presets(vec![("implementer", "role a", "implementer content")]);
        let original_sections = dialog.enabled_sections.clone();

        handle_preset_picker_key(&mut dialog, KeyCode::Esc);

        assert_eq!(dialog.picker_mode, SectionPickerMode::None);
        assert_eq!(dialog.enabled_sections, original_sections);
    }

    #[test]
    fn enter_on_an_empty_directory_closes_without_inserting_anything() {
        // Mirrors the "no presets in ~/.canopy/prompts" hint state: the
        // picker opened successfully but found nothing to list.
        let mut dialog = dialog_with_presets(vec![]);
        let original_sections = dialog.enabled_sections.clone();

        handle_preset_picker_key(&mut dialog, KeyCode::Enter);

        assert_eq!(dialog.picker_mode, SectionPickerMode::None);
        assert_eq!(dialog.enabled_sections, original_sections);
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

#[cfg(test)]
mod should_expand_tests {
    use super::should_expand_on_key;
    use ratatui::crossterm::event::{KeyCode, KeyModifiers};

    #[test]
    fn char_keys_expand() {
        assert!(should_expand_on_key(KeyCode::Char('a'), KeyModifiers::NONE));
        assert!(should_expand_on_key(
            KeyCode::Char('z'),
            KeyModifiers::SHIFT
        ));
        assert!(should_expand_on_key(KeyCode::Char(' '), KeyModifiers::NONE));
    }

    #[test]
    fn backspace_and_delete_expand() {
        assert!(should_expand_on_key(KeyCode::Backspace, KeyModifiers::NONE));
        assert!(should_expand_on_key(KeyCode::Delete, KeyModifiers::NONE));
    }

    #[test]
    fn enter_with_modifier_expands() {
        assert!(should_expand_on_key(KeyCode::Enter, KeyModifiers::CONTROL));
    }

    #[test]
    fn enter_without_modifier_does_not_expand() {
        assert!(!should_expand_on_key(KeyCode::Enter, KeyModifiers::NONE));
    }

    #[test]
    fn navigation_keys_do_not_expand() {
        assert!(!should_expand_on_key(KeyCode::Up, KeyModifiers::NONE));
        assert!(!should_expand_on_key(KeyCode::Down, KeyModifiers::NONE));
        assert!(!should_expand_on_key(KeyCode::Left, KeyModifiers::NONE));
        assert!(!should_expand_on_key(KeyCode::Right, KeyModifiers::NONE));
        assert!(!should_expand_on_key(KeyCode::Tab, KeyModifiers::NONE));
        assert!(!should_expand_on_key(KeyCode::BackTab, KeyModifiers::SHIFT));
    }

    #[test]
    fn other_keys_do_not_expand() {
        assert!(!should_expand_on_key(KeyCode::Esc, KeyModifiers::NONE));
        assert!(!should_expand_on_key(KeyCode::PageUp, KeyModifiers::NONE));
        assert!(!should_expand_on_key(KeyCode::PageDown, KeyModifiers::NONE));
        assert!(!should_expand_on_key(KeyCode::Home, KeyModifiers::NONE));
        assert!(!should_expand_on_key(KeyCode::End, KeyModifiers::NONE));
    }
}

#[cfg(test)]
mod focused_section_name_tests {
    use super::focused_section_name;
    use crate::tui::app::dialog::SimplePromptDialog;

    #[test]
    fn focus_zero_returns_none() {
        let mut dialog = SimplePromptDialog::new();
        dialog.focused_section = 0;
        assert!(focused_section_name(&mut dialog).is_none());
    }

    #[test]
    fn focus_one_returns_first_section() {
        let mut dialog = SimplePromptDialog::new();
        dialog.focused_section = 1;
        assert_eq!(
            focused_section_name(&mut dialog),
            Some("instruction_1".to_string())
        );
    }

    #[test]
    fn stale_focus_is_clamped() {
        let mut dialog = SimplePromptDialog::new();
        // Only 1 enabled section; focus 5 is out of bounds.
        dialog.focused_section = 5;
        let name = focused_section_name(&mut dialog);
        assert!(name.is_some());
        assert_eq!(
            dialog.focused_section, 1,
            "should be clamped to valid index"
        );
    }

    #[test]
    fn no_enabled_sections_returns_none_for_nonzero_focus() {
        let mut dialog = SimplePromptDialog::new();
        dialog.enabled_sections.clear();
        dialog.focused_section = 1;
        assert!(focused_section_name(&mut dialog).is_none());
    }
}

#[cfg(test)]
mod normalize_altgr_char_tests {
    use super::normalize_altgr_char;

    #[test]
    fn maps_q_q_and_2_to_at() {
        assert_eq!(normalize_altgr_char('q'), '@');
        assert_eq!(normalize_altgr_char('Q'), '@');
        assert_eq!(normalize_altgr_char('2'), '@');
    }

    #[test]
    fn other_chars_pass_through() {
        assert_eq!(normalize_altgr_char('a'), 'a');
        assert_eq!(normalize_altgr_char('z'), 'z');
        assert_eq!(normalize_altgr_char(' '), ' ');
        assert_eq!(normalize_altgr_char('1'), '1');
    }
}

#[cfg(test)]
mod is_instruction_section_tests {
    use super::is_instruction_section;

    #[test]
    fn plain_instruction() {
        assert!(is_instruction_section("instruction"));
    }

    #[test]
    fn numbered_instruction() {
        assert!(is_instruction_section("instruction_1"));
        assert!(is_instruction_section("instruction_42"));
    }

    #[test]
    fn non_instruction_sections() {
        assert!(!is_instruction_section("tools"));
        assert!(!is_instruction_section("project_context"));
        assert!(!is_instruction_section("context"));
        assert!(!is_instruction_section(""));
        assert!(!is_instruction_section("instructions"));
    }
}

#[cfg(test)]
mod picker_navigation_tests {
    use super::*;
    use crate::db::Database;
    use crate::tui::app::dialog::{ProjectPickerEntry, SectionPickerMode, SimplePromptDialog};
    use std::sync::Arc;
    use tempfile::NamedTempFile;

    #[test]
    fn skills_picker_up_at_zero_wraps() {
        let mut dialog = SimplePromptDialog::new();
        dialog.picker_mode = SectionPickerMode::SkillsPicker {
            selected: 0,
            entries: vec![
                ("s1".into(), "r1".into(), "skill".into()),
                ("s2".into(), "r2".into(), "skill".into()),
            ],
            replace_id: None,
        };
        handle_skills_picker_key(&mut dialog, 0, 2, KeyCode::Up);
        if let SectionPickerMode::SkillsPicker { selected, .. } = &dialog.picker_mode {
            assert_eq!(*selected, 0, "stays at zero, no wrap");
        } else {
            panic!("expected SkillsPicker");
        }
    }

    #[test]
    fn skills_picker_down_at_end_wraps() {
        let mut dialog = SimplePromptDialog::new();
        dialog.picker_mode = SectionPickerMode::SkillsPicker {
            selected: 1,
            entries: vec![
                ("s1".into(), "r1".into(), "skill".into()),
                ("s2".into(), "r2".into(), "skill".into()),
            ],
            replace_id: None,
        };
        handle_skills_picker_key(&mut dialog, 1, 2, KeyCode::Down);
        if let SectionPickerMode::SkillsPicker { selected, .. } = &dialog.picker_mode {
            assert_eq!(*selected, 1, "stays at end, no wrap");
        } else {
            panic!("expected SkillsPicker");
        }
    }

    #[test]
    fn skills_picker_esc_closes() {
        let mut dialog = SimplePromptDialog::new();
        dialog.picker_mode = SectionPickerMode::SkillsPicker {
            selected: 0,
            entries: vec![("s1".into(), "r1".into(), "skill".into())],
            replace_id: None,
        };
        handle_skills_picker_key(&mut dialog, 0, 1, KeyCode::Esc);
        assert_eq!(dialog.picker_mode, SectionPickerMode::None);
    }

    #[test]
    fn project_picker_navigation() {
        let mut dialog = SimplePromptDialog::new();
        dialog.picker_mode = SectionPickerMode::ProjectPicker {
            selected: 0,
            entries: vec![
                ProjectPickerEntry {
                    hash: "h1".into(),
                    name: "p1".into(),
                    path: "/p1".into(),
                },
                ProjectPickerEntry {
                    hash: "h2".into(),
                    name: "p2".into(),
                    path: "/p2".into(),
                },
            ],
        };
        handle_project_picker_key(&mut dialog, 0, 2, KeyCode::Down);
        if let SectionPickerMode::ProjectPicker { selected, .. } = &dialog.picker_mode {
            assert_eq!(*selected, 1);
        } else {
            panic!("expected ProjectPicker");
        }
    }

    #[test]
    fn project_picker_esc_closes() {
        let mut dialog = SimplePromptDialog::new();
        dialog.picker_mode = SectionPickerMode::ProjectPicker {
            selected: 0,
            entries: vec![ProjectPickerEntry {
                hash: "h1".into(),
                name: "p1".into(),
                path: "/p1".into(),
            }],
        };
        handle_project_picker_key(&mut dialog, 0, 1, KeyCode::Esc);
        assert_eq!(dialog.picker_mode, SectionPickerMode::None);
    }

    #[test]
    fn add_custom_section_esc_closes() {
        let mut dialog = SimplePromptDialog::new();
        dialog.picker_mode = SectionPickerMode::AddCustom {
            input: "test".into(),
        };
        handle_add_custom_section_key(&mut dialog, "test".into(), KeyCode::Esc);
        assert_eq!(dialog.picker_mode, SectionPickerMode::None);
    }

    #[test]
    fn add_custom_section_char_appends() {
        let mut dialog = SimplePromptDialog::new();
        dialog.picker_mode = SectionPickerMode::AddCustom {
            input: String::new(),
        };
        handle_add_custom_section_key(&mut dialog, String::new(), KeyCode::Char('x'));
        if let SectionPickerMode::AddCustom { input } = &dialog.picker_mode {
            assert_eq!(input, "x");
        } else {
            panic!("expected AddCustom");
        }
    }

    #[test]
    fn add_custom_section_backspace_removes() {
        let mut dialog = SimplePromptDialog::new();
        dialog.picker_mode = SectionPickerMode::AddCustom {
            input: "abc".into(),
        };
        handle_add_custom_section_key(&mut dialog, "abc".into(), KeyCode::Backspace);
        if let SectionPickerMode::AddCustom { input } = &dialog.picker_mode {
            assert_eq!(input, "ab");
        } else {
            panic!("expected AddCustom");
        }
    }

    #[test]
    fn remove_section_picker_navigation() {
        let mut dialog = SimplePromptDialog::new();
        dialog.picker_mode = SectionPickerMode::RemoveSection { selected: 1 };
        handle_remove_section_picker_key(&mut dialog, 1, KeyCode::Up);
        if let SectionPickerMode::RemoveSection { selected } = &dialog.picker_mode {
            assert_eq!(*selected, 0);
        } else {
            panic!("expected RemoveSection");
        }
    }

    #[test]
    fn remove_section_picker_esc_closes() {
        let mut dialog = SimplePromptDialog::new();
        dialog.picker_mode = SectionPickerMode::RemoveSection { selected: 0 };
        handle_remove_section_picker_key(&mut dialog, 0, KeyCode::Esc);
        assert_eq!(dialog.picker_mode, SectionPickerMode::None);
    }

    fn test_db() -> Arc<Database> {
        let tmp = NamedTempFile::new().expect("create temp file");
        let path = tmp.path().to_path_buf();
        std::mem::forget(tmp);
        Arc::new(Database::new(&path).expect("create test db"))
    }

    #[test]
    fn add_section_picker_up_at_zero_is_noop() {
        let mut dialog = SimplePromptDialog::new();
        dialog.picker_mode = SectionPickerMode::AddSection { selected: 0 };
        let db = test_db();
        let workdir = Path::new("/tmp");
        handle_add_section_picker_key(&mut dialog, 0, &db, workdir, KeyCode::Up).unwrap();
        if let SectionPickerMode::AddSection { selected } = &dialog.picker_mode {
            assert_eq!(*selected, 0, "should stay at 0");
        } else {
            panic!("expected AddSection");
        }
    }

    #[test]
    fn add_section_picker_custom_switches_mode() {
        let mut dialog = SimplePromptDialog::new();
        dialog.picker_mode = SectionPickerMode::AddSection { selected: 0 };
        let db = test_db();
        let workdir = Path::new("/tmp");
        handle_add_section_picker_key(&mut dialog, 0, &db, workdir, KeyCode::Char('c')).unwrap();
        assert!(matches!(
            dialog.picker_mode,
            SectionPickerMode::AddCustom { .. }
        ));
    }
}

#[cfg(test)]
mod utility_function_tests {
    use super::*;
    use ratatui::crossterm::event::KeyCode;

    /// should_expand_on_key expands on regular character input
    #[test]
    fn should_expand_on_char_input() {
        assert!(should_expand_on_key(
            KeyCode::Char('a'),
            KeyModifiers::empty()
        ));
        assert!(should_expand_on_key(
            KeyCode::Char('Z'),
            KeyModifiers::empty()
        ));
        assert!(should_expand_on_key(
            KeyCode::Char('1'),
            KeyModifiers::empty()
        ));
        assert!(should_expand_on_key(
            KeyCode::Char(' '),
            KeyModifiers::empty()
        ));
    }

    /// should_expand_on_key expands on Backspace and Delete
    #[test]
    fn should_expand_on_delete_keys() {
        assert!(should_expand_on_key(
            KeyCode::Backspace,
            KeyModifiers::empty()
        ));
        assert!(should_expand_on_key(KeyCode::Delete, KeyModifiers::empty()));
    }

    /// should_expand_on_key doesn't expand on navigation keys
    #[test]
    fn should_not_expand_on_navigation() {
        assert!(!should_expand_on_key(KeyCode::Up, KeyModifiers::empty()));
        assert!(!should_expand_on_key(KeyCode::Down, KeyModifiers::empty()));
        assert!(!should_expand_on_key(KeyCode::Left, KeyModifiers::empty()));
        assert!(!should_expand_on_key(KeyCode::Right, KeyModifiers::empty()));
        assert!(!should_expand_on_key(KeyCode::Tab, KeyModifiers::empty()));
        assert!(!should_expand_on_key(
            KeyCode::BackTab,
            KeyModifiers::empty()
        ));
    }

    /// should_expand_on_key expands on Enter only with modifiers
    #[test]
    fn should_expand_on_enter_with_modifiers() {
        assert!(!should_expand_on_key(KeyCode::Enter, KeyModifiers::empty()));
        assert!(should_expand_on_key(KeyCode::Enter, KeyModifiers::SHIFT));
        assert!(should_expand_on_key(KeyCode::Enter, KeyModifiers::CONTROL));
    }

    /// should_send_on_shift_enter requires both conditions
    #[test]
    fn should_send_on_shift_enter_both_required() {
        assert!(!should_send_on_shift_enter(false, false));
        assert!(!should_send_on_shift_enter(true, false));
        assert!(!should_send_on_shift_enter(false, true));
        assert!(should_send_on_shift_enter(true, true));
    }

    /// normalize_prompt_char_input passes through chars with no modifiers
    #[test]
    fn normalize_prompt_char_input_no_modifiers() {
        assert_eq!(
            normalize_prompt_char_input('a', KeyModifiers::empty()),
            Some('a')
        );
        assert_eq!(
            normalize_prompt_char_input('Z', KeyModifiers::empty()),
            Some('Z')
        );
    }

    /// normalize_prompt_char_input passes through chars with only SHIFT
    #[test]
    fn normalize_prompt_char_input_shift_only() {
        assert_eq!(
            normalize_prompt_char_input('a', KeyModifiers::SHIFT),
            Some('a')
        );
        assert_eq!(
            normalize_prompt_char_input('Z', KeyModifiers::SHIFT),
            Some('Z')
        );
    }

    /// normalize_prompt_char_input returns None for CTRL alone
    #[test]
    fn normalize_prompt_char_input_ctrl_alone() {
        assert_eq!(
            normalize_prompt_char_input('a', KeyModifiers::CONTROL),
            None
        );
    }

    /// normalize_prompt_char_input normalizes AltGr chars (Ctrl+Alt)
    #[test]
    fn normalize_prompt_char_input_altgr() {
        let altgr = KeyModifiers::CONTROL | KeyModifiers::ALT;
        assert_eq!(normalize_prompt_char_input('q', altgr), Some('@'));
        assert_eq!(normalize_prompt_char_input('Q', altgr), Some('@'));
        assert_eq!(normalize_prompt_char_input('2', altgr), Some('@'));
        // Other chars pass through
        assert_eq!(normalize_prompt_char_input('x', altgr), Some('x'));
    }

    /// normalize_altgr_char maps known AltGr sequences
    #[test]
    fn normalize_altgr_char_mappings() {
        assert_eq!(normalize_altgr_char('q'), '@');
        assert_eq!(normalize_altgr_char('Q'), '@');
        assert_eq!(normalize_altgr_char('2'), '@');
    }

    /// normalize_altgr_char passes through other characters
    #[test]
    fn normalize_altgr_char_passthrough() {
        assert_eq!(normalize_altgr_char('a'), 'a');
        assert_eq!(normalize_altgr_char('Z'), 'Z');
        assert_eq!(normalize_altgr_char('1'), '1');
        assert_eq!(normalize_altgr_char('@'), '@');
    }

    /// is_instruction_section recognizes "instruction"
    #[test]
    fn is_instruction_section_exact_match() {
        assert!(is_instruction_section("instruction"));
    }

    /// is_instruction_section recognizes "instruction_*" prefixes
    #[test]
    fn is_instruction_section_with_prefix() {
        assert!(is_instruction_section("instruction_first"));
        assert!(is_instruction_section("instruction_system"));
        assert!(is_instruction_section("instruction_"));
        assert!(is_instruction_section("instruction_complex_name"));
    }

    /// is_instruction_section rejects non-instruction sections
    #[test]
    fn is_instruction_section_rejects_other() {
        assert!(!is_instruction_section("context"));
        assert!(!is_instruction_section("knowledge"));
        assert!(!is_instruction_section("tools"));
        assert!(!is_instruction_section("instruct"));
        assert!(!is_instruction_section("instructions"));
        assert!(!is_instruction_section(""));
    }

    /// should_expand_on_key with various modifier combinations
    #[test]
    fn should_expand_on_key_modifier_combinations() {
        // Alt alone should not affect char expansion
        assert!(should_expand_on_key(KeyCode::Char('a'), KeyModifiers::ALT));
        // Ctrl alone should not affect char expansion
        assert!(should_expand_on_key(
            KeyCode::Char('a'),
            KeyModifiers::CONTROL
        ));
        // Super should not affect char expansion
        assert!(should_expand_on_key(
            KeyCode::Char('a'),
            KeyModifiers::SUPER
        ));
    }

    /// normalize_prompt_char_input with all modifier combinations
    #[test]
    fn normalize_prompt_char_input_all_combinations() {
        let alt_only = KeyModifiers::ALT;
        let super_only = KeyModifiers::SUPER;

        // Alt alone: None
        assert_eq!(normalize_prompt_char_input('a', alt_only), None);
        // Super alone: None
        assert_eq!(normalize_prompt_char_input('a', super_only), None);
        // Shift+Ctrl: None (not treated specially)
        assert_eq!(
            normalize_prompt_char_input('a', KeyModifiers::SHIFT | KeyModifiers::CONTROL),
            None
        );
    }

    /// is_instruction_section with case sensitivity
    #[test]
    fn is_instruction_section_case_sensitive() {
        assert!(is_instruction_section("instruction"));
        assert!(!is_instruction_section("Instruction"));
        assert!(!is_instruction_section("INSTRUCTION"));
        assert!(!is_instruction_section("iNsTrUcTiOn"));
    }

    /// should_expand_on_key exhaustive key coverage
    #[test]
    fn should_expand_on_key_escape_and_special() {
        // Escape and function keys should not expand
        assert!(!should_expand_on_key(KeyCode::Esc, KeyModifiers::empty()));
        assert!(!should_expand_on_key(KeyCode::F(1), KeyModifiers::empty()));
        assert!(!should_expand_on_key(KeyCode::Home, KeyModifiers::empty()));
        assert!(!should_expand_on_key(KeyCode::End, KeyModifiers::empty()));
        assert!(!should_expand_on_key(
            KeyCode::PageUp,
            KeyModifiers::empty()
        ));
        assert!(!should_expand_on_key(
            KeyCode::PageDown,
            KeyModifiers::empty()
        ));
    }
}
