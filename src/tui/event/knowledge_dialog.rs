use ratatui::crossterm::event::KeyCode;

use crate::db::intelligence::IntelligenceNodeInput;
use crate::tui::app::dialog::KnowledgeDialog;
use crate::tui::app::types::Focus;
use crate::tui::app::App;

pub fn handle_knowledge_dialog_key(app: &mut App, code: KeyCode) -> anyhow::Result<()> {
    let Some(dialog) = app.knowledge_dialog.as_mut() else {
        return Ok(());
    };

    match code {
        KeyCode::Esc => {
            close_knowledge_dialog(app);
        }
        KeyCode::Tab => {
            dialog.next_field();
        }
        KeyCode::BackTab => {
            dialog.prev_field();
        }
        KeyCode::Enter => {
            if dialog.field == 2 {
                save_knowledge_dialog(app)?;
            } else {
                dialog.next_field();
            }
        }
        KeyCode::Char(' ') if dialog.field == 2 => {
            dialog.cycle_kind();
        }
        KeyCode::Char(c) => match dialog.field {
            0 => dialog.title.push(c),
            1 => dialog.body.push(c),
            _ => {}
        },
        KeyCode::Backspace => match dialog.field {
            0 => {
                dialog.title.pop();
            }
            1 => {
                dialog.body.pop();
            }
            _ => {}
        },
        _ => {}
    }

    Ok(())
}

fn close_knowledge_dialog(app: &mut App) {
    let prev_focus = app
        .knowledge_dialog
        .as_ref()
        .and_then(|d| d.prev_focus)
        .unwrap_or(Focus::Home);
    app.knowledge_dialog = None;
    app.focus = prev_focus;
}

fn save_knowledge_dialog(app: &mut App) -> anyhow::Result<()> {
    let Some(dialog) = app.knowledge_dialog.take() else {
        return Ok(());
    };

    if dialog.title.trim().is_empty() {
        app.focus = dialog.prev_focus.unwrap_or(Focus::Home);
        return Ok(());
    }

    let input = IntelligenceNodeInput {
        id: dialog.edit_id.clone(),
        kind: Some(dialog.kind_str().to_string()),
        status: Some("noted".to_string()),
        title: Some(dialog.title.clone()),
        body: Some(dialog.body.clone()),
        body_replace: None,
        metadata: None,
        project_hash: dialog.project_hash.clone().map(Some),
        session_id: None,
        relations: None,
    };

    app.db.upsert_intelligence_node(input)?;
    app.record_gardener_edit().ok();

    app.refresh_project_knowledge()?;
    app.focus = dialog.prev_focus.unwrap_or(Focus::Home);

    Ok(())
}

pub fn open_knowledge_dialog(app: &mut App) {
    let project_hash = app
        .projects
        .get(app.selected_project)
        .map(|p| p.hash.clone());

    let mut dialog = KnowledgeDialog::new(project_hash);
    dialog.prev_focus = Some(app.focus);
    app.knowledge_dialog = Some(dialog);
    app.focus = Focus::KnowledgeDialog;
}

pub fn edit_knowledge_dialog(app: &mut App) {
    let Some(node) = app.project_knowledge.get(app.selected_knowledge).cloned() else {
        return;
    };

    let kind = if node.kind == "pattern" {
        crate::tui::app::dialog::KnowledgeKind::Pattern
    } else {
        crate::tui::app::dialog::KnowledgeKind::Fact
    };

    let mut dialog = KnowledgeDialog::edit(node.id, node.title, node.body, kind, node.project_hash);
    dialog.prev_focus = Some(app.focus);
    app.knowledge_dialog = Some(dialog);
    app.focus = Focus::KnowledgeDialog;
}
