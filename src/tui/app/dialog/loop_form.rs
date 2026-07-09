//! `LoopFormDialog` — state and logic for creating/editing a loop's
//! metadata and trigger from the TUI.
//!
//! Saving goes through the daemon's MCP `loop_create`/`loop_update` tools
//! (via [`crate::tui::mcp_client`]) instead of writing the DB directly: the
//! daemon owns the live trigger wiring (waking the cron scheduler, starting
//! and stopping file watchers), so a direct DB write would leave a watch
//! loop without a running watcher until the daemon restarts.

use anyhow::Result;

use crate::application::ports::StateRepository;
use crate::daemon::handler::{build_loop_trigger, validate_absolute_dir, validate_non_empty};
use crate::daemon::params::LoopTriggerParams;
use crate::domain::loops::Loop;
use crate::domain::models::Trigger;
use crate::tui::app::types::{App, Focus};
use crate::tui::mcp_client;

/// Trigger choice inside the loop form.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum LoopTriggerChoice {
    Manual,
    Cron,
    Watch,
}

impl LoopTriggerChoice {
    pub fn label(self) -> &'static str {
        match self {
            Self::Manual => "manual",
            Self::Cron => "cron",
            Self::Watch => "watch",
        }
    }

    fn cycled(self, forward: bool) -> Self {
        match (self, forward) {
            (Self::Manual, true) | (Self::Watch, false) => Self::Cron,
            (Self::Cron, true) | (Self::Manual, false) => Self::Watch,
            (Self::Watch, true) | (Self::Cron, false) => Self::Manual,
        }
    }
}

/// Field indices for keyboard focus inside the form.
pub const FIELD_NAME: usize = 0;
pub const FIELD_DESCRIPTION: usize = 1;
pub const FIELD_WORKDIR: usize = 2;
pub const FIELD_TRIGGER_KIND: usize = 3;
/// Cron expression or watch path, depending on the trigger choice.
pub const FIELD_TRIGGER_VALUE: usize = 4;

/// State for the loop create/edit dialog.
pub struct LoopFormDialog {
    /// When `Some(id)`, the dialog is in edit mode for an existing loop.
    pub edit_id: Option<String>,
    pub name: String,
    pub description: String,
    pub workdir: String,
    pub trigger_choice: LoopTriggerChoice,
    pub cron_expr: String,
    pub watch_path: String,
    /// Watch events sent with a watch trigger. Not editable in the form
    /// (mirrors the agent dialog); preserved from the loop when editing.
    pub watch_events: Vec<String>,
    /// Watch debounce/recursive are not exposed in the form either; they are
    /// carried through so editing a watch loop does not reset them.
    pub watch_debounce: Option<u64>,
    pub watch_recursive: Option<bool>,
    pub field: usize,
    /// Validation or daemon error shown inside the dialog.
    pub error: Option<String>,
    pub prev_focus: Option<Focus>,
}

impl LoopFormDialog {
    pub fn new(start_dir: Option<&str>) -> Self {
        let workdir = start_dir.map(str::to_string).unwrap_or_else(|| {
            std::env::current_dir()
                .map(|p| p.to_string_lossy().to_string())
                .unwrap_or_default()
        });
        Self {
            edit_id: None,
            name: String::new(),
            description: String::new(),
            watch_path: workdir.clone(),
            workdir,
            trigger_choice: LoopTriggerChoice::Manual,
            cron_expr: "0 9 * * *".to_string(),
            watch_events: vec!["create".to_string(), "modify".to_string()],
            watch_debounce: None,
            watch_recursive: None,
            field: FIELD_NAME,
            error: None,
            prev_focus: None,
        }
    }

    /// Prefill the form from an existing loop (edit mode).
    pub fn for_loop(lp: &Loop) -> Self {
        let mut dialog = Self::new(Some(&lp.workdir));
        dialog.edit_id = Some(lp.id.clone());
        dialog.name = lp.name.clone();
        dialog.description = lp.description.clone().unwrap_or_default();
        match &lp.trigger {
            Some(Trigger::Cron { schedule_expr }) => {
                dialog.trigger_choice = LoopTriggerChoice::Cron;
                dialog.cron_expr = schedule_expr.clone();
            }
            Some(Trigger::Watch {
                path,
                events,
                debounce_seconds,
                recursive,
            }) => {
                dialog.trigger_choice = LoopTriggerChoice::Watch;
                dialog.watch_path = path.clone();
                dialog.watch_events = events
                    .iter()
                    .map(|e| e.to_string().to_lowercase())
                    .collect();
                dialog.watch_debounce = Some(*debounce_seconds);
                dialog.watch_recursive = Some(*recursive);
            }
            None => dialog.trigger_choice = LoopTriggerChoice::Manual,
        }
        dialog
    }

