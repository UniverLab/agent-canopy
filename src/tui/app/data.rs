use anyhow::Result;

use crate::application::ports::{AgentRepository, RunRepository, StateRepository};

use super::types::{AgentEntry, App};
use super::utils::{is_local_port_open, is_process_running, relative_time, tail_lines};

impl App {
    pub(super) fn refresh_daemon_status(&mut self) {
        let pid_path = self.data_dir.join("daemon.pid");
        self.daemon_pid = std::fs::read_to_string(&pid_path)
            .ok()
            .and_then(|s| s.trim().parse().ok());
        let daemon_running_by_pid = self.daemon_pid.map(is_process_running).unwrap_or(false);
        self.daemon_version = self
            .db
            .get_state("version")
            .ok()
            .flatten()
            .unwrap_or_default();
        let daemon_port = self
            .db
            .get_state("port")
            .ok()
            .flatten()
            .and_then(|v| v.parse::<u16>().ok())
            .unwrap_or(7755);
        let daemon_running_by_port = is_local_port_open(daemon_port);
        self.daemon_running = daemon_running_by_pid || daemon_running_by_port;
    }

    pub(super) fn refresh_agents(&mut self) -> Result<()> {
        let agents = self.db.list_agents()?;
        // Corrupt rows (e.g. malformed trigger_config) never fail this
        // refresh — they're shown as degraded cards instead.
        let corrupt = self.db.list_corrupt_agents().unwrap_or_default();

        self.agents.clear();
        // Background agents first (they are rendered at the top of the sidebar)
        for a in agents {
            self.agents.push(AgentEntry::Agent(a));
        }
        for c in corrupt {
            self.agents.push(AgentEntry::Corrupt(c));
        }
        // Interactive sessions
        for i in 0..self.interactive_agents.len() {
            self.agents.push(AgentEntry::Interactive(i));
        }
        // Then terminals
        for i in 0..self.terminal_agents.len() {
            self.agents.push(AgentEntry::Terminal(i));
        }
        // Orphaned sessions (can be revived or dismissed)
        for i in 0..self.orphaned_sessions.len() {
            self.agents.push(AgentEntry::Orphaned(i));
        }
        // Then split groups
        for i in 0..self.split_groups.len() {
            self.agents.push(AgentEntry::Group(i));
        }

        let total = self.agents.len();
        if total > 0 && self.selected >= total {
            self.selected = total - 1;
        }

        Ok(())
    }

    pub(super) fn refresh_active_runs(&mut self) -> Result<()> {
        let prev_ids = std::mem::take(&mut self.prev_active_run_ids);

        self.active_runs.clear();
        for agent in &self.agents {
            let id = agent.id(self);
            if let Ok(Some(run)) = self.db.get_active_run(id) {
                self.active_runs.insert(id.to_string(), run);
            }
        }

        // Detect background task completions: was active last tick, gone now.
        //
        // The daemon's executor (`notify_result`) is the single owner of
        // desktop notifications for background-agent runs — when it's alive it
        // fires the completion toast, so the TUI must stay silent to avoid a
        // duplicate toast for the same run. This TUI-side sender exists only as
        // a fallback for a daemonless TUI (no daemon process detected), so it
        // still surfaces completions when nothing else would.
        if self.notifications_enabled && !self.daemon_running {
            for finished_id in &prev_ids {
                if !self.active_runs.contains_key(finished_id.as_str()) {
                    if self.db.get_agent(finished_id).ok().flatten().is_none() {
                        continue;
                    }
                    if let Some(run) = self
                        .db
                        .list_runs(finished_id, 1)
                        .ok()
                        .and_then(|mut runs| runs.drain(..).next())
                    {
                        let success =
                            matches!(run.status, crate::domain::models::RunStatus::Success);
                        self.notification_service.notify_task_completed(
                            finished_id,
                            success,
                            run.exit_code,
                        );
                    }
                }
            }
        }
        self.prev_active_run_ids = self.active_runs.keys().cloned().collect();

        self.recent_runs = self.db.list_all_recent_runs(50)?;
        Ok(())
    }

