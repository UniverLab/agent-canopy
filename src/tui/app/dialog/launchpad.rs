use anyhow::Result;

use crate::db::Database;
use crate::domain::sync::summarize_sync_context;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LaunchpadChoice {
    ContinueMission,
    NewMission,
}

#[derive(Clone)]
pub struct LaunchpadContext {
    pub node_id: String,
    pub mission: String,
    pub summary: Option<String>,
}

/// A brief summary of an active mission from another agent.
#[derive(Clone)]
pub struct ActiveMissionSummary {
    pub agent_name: String,
    pub impact: String,
    pub mission: String,
}

#[derive(Clone)]
pub struct LaunchpadDialog {
    pub workdir: String,
    pub recent_missions: Vec<LaunchpadContext>,
    pub selected_index: usize,
    pub new_mission: String,
    pub cursor: usize,
    pub submit_blocked: bool,
    /// Active missions from peer agents in this workdir (mission handoff context).
    pub active_missions: Vec<ActiveMissionSummary>,
}

impl LaunchpadDialog {
    pub fn for_workdir(db: &Database, workdir: &str) -> Result<Self> {
        let nodes = db.search_intelligence_nodes(workdir, Some("session"), 50)?;
        let mut recent_missions: Vec<LaunchpadContext> = Vec::new();
        let mut seen_titles = std::collections::HashSet::new();
        let mut run_summary: Option<String> = None;

        for node in nodes {
            let metadata = node
                .metadata
                .as_deref()
                .and_then(|raw| serde_json::from_str::<serde_json::Value>(raw).ok());
            let Some(metadata) = metadata else {
                continue;
            };
            let Some(metadata_workdir) = metadata.get("workdir").and_then(|value| value.as_str())
            else {
                continue;
            };
            if metadata_workdir != workdir {
                continue;
            }
            let source = metadata
                .get("source")
                .and_then(|value| value.as_str())
                .unwrap_or_default();
            let kind = metadata
                .get("kind")
                .and_then(|value| value.as_str())
                .unwrap_or_default();

            // Nodes arrive newest-first; continuing a mission creates a new
            // node with the same title, so dedupe to keep only the latest.
            if recent_missions.len() < 5
                && (source == "launchpad" || (source == "sync" && kind == "intent"))
                && seen_titles.insert(node.title.trim().to_lowercase())
            {
                recent_missions.push(LaunchpadContext {
                    node_id: node.id.clone(),
                    mission: node.title.clone(),
                    summary: metadata
                        .get("summary")
                        .and_then(|value| value.as_str())
                        .map(str::to_owned),
                });
            }

            if run_summary.is_none() && source == "run" {
                run_summary = metadata
                    .get("summary")
                    .and_then(|value| value.as_str())
                    .map(str::to_owned);
            }
        }

        if let Some(ctx) = recent_missions.first_mut() {
            if ctx.summary.is_none() {
                ctx.summary = run_summary;
            }
        }

        let selected_index = 0;

        let active_missions = db
            .list_sync_messages(workdir, 30)
            .ok()
            .map(|messages| {
                let agent_ids = messages
                    .iter()
                    .map(|m| m.agent_id.clone())
                    .collect::<std::collections::HashSet<_>>();
                let snapshot = summarize_sync_context(&messages, &agent_ids, 5);
                snapshot
                    .active_intents
                    .into_iter()
                    .map(|intent| ActiveMissionSummary {
                        agent_name: intent.agent_name,
                        impact: intent.impact.as_str().to_owned(),
                        mission: intent.mission,
                    })
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();

        Ok(Self {
            workdir: workdir.to_owned(),
            recent_missions,
            selected_index,
            new_mission: String::new(),
            cursor: 0,
            submit_blocked: false,
            active_missions,
        })
    }

    pub fn total_items(&self) -> usize {
        self.recent_missions.len() + 1
    }

    /// "New mission" is the first option (index 0) and the default selection.
    pub fn is_new_mission_selected(&self) -> bool {
        self.selected_index == 0
    }

    pub fn selected_mission(&self) -> Option<&LaunchpadContext> {
        self.recent_missions
            .get(self.selected_index.checked_sub(1)?)
    }

    pub fn choice(&self) -> LaunchpadChoice {
        if self.is_new_mission_selected() {
            LaunchpadChoice::NewMission
        } else {
            LaunchpadChoice::ContinueMission
        }
    }

    pub fn move_selection_up(&mut self) {
        if self.selected_index > 0 {
            self.selected_index -= 1;
        }
        self.submit_blocked = false;
    }

    pub fn move_selection_down(&mut self) {
        if self.selected_index < self.total_items() - 1 {
            self.selected_index += 1;
        }
        self.submit_blocked = false;
    }

    pub fn new_mission_title(&self) -> Option<&str> {
        let mission = self.new_mission.trim();
        (!mission.is_empty()).then_some(mission)
    }

    pub fn can_confirm_selection(&self) -> bool {
        if self.is_new_mission_selected() {
            self.new_mission_title().is_some()
        } else {
            self.selected_mission().is_some()
        }
    }

    pub fn validation_message(&self) -> Option<&'static str> {
        if self.is_new_mission_selected() && self.new_mission_title().is_none() {
            Some("Type a mission name above.")
        } else {
            None
        }
    }

    pub fn mark_submit_blocked(&mut self) {
        self.submit_blocked = true;
    }

    pub fn clear_submit_blocked(&mut self) {
        self.submit_blocked = false;
    }

    pub fn insert_char(&mut self, c: char) {
        if !self.is_new_mission_selected() {
            return;
        }
        self.new_mission.insert(self.cursor, c);
        self.cursor += c.len_utf8();
        self.clear_submit_blocked();
    }

    pub fn backspace(&mut self) {
        if !self.is_new_mission_selected() || self.cursor == 0 {
            return;
        }
        let prev = self
            .new_mission
            .char_indices()
            .take_while(|(idx, _)| *idx < self.cursor)
            .last()
            .map(|(idx, _)| idx)
            .unwrap_or(0);
        self.new_mission.drain(prev..self.cursor);
        self.cursor = prev;
        self.clear_submit_blocked();
    }

    pub fn delete(&mut self) {
        if !self.is_new_mission_selected() || self.cursor >= self.new_mission.len() {
            return;
        }
        let next = self
            .new_mission
            .char_indices()
            .find(|(idx, _)| *idx > self.cursor)
            .map(|(idx, _)| idx)
            .unwrap_or(self.new_mission.len());
        self.new_mission.drain(self.cursor..next);
        self.clear_submit_blocked();
    }

    pub fn move_cursor_left(&mut self) {
        if !self.is_new_mission_selected() || self.cursor == 0 {
            return;
        }
        self.cursor = self
            .new_mission
            .char_indices()
            .take_while(|(idx, _)| *idx < self.cursor)
            .last()
            .map(|(idx, _)| idx)
            .unwrap_or(0);
    }

    pub fn move_cursor_right(&mut self) {
        if !self.is_new_mission_selected() || self.cursor >= self.new_mission.len() {
            return;
        }
        self.cursor = self
            .new_mission
            .char_indices()
            .find(|(idx, _)| *idx > self.cursor)
            .map(|(idx, _)| idx)
            .unwrap_or(self.new_mission.len());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn can_confirm_selection_requires_explicit_new_mission_text() {
        let mut dialog = LaunchpadDialog {
            workdir: "/tmp/project".to_string(),
            recent_missions: Vec::new(),
            selected_index: 0,
            new_mission: "   ".to_string(),
            cursor: 3,
            submit_blocked: false,
            active_missions: Vec::new(),
        };

        assert!(!dialog.can_confirm_selection());
        assert_eq!(
            dialog.validation_message(),
            Some("Type a mission name above.")
        );

        dialog.new_mission = "Refactor sync panel".to_string();
        dialog.cursor = dialog.new_mission.len();

        assert!(dialog.can_confirm_selection());
        assert_eq!(dialog.new_mission_title(), Some("Refactor sync panel"));
    }

    #[test]
    fn navigation_respects_bounds() {
        let mut dialog = LaunchpadDialog {
            workdir: "/tmp/project".to_string(),
            recent_missions: vec![
                LaunchpadContext {
                    node_id: "1".into(),
                    mission: "Mission 1".into(),
                    summary: None,
                },
                LaunchpadContext {
                    node_id: "2".into(),
                    mission: "Mission 2".into(),
                    summary: None,
                },
            ],
            selected_index: 0,
            new_mission: String::new(),
            cursor: 0,
            submit_blocked: false,
            active_missions: Vec::new(),
        };

        dialog.move_selection_up();
        assert_eq!(dialog.selected_index, 0);

        dialog.move_selection_down();
        assert_eq!(dialog.selected_index, 1);

        dialog.move_selection_down();
        assert_eq!(dialog.selected_index, 2);

        dialog.move_selection_down();
        assert_eq!(dialog.selected_index, 2);
    }

    #[test]
    fn select_existing_mission() {
        let dialog = LaunchpadDialog {
            workdir: "/tmp/project".to_string(),
            recent_missions: vec![LaunchpadContext {
                node_id: "1".into(),
                mission: "Fix bug".into(),
                summary: None,
            }],
            selected_index: 1,
            new_mission: String::new(),
            cursor: 0,
            submit_blocked: false,
            active_missions: Vec::new(),
        };

        assert!(dialog.can_confirm_selection());
        assert_eq!(dialog.choice(), LaunchpadChoice::ContinueMission);
        assert_eq!(dialog.selected_mission().unwrap().mission, "Fix bug");
    }

    #[test]
    fn new_mission_is_first_and_default() {
        let dialog = LaunchpadDialog {
            workdir: "/tmp/project".to_string(),
            recent_missions: vec![LaunchpadContext {
                node_id: "1".into(),
                mission: "Fix bug".into(),
                summary: None,
            }],
            selected_index: 0,
            new_mission: String::new(),
            cursor: 0,
            submit_blocked: false,
            active_missions: Vec::new(),
        };

        assert!(dialog.is_new_mission_selected());
        assert!(dialog.selected_mission().is_none());
        assert_eq!(dialog.choice(), LaunchpadChoice::NewMission);
    }
}