    pub fn is_edit_mode(&self) -> bool {
        self.edit_id.is_some()
    }

    /// Number of navigable fields for the current trigger choice.
    pub fn field_count(&self) -> usize {
        match self.trigger_choice {
            LoopTriggerChoice::Manual => 4,
            LoopTriggerChoice::Cron | LoopTriggerChoice::Watch => 5,
        }
    }

    pub fn next_field(&mut self) {
        self.field = (self.field + 1) % self.field_count();
    }

    pub fn prev_field(&mut self) {
        self.field = self.field.checked_sub(1).unwrap_or(self.field_count() - 1);
    }

    /// Cycle the trigger choice, keeping the focused field in range (a
    /// manual loop has no trigger-value field).
    pub fn cycle_trigger(&mut self, forward: bool) {
        self.trigger_choice = self.trigger_choice.cycled(forward);
        if self.field >= self.field_count() {
            self.field = FIELD_TRIGGER_KIND;
        }
    }

    /// The editable text buffer behind the focused field, if any.
    pub fn focused_text_mut(&mut self) -> Option<&mut String> {
        match self.field {
            FIELD_NAME => Some(&mut self.name),
            FIELD_DESCRIPTION => Some(&mut self.description),
            FIELD_WORKDIR => Some(&mut self.workdir),
            FIELD_TRIGGER_VALUE => match self.trigger_choice {
                LoopTriggerChoice::Cron => Some(&mut self.cron_expr),
                LoopTriggerChoice::Watch => Some(&mut self.watch_path),
                LoopTriggerChoice::Manual => None,
            },
            _ => None,
        }
    }

    /// The MCP `trigger` parameter matching the current form state.
    pub fn trigger_params(&self) -> LoopTriggerParams {
        let mut params = LoopTriggerParams {
            kind: self.trigger_choice.label().to_string(),
            schedule: None,
            path: None,
            events: None,
            debounce_seconds: None,
            recursive: None,
        };
        match self.trigger_choice {
            LoopTriggerChoice::Manual => {}
            LoopTriggerChoice::Cron => params.schedule = Some(self.cron_expr.clone()),
            LoopTriggerChoice::Watch => {
                params.path = Some(self.watch_path.clone());
                params.events = Some(self.watch_events.clone());
                params.debounce_seconds = self.watch_debounce;
                params.recursive = self.watch_recursive;
            }
        }
        params
    }

    /// Client-side validation reusing the daemon's own checks (the same
    /// functions `loop_create`/`loop_update` run — not a reimplementation)
    /// for instant feedback before the MCP round-trip.
    pub fn validate(&self) -> Result<(), String> {
        validate_non_empty(&self.name, "Loop name")?;
        validate_non_empty(&self.workdir, "Loop workdir")?;
        validate_absolute_dir(self.workdir.trim())?;
        build_loop_trigger(&Some(self.trigger_params()))?;
        Ok(())
    }

    /// Arguments for the MCP `loop_create` call.
    pub fn create_arguments(&self) -> serde_json::Value {
        serde_json::json!({
            "name": self.name.trim(),
            "description": self.description.trim(),
            "workdir": self.workdir.trim(),
            "trigger": self.trigger_params(),
        })
    }

    /// Arguments for the MCP `loop_update` call. Sends every field the form
    /// owns; an empty description clears the loop's description server-side.
    pub fn update_arguments(&self) -> serde_json::Value {
        serde_json::json!({
            "loop_id": self.edit_id.as_deref().unwrap_or_default(),
            "name": self.name.trim(),
            "description": self.description.trim(),
            "workdir": self.workdir.trim(),
            "trigger": self.trigger_params(),
        })
    }
}

impl App {
    /// Open the loop form in create mode (`n` on the Loops panel).
    pub fn open_new_loop_dialog(&mut self) {
        let start_dir = self
            .projects
            .get(self.selected_project)
            .map(|p| p.path.clone());
        let mut dialog = LoopFormDialog::new(start_dir.as_deref());
        dialog.prev_focus = Some(self.focus);
        self.loop_form_dialog = Some(dialog);
        self.focus = Focus::LoopFormDialog;
    }