    pub(super) fn refresh_log(&mut self) {
        let Some(agent) = self.agents.get(self.selected) else {
            self.log_content = String::new();
            return;
        };

        match agent {
            AgentEntry::Interactive(idx) => {
                if *idx >= self.interactive_agents.len() {
                    self.log_content = String::from("Agent removed");
                    return;
                }
                let output = self.interactive_agents[*idx].output();
                self.log_content = if output.is_empty() {
                    format!(
                        "Agent '{}' — waiting for output...",
                        self.interactive_agents[*idx].id
                    )
                } else {
                    output
                };
            }
            AgentEntry::Terminal(idx) => {
                if *idx >= self.terminal_agents.len() {
                    self.log_content = String::from("Terminal removed");
                    return;
                }
                let output = self.terminal_agents[*idx].output();
                self.log_content = if output.is_empty() {
                    format!(
                        "Terminal '{}' — waiting for output...",
                        self.terminal_agents[*idx].name
                    )
                } else {
                    output
                };
            }
            AgentEntry::Group(idx) => {
                if let Some(group) = self.split_groups.get(*idx) {
                    self.log_content = format!(
                        "Split Group: {}\n{} · {}\nOrientation: {}",
                        group.id,
                        group.session_a,
                        group.session_b,
                        group.orientation.as_str(),
                    );
                }
            }
            _ => {
                let id = agent.id(self).to_string();
                let log_path = self.data_dir.join("logs").join(format!("{id}.log"));

                let mut content = match std::fs::read_to_string(&log_path) {
                    Ok(c) => tail_lines(&c, 200),
                    Err(_) => String::new(),
                };

                if let Some(run) = self.active_runs.get(&id) {
                    let header = format!(
                        "⏳ Run {} in progress ({})\n{}\n",
                        &run.id[..8.min(run.id.len())],
                        relative_time(&run.started_at),
                        "─".repeat(40),
                    );
                    content = if content.is_empty() {
                        format!("{header}Waiting for output...")
                    } else {
                        format!("{header}{content}")
                    };
                } else if content.is_empty() {
                    content = format!("No logs yet for '{id}'");
                }

                self.log_content = content;
            }
        }
    }

