use anyhow::Result;
use ratatui::crossterm::event::{KeyCode, KeyModifiers};

use crate::tui::app::dialog::loop_form::FIELD_TRIGGER_KIND;
use crate::tui::app::types::App;

pub fn handle_loop_form_key(app: &mut App, code: KeyCode, modifiers: KeyModifiers) -> Result<()> {
    match code {
        KeyCode::Esc => app.close_loop_form_dialog(),
        KeyCode::Enter => app.save_loop_form_dialog()?,
        KeyCode::Tab | KeyCode::Down => {
            if let Some(dialog) = app.loop_form_dialog.as_mut() {
                dialog.next_field();
            }
        }
        KeyCode::BackTab | KeyCode::Up => {
            if let Some(dialog) = app.loop_form_dialog.as_mut() {
                dialog.prev_field();
            }
        }
        KeyCode::Left | KeyCode::Right => {
            if let Some(dialog) = app.loop_form_dialog.as_mut() {
                if dialog.field == FIELD_TRIGGER_KIND {
                    dialog.error = None;
                    dialog.cycle_trigger(code == KeyCode::Right);
                }
            }
        }
        KeyCode::Char(' ') => {
            if let Some(dialog) = app.loop_form_dialog.as_mut() {
                if dialog.field == FIELD_TRIGGER_KIND {
                    dialog.error = None;
                    dialog.cycle_trigger(true);
                } else if let Some(text) = dialog.focused_text_mut() {
                    text.push(' ');
                    dialog.error = None;
                }
            }
        }
        KeyCode::Backspace => {
            if let Some(dialog) = app.loop_form_dialog.as_mut() {
                if let Some(text) = dialog.focused_text_mut() {
                    text.pop();
                    dialog.error = None;
                }
            }
        }
        KeyCode::Char(value) if !modifiers.contains(KeyModifiers::CONTROL) => {
            if let Some(dialog) = app.loop_form_dialog.as_mut() {
                if let Some(text) = dialog.focused_text_mut() {
                    text.push(value);
                    dialog.error = None;
                }
            }
        }
        _ => {}
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Database;
    use crate::tui::app::dialog::loop_form::{FIELD_NAME, FIELD_TRIGGER_VALUE};
    use crate::tui::app::dialog::LoopTriggerChoice;
    use crate::tui::app::types::Focus;
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
        handle_loop_form_key(app, code, KeyModifiers::NONE).expect("key handled");
    }

    #[test]
    fn typing_edits_the_focused_field_and_tab_navigates() {
        let (mut app, _dir) = test_app();
        app.open_new_loop_dialog();

        press(&mut app, KeyCode::Char('h'));
        press(&mut app, KeyCode::Char('i'));
        press(&mut app, KeyCode::Backspace);
        assert_eq!(app.loop_form_dialog.as_ref().unwrap().name, "h");

        press(&mut app, KeyCode::Tab);
        press(&mut app, KeyCode::Char('d'));
        let dialog = app.loop_form_dialog.as_ref().unwrap();
        assert_eq!(dialog.description, "d");

        press(&mut app, KeyCode::BackTab);
        assert_eq!(app.loop_form_dialog.as_ref().unwrap().field, FIELD_NAME);
    }

    #[test]
    fn arrows_cycle_the_trigger_only_on_the_trigger_field() {
        let (mut app, _dir) = test_app();
        app.open_new_loop_dialog();

        // On the name field, Left/Right must not touch the trigger.
        press(&mut app, KeyCode::Right);
        assert_eq!(
            app.loop_form_dialog.as_ref().unwrap().trigger_choice,
            LoopTriggerChoice::Manual
        );

        app.loop_form_dialog.as_mut().unwrap().field = FIELD_TRIGGER_KIND;
        press(&mut app, KeyCode::Right);
        assert_eq!(
            app.loop_form_dialog.as_ref().unwrap().trigger_choice,
            LoopTriggerChoice::Cron
        );
        press(&mut app, KeyCode::Left);
        assert_eq!(
            app.loop_form_dialog.as_ref().unwrap().trigger_choice,
            LoopTriggerChoice::Manual
        );
    }

    #[test]
    fn down_reaches_the_trigger_value_field_for_a_cron_loop() {
        let (mut app, _dir) = test_app();
        app.open_new_loop_dialog();
        {
            let dialog = app.loop_form_dialog.as_mut().unwrap();
            dialog.trigger_choice = LoopTriggerChoice::Cron;
            dialog.cron_expr.clear();
        }

        for _ in 0..4 {
            press(&mut app, KeyCode::Down);
        }
        press(&mut app, KeyCode::Char('5'));

        let dialog = app.loop_form_dialog.as_ref().unwrap();
        assert_eq!(dialog.field, FIELD_TRIGGER_VALUE);
        assert_eq!(dialog.cron_expr, "5");
    }

    #[test]
    fn esc_closes_the_dialog_and_restores_the_previous_focus() {
        let (mut app, _dir) = test_app();
        app.focus = Focus::Preview;
        app.open_new_loop_dialog();
        assert!(matches!(app.focus, Focus::LoopFormDialog));

        press(&mut app, KeyCode::Esc);

        assert!(app.loop_form_dialog.is_none());
        assert!(matches!(app.focus, Focus::Preview));
    }

    #[test]
    fn enter_runs_save_and_local_validation_keeps_the_dialog_open() {
        let (mut app, _dir) = test_app();
        app.open_new_loop_dialog();

        // Name left empty — save must surface the validation error inline.
        press(&mut app, KeyCode::Enter);

        let dialog = app.loop_form_dialog.as_ref().expect("dialog stays open");
        assert!(dialog.error.as_deref().unwrap_or_default().contains("name"));
    }
}