    /// Open the loop form in edit mode for the selected loop (`e`).
    pub fn open_edit_loop_dialog(&mut self) {
        let Some(lp) = self.selected_loop().cloned() else {
            return;
        };
        let mut dialog = LoopFormDialog::for_loop(&lp);
        dialog.prev_focus = Some(self.focus);
        self.loop_form_dialog = Some(dialog);
        self.focus = Focus::LoopFormDialog;
    }

    /// Close the loop form without saving.
    pub fn close_loop_form_dialog(&mut self) {
        let prev = self
            .loop_form_dialog
            .take()
            .and_then(|dialog| dialog.prev_focus)
            .unwrap_or(Focus::Preview);
        self.focus = prev;
    }

    /// Validate and persist the loop form through the daemon MCP tools.
    /// Validation and daemon errors keep the dialog open showing the message.
    pub fn save_loop_form_dialog(&mut self) -> Result<()> {
        let Some(dialog) = self.loop_form_dialog.as_mut() else {
            return Ok(());
        };
        if let Err(message) = dialog.validate() {
            dialog.error = Some(message);
            return Ok(());
        }

        let (tool, arguments) = if dialog.is_edit_mode() {
            ("loop_update", dialog.update_arguments())
        } else {
            ("loop_create", dialog.create_arguments())
        };
        let port = self
            .db
            .get_state("port")?
            .unwrap_or_else(|| "7755".to_string());

        match mcp_client::call_daemon_tool(&port, tool, &arguments) {
            Ok(outcome) if outcome.is_error => dialog.error = Some(outcome.text),
            Ok(_) => {
                self.close_loop_form_dialog();
                self.refresh_loops()?;
            }
            Err(e) => dialog.error = Some(format!("Daemon call failed: {e:#}")),
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Database;
    use crate::domain::loops::LoopStatus;
    use crate::domain::models::WatchEvent;
    use crate::tui::mcp_client::test_support::{spawn_fake_daemon, unused_port};
    use std::sync::Arc;
    use tempfile::{tempdir, NamedTempFile};

    fn test_db() -> Arc<Database> {
        let tmp = NamedTempFile::new().expect("create temp file");
        let path = tmp.path().to_path_buf();
        std::mem::forget(tmp);
        Arc::new(Database::new(&path).expect("create test db"))
    }

    fn test_app(db: &Arc<Database>) -> (App, tempfile::TempDir) {
        let data_dir = tempdir().expect("create data dir");
        let app = App::new(Arc::clone(db), data_dir.path()).expect("create app");
        (app, data_dir)
    }

    fn sample_loop(id: &str, trigger: Option<Trigger>) -> Loop {
        Loop {
            id: id.to_string(),
            name: "Nightly review".to_string(),
            description: Some("Review the queue".to_string()),
            workdir: "/tmp".to_string(),
            status: LoopStatus::Draft,
            trigger,
            created_at: chrono::Utc::now(),
            started_at: None,
            completed_at: None,
            autorun_at: None,
        }
    }

    #[test]
    fn for_loop_prefills_a_manual_loop() {
        let dialog = LoopFormDialog::for_loop(&sample_loop("wf-1", None));

        assert_eq!(dialog.edit_id.as_deref(), Some("wf-1"));
        assert!(dialog.is_edit_mode());
        assert_eq!(dialog.name, "Nightly review");
        assert_eq!(dialog.description, "Review the queue");
        assert_eq!(dialog.workdir, "/tmp");
        assert_eq!(dialog.trigger_choice, LoopTriggerChoice::Manual);
    }

    #[test]
    fn for_loop_prefills_a_cron_loop() {
        let lp = sample_loop(
            "wf-2",
            Some(Trigger::Cron {
                schedule_expr: "30 8 * * *".to_string(),
            }),
        );

        let dialog = LoopFormDialog::for_loop(&lp);

        assert_eq!(dialog.trigger_choice, LoopTriggerChoice::Cron);
        assert_eq!(dialog.cron_expr, "30 8 * * *");
    }

    #[test]
    fn for_loop_prefills_a_watch_loop_and_preserves_hidden_fields() {
        let lp = sample_loop(
            "wf-3",
            Some(Trigger::Watch {
                path: "/tmp/watched".to_string(),
                events: vec![WatchEvent::Create, WatchEvent::Delete],
                debounce_seconds: 42,
                recursive: true,
            }),
        );

        let dialog = LoopFormDialog::for_loop(&lp);

        assert_eq!(dialog.trigger_choice, LoopTriggerChoice::Watch);
        assert_eq!(dialog.watch_path, "/tmp/watched");
        assert_eq!(
            dialog.watch_events,
            vec!["create".to_string(), "delete".to_string()]
        );
        // Not exposed in the form — must ride through to trigger_params so an
        // edit does not reset them to the server defaults.
        let params = dialog.trigger_params();
        assert_eq!(params.debounce_seconds, Some(42));
        assert_eq!(params.recursive, Some(true));
    }

    #[test]
    fn validate_reuses_daemon_checks() {
        let mut dialog = LoopFormDialog::new(Some("/tmp"));
        assert!(dialog.validate().unwrap_err().contains("Loop name"));

        dialog.name = "ok".to_string();
        dialog.workdir = "relative/dir".to_string();
        assert!(dialog.validate().unwrap_err().contains("absolute"));

        dialog.workdir = "/tmp".to_string();
        dialog.trigger_choice = LoopTriggerChoice::Cron;
        dialog.cron_expr = "not a cron".to_string();
        assert!(dialog
            .validate()
            .unwrap_err()
            .contains("Invalid cron expression"));

        dialog.cron_expr = "0 9 * * *".to_string();
        assert!(dialog.validate().is_ok());
    }

    #[test]
    fn cycle_trigger_keeps_focused_field_in_range() {
        let mut dialog = LoopFormDialog::new(Some("/tmp"));
        dialog.trigger_choice = LoopTriggerChoice::Cron;
        dialog.field = FIELD_TRIGGER_VALUE;

        // Cron → Manual drops the trigger-value field; focus must not dangle.
        dialog.cycle_trigger(false);

        assert_eq!(dialog.trigger_choice, LoopTriggerChoice::Manual);
        assert_eq!(dialog.field, FIELD_TRIGGER_KIND);
    }

    #[test]
    fn create_and_update_arguments_carry_the_form_fields() {
        let mut dialog = LoopFormDialog::for_loop(&sample_loop("wf-9", None));
        dialog.name = " Renamed ".to_string();
        dialog.trigger_choice = LoopTriggerChoice::Cron;
        dialog.cron_expr = "15 7 * * *".to_string();

        let create = dialog.create_arguments();
        assert_eq!(create["name"], "Renamed");
        assert_eq!(create["workdir"], "/tmp");
        assert_eq!(create["trigger"]["kind"], "cron");
        assert_eq!(create["trigger"]["schedule"], "15 7 * * *");
        assert!(create.get("loop_id").is_none());

        let update = dialog.update_arguments();
        assert_eq!(update["loop_id"], "wf-9");
        assert_eq!(update["trigger"]["kind"], "cron");
    }

    #[test]
    fn open_edit_loop_dialog_prefills_from_the_selected_loop() {
        let db = test_db();
        let (mut app, _dir) = test_app(&db);
        let lp = sample_loop(
            "wf-sel",
            Some(Trigger::Cron {
                schedule_expr: "0 6 * * *".to_string(),
            }),
        );
        app.loops = vec![lp];
        app.selected_loop_id = Some("wf-sel".to_string());

        app.open_edit_loop_dialog();

        let dialog = app.loop_form_dialog.as_ref().expect("dialog should open");
        assert_eq!(dialog.edit_id.as_deref(), Some("wf-sel"));
        assert_eq!(dialog.cron_expr, "0 6 * * *");
        assert!(matches!(app.focus, Focus::LoopFormDialog));
    }

    #[test]
    fn cancelling_the_loop_form_sends_no_mcp_call() {
        let fake = spawn_fake_daemon(serde_json::json!({"content": [], "isError": false}));
        let db = test_db();
        {
            use crate::application::ports::StateRepository;
            db.set_state("port", &fake.port).expect("set port");
        }
        let (mut app, _dir) = test_app(&db);

        app.open_new_loop_dialog();
        app.loop_form_dialog.as_mut().unwrap().name = "never saved".to_string();
        app.close_loop_form_dialog();

        assert!(app.loop_form_dialog.is_none());
        assert_eq!(fake.request_count(), 0);
        assert!(db.list_loops(None).unwrap().is_empty());
    }

    #[test]
    fn saving_a_new_loop_calls_loop_create_with_the_form_fields() {
        let fake = spawn_fake_daemon(serde_json::json!({
            "content": [{"type": "text", "text": "{\"loop_id\":\"new-id\"}"}],
            "isError": false
        }));
        let db = test_db();
        {
            use crate::application::ports::StateRepository;
            db.set_state("port", &fake.port).expect("set port");
        }
        let (mut app, _dir) = test_app(&db);

        app.open_new_loop_dialog();
        {
            let dialog = app.loop_form_dialog.as_mut().unwrap();
            dialog.name = "Fresh loop".to_string();
            dialog.workdir = "/tmp".to_string();
            dialog.trigger_choice = LoopTriggerChoice::Cron;
            dialog.cron_expr = "0 9 * * *".to_string();
        }
        app.save_loop_form_dialog().expect("save");

        assert!(app.loop_form_dialog.is_none(), "dialog closes on success");
        let calls = fake.recorded_calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0]["name"], "loop_create");
        assert_eq!(calls[0]["arguments"]["name"], "Fresh loop");
        assert_eq!(calls[0]["arguments"]["trigger"]["kind"], "cron");
    }