    /// Poll for due scheduled sends and deliver them to the target interactive
    /// session. If the target session is dead/missing, send a desktop
    /// notification and keep the prompt recoverable via the last-prompt recall.
    pub(super) fn deliver_due_scheduled_sends(&mut self) {
        // Hold delivery until the startup restore has run: the first refresh
        // happens before sessions auto-resume, so acting now would see no live
        // sessions and wrongly declare every due schedule dead.
        if !self.scheduled_sends_restored {
            return;
        }
        let now = chrono::Utc::now();
        let due = match self.db.list_due_scheduled_sends(now) {
            Ok(due) => due,
            Err(e) => {
                tracing::warn!("Failed to list due scheduled sends: {e}");
                return;
            }
        };

        let live_session_ids: Vec<String> = self
            .interactive_agents
            .iter()
            .map(|a| a.id.clone())
            .collect();

        for send in &due {
            if crate::db::scheduled_sends::is_target_alive(
                &send.target_session_id,
                &live_session_ids,
            ) {
                // Find the target interactive agent by session ID.
                let Some(agent) = self
                    .interactive_agents
                    .iter()
                    .find(|a| a.id == send.target_session_id)
                else {
                    continue;
                };
                // Deliver the prompt using the same path as a manual send.
                let spec = agent.cli.paste_submit_spec();
                if let Err(e) = agent.submit_prompt_to_pty(&send.prompt, spec) {
                    tracing::warn!("Scheduled send '{}' delivery failed: {e}", send.id);
                    crate::domain::notification::send_notification(
                        "Scheduled send failed",
                        &format!("Could not deliver prompt: {e}"),
                        crate::domain::notification::NotificationLevel::Error,
                    );
                } else {
                    tracing::info!(
                        "Scheduled send '{}' delivered to session '{}'",
                        send.id,
                        send.target_session_id
                    );
                    // A hook-originated message stays identifiable as such:
                    // the delivered prompt is exactly the promptbuilder text,
                    // and the loop/event that sent it is surfaced here rather
                    // than being buried in that text.
                    if let Some(provenance) = send.provenance.as_ref() {
                        if provenance.kind == "hook" {
                            crate::domain::notification::send_notification(
                                "Hook message delivered",
                                &format!(
                                    "Loop '{}' sent a message to this session (event '{}').",
                                    provenance.loop_id, provenance.event
                                ),
                                crate::domain::notification::NotificationLevel::Info,
                            );
                        }
                    }
                }
            } else {
                // Target session doesn't exist — notify and preserve the prompt
                // so it stays reachable via the project's last-prompt recall.
                tracing::warn!(
                    "Scheduled send '{}': target session '{}' not found; \
                     prompt preserved for recall",
                    send.id,
                    send.target_session_id
                );
                if let Err(e) = self.db.insert_failed_scheduled_send(
                    &send.id,
                    &send.prompt,
                    &send.target_session_id,
                    send.workdir.as_deref(),
                    now,
                    send.provenance.as_ref(),
                ) {
                    tracing::warn!(
                        "Failed to preserve failed scheduled send '{}': {e}",
                        send.id
                    );
                }
                // Also surface it as the project's last prompt so Ctrl+L in
                // the prompt builder recovers it (U8). Only the flattened
                // text is available here — the builder that composed it is
                // long gone by the time this fires.
                if let Some(workdir) = send.workdir.as_deref() {
                    let id = format!("lp-{}", uuid::Uuid::new_v4());
                    if let Err(e) =
                        self.db
                            .insert_last_prompt(&id, workdir, &send.prompt, None, now)
                    {
                        tracing::warn!(
                            "Failed to record last prompt for failed send '{}': {e}",
                            send.id
                        );
                    }
                }
                crate::domain::notification::send_notification(
                    "Scheduled send: session not found",
                    &format!(
                        "Target session '{}' is no longer active. The prompt is preserved for recall.",
                        send.target_session_id
                    ),
                    crate::domain::notification::NotificationLevel::Warning,
                );
            }

            // Remove the scheduled send after delivery attempt (whether
            // successful or not — the prompt is either delivered or preserved).
            if let Err(e) = self.db.delete_scheduled_send(&send.id) {
                tracing::warn!("Failed to delete scheduled send '{}': {e}", send.id);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::App;
    use crate::db::Database;
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

    /// U7's dead-target path (no live interactive session matches the
    /// scheduled send's target) must not just preserve the prompt in
    /// `failed_scheduled_sends` — it must also surface it as the project's
    /// last prompt so Ctrl+L (U8) can recover it, since the builder that
    /// composed it is long gone by delivery time.
    #[test]
    fn dead_target_scheduled_send_becomes_the_projects_last_prompt() {
        let (mut app, _dir) = test_app();
        // Simulate a running (post-restore) app whose target session died mid
        // run: the delivery gate is open and the schedule survived restore.
        app.scheduled_sends_restored = true;
        let workdir = "/home/user/dead-target-project";
        let fire_at = chrono::Utc::now() - chrono::Duration::minutes(1);

        app.db
            .insert_scheduled_send(
                "ss-dead-1",
                "please recover me",
                "session-that-no-longer-exists",
                Some(workdir),
                fire_at,
                None,
                None,
            )
            .expect("insert scheduled send");

        // No interactive agents registered, so the target cannot be alive.
        assert!(app.interactive_agents.is_empty());

        app.deliver_due_scheduled_sends();

        let last = app
            .db
            .get_last_prompt_for_workdir(workdir)
            .expect("query last prompt")
            .expect("last prompt recorded for the dead-target workdir");
        assert_eq!(last.prompt_text, "please recover me");
        // Only the flattened text survives a dead-target recovery — the
        // builder that composed it no longer exists at delivery time.
        assert!(last.builder_state.is_none());

        // The scheduled send itself must not remain (it fired once, whether
        // delivered or preserved) and it should also be visible in the
        // failed-sends table for the workdir.
        assert!(app
            .db
            .list_due_scheduled_sends(chrono::Utc::now())
            .expect("list due")
            .is_empty());
        let failed = app
            .db
            .list_failed_scheduled_sends_for_workdir(workdir)
            .expect("list failed");
        assert_eq!(failed.len(), 1);
        assert_eq!(failed[0].prompt, "please recover me");
    }

    /// Before the startup restore runs, the delivery gate is closed: a due
    /// schedule must be left completely untouched (not delivered, not
    /// preserved) so a session that is still resuming isn't wrongly declared
    /// dead during the pre-resume window.
    #[test]
    fn delivery_is_held_until_scheduled_sends_are_restored() {
        let (mut app, _dir) = test_app();
        assert!(!app.scheduled_sends_restored);
        let fire_at = chrono::Utc::now() - chrono::Duration::minutes(5);
        app.db
            .insert_scheduled_send(
                "ss-held",
                "later",
                "resuming-session",
                None,
                fire_at,
                None,
                None,
            )
            .expect("insert scheduled send");

        app.deliver_due_scheduled_sends();

        // Still pending, and NOT shunted into the failed table.
        assert_eq!(
            app.db
                .list_due_scheduled_sends(chrono::Utc::now())
                .expect("list due")
                .len(),
            1
        );
        assert!(app
            .db
            .list_failed_scheduled_sends_for_workdir("resuming-session")
            .expect("list failed")
            .is_empty());
    }

    /// A due hook send is held like any other send before the startup
    /// restore runs: with no TUI agent present it stays pending (not
    /// delivered, not failed), keeping its provenance for the delivery that
    /// happens once a TUI comes up.
    #[test]
    fn hook_send_is_held_pending_without_tui() {
        let (mut app, _dir) = test_app();
        assert!(!app.scheduled_sends_restored);
        let fire_at = chrono::Utc::now() - chrono::Duration::minutes(5);
        let provenance =
            crate::db::scheduled_sends::ScheduledSendProvenance::hook("loop-hook", "on_failed");
        app.db
            .insert_scheduled_send(
                "ss-hook-held",
                "Loop Loop failed",
                "operator-session",
                None,
                fire_at,
                None,
                Some(&provenance),
            )
            .expect("insert scheduled send");

        app.deliver_due_scheduled_sends();

        let due = app
            .db
            .list_due_scheduled_sends(chrono::Utc::now())
            .expect("list due");
        assert_eq!(due.len(), 1);
        assert_eq!(due[0].provenance, Some(provenance));
    }

    /// On startup, a pending schedule whose target session was not resumed
    /// (it no longer exists) is dropped silently — no failed record, no
    /// last-prompt recall — and the delivery gate opens.
    #[test]
    fn restore_drops_schedules_for_sessions_that_no_longer_exist() {
        let (mut app, _dir) = test_app();
        let workdir = "/home/user/gone-project";
        let fire_at = chrono::Utc::now() - chrono::Duration::minutes(1);
        app.db
            .insert_scheduled_send(
                "ss-gone",
                "orphan",
                "gone-session",
                Some(workdir),
                fire_at,
                None,
                None,
            )
            .expect("insert scheduled send");
        // No sessions were resumed.
        assert!(app.interactive_agents.is_empty());

        app.restore_scheduled_sends();

        assert!(app.scheduled_sends_restored);
        // Dropped silently: gone from the schedule, and never preserved.
        assert!(app
            .db
            .list_due_scheduled_sends(chrono::Utc::now())
            .expect("list due")
            .is_empty());
        assert!(app
            .db
            .list_failed_scheduled_sends_for_workdir(workdir)
            .expect("list failed")
            .is_empty());
        assert!(app
            .db
            .get_last_prompt_for_workdir(workdir)
            .expect("query last prompt")
            .is_none());
    }
}
