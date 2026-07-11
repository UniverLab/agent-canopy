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

        self.agents.clear();
        // Background agents first (they are rendered at the top of the sidebar)
        for a in agents {
            self.agents.push(AgentEntry::Agent(a));
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
        if self.notifications_enabled {
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
}