    #[test]
    fn saving_an_edited_loop_calls_loop_update_with_the_loop_id() {
        let fake = spawn_fake_daemon(serde_json::json!({
            "content": [{"type": "text", "text": "updated"}],
            "isError": false
        }));
        let db = test_db();
        {
            use crate::application::ports::StateRepository;
            db.set_state("port", &fake.port).expect("set port");
        }
        let (mut app, _dir) = test_app(&db);
        app.loops = vec![sample_loop("wf-edit", None)];
        app.selected_loop_id = Some("wf-edit".to_string());

        app.open_edit_loop_dialog();
        app.loop_form_dialog.as_mut().unwrap().name = "Renamed loop".to_string();
        app.save_loop_form_dialog().expect("save");

        assert!(app.loop_form_dialog.is_none());
        let calls = fake.recorded_calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0]["name"], "loop_update");
        assert_eq!(calls[0]["arguments"]["loop_id"], "wf-edit");
        assert_eq!(calls[0]["arguments"]["name"], "Renamed loop");
    }

    #[test]
    fn daemon_side_errors_keep_the_dialog_open_with_the_message() {
        let fake = spawn_fake_daemon(serde_json::json!({
            "content": [{"type": "text", "text": "Loop workdir must point to an existing directory."}],
            "isError": true
        }));
        let db = test_db();
        {
            use crate::application::ports::StateRepository;
            db.set_state("port", &fake.port).expect("set port");
        }
        let (mut app, _dir) = test_app(&db);

        app.open_new_loop_dialog();
        {
            let dialog = app.loop_form_dialog.as_mut().unwrap();
            dialog.name = "Broken".to_string();
            dialog.workdir = "/tmp".to_string();
        }
        app.save_loop_form_dialog().expect("save call itself is Ok");

        let dialog = app.loop_form_dialog.as_ref().expect("dialog stays open");
        assert!(dialog
            .error
            .as_deref()
            .unwrap_or_default()
            .contains("existing directory"));
    }

    #[test]
    fn unreachable_daemon_keeps_the_dialog_open_with_an_error() {
        let db = test_db();
        {
            use crate::application::ports::StateRepository;
            db.set_state("port", &unused_port()).expect("set port");
        }
        let (mut app, _dir) = test_app(&db);

        app.open_new_loop_dialog();
        {
            let dialog = app.loop_form_dialog.as_mut().unwrap();
            dialog.name = "No daemon".to_string();
            dialog.workdir = "/tmp".to_string();
        }
        app.save_loop_form_dialog().expect("save call itself is Ok");

        let dialog = app.loop_form_dialog.as_ref().expect("dialog stays open");
        assert!(dialog
            .error
            .as_deref()
            .unwrap_or_default()
            .contains("Daemon call failed"));
    }

    #[test]
    fn local_validation_errors_never_reach_the_daemon() {
        let fake = spawn_fake_daemon(serde_json::json!({"content": [], "isError": false}));
        let db = test_db();
        {
            use crate::application::ports::StateRepository;
            db.set_state("port", &fake.port).expect("set port");
        }
        let (mut app, _dir) = test_app(&db);

        app.open_new_loop_dialog();
        // Name left empty — invalid.
        app.save_loop_form_dialog().expect("save");

        let dialog = app.loop_form_dialog.as_ref().expect("dialog stays open");
        assert!(dialog.error.as_deref().unwrap_or_default().contains("name"));
        assert_eq!(fake.request_count(), 0);
    }
}
