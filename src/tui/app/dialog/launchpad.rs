use anyhow::Result;

use crate::db::Database;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum LaunchpadChoice {
    ContinuePrevious,
    NewMission,
}

#[derive(Clone)]
pub struct LaunchpadContext {
    pub node_id: String,
    pub mission: String,
    pub summary: Option<String>,
}

#[derive(Clone)]
pub struct LaunchpadDialog {
    pub workdir: String,
    pub previous: Option<LaunchpadContext>,
    pub selected: LaunchpadChoice,
    pub new_mission: String,
    pub cursor: usize,
}

impl LaunchpadDialog {
    pub fn for_workdir(db: &Database, workdir: &str) -> Result<Self> {
        let nodes = db.search_intelligence_nodes(workdir, Some("session"), 50)?;
        let mut previous: Option<LaunchpadContext> = None;
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

            if previous.is_none() && source == "launchpad" {
                previous = Some(LaunchpadContext {
                    node_id: node.id.clone(),
                    mission: node.title.clone(),
                    summary: metadata
                        .get("summary")
                        .and_then(|value| value.as_str())
                        .map(str::to_owned),
                });
            }

            if previous.is_none()
                && source == "sync"
                && metadata.get("kind").and_then(|value| value.as_str()) == Some("intent")
            {
                previous = Some(LaunchpadContext {
                    node_id: node.id.clone(),
                    mission: node.title.clone(),
                    summary: None,
                });
            }

            if run_summary.is_none() && source == "run" {
                run_summary = metadata
                    .get("summary")
                    .and_then(|value| value.as_str())
                    .map(str::to_owned);
            }
        }

        if let Some(context) = previous.as_mut() {
            if context.summary.is_none() {
                context.summary = run_summary;
            }
        }

        let selected = if previous.is_some() {
            LaunchpadChoice::ContinuePrevious
        } else {
            LaunchpadChoice::NewMission
        };

        Ok(Self {
            workdir: workdir.to_owned(),
            previous,
            selected,
            new_mission: String::new(),
            cursor: 0,
        })
    }

    pub fn has_previous(&self) -> bool {
        self.previous.is_some()
    }

    pub fn toggle_choice(&mut self) {
        self.selected = match self.selected {
            LaunchpadChoice::ContinuePrevious if self.has_previous() => LaunchpadChoice::NewMission,
            LaunchpadChoice::ContinuePrevious => LaunchpadChoice::NewMission,
            LaunchpadChoice::NewMission if self.has_previous() => LaunchpadChoice::ContinuePrevious,
            LaunchpadChoice::NewMission => LaunchpadChoice::NewMission,
        };
    }

    pub fn insert_char(&mut self, c: char) {
        if self.selected != LaunchpadChoice::NewMission {
            return;
        }
        self.new_mission.insert(self.cursor, c);
        self.cursor += c.len_utf8();
    }

    pub fn backspace(&mut self) {
        if self.selected != LaunchpadChoice::NewMission || self.cursor == 0 {
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
    }

    pub fn delete(&mut self) {
        if self.selected != LaunchpadChoice::NewMission || self.cursor >= self.new_mission.len() {
            return;
        }
        let next = self
            .new_mission
            .char_indices()
            .find(|(idx, _)| *idx > self.cursor)
            .map(|(idx, _)| idx)
            .unwrap_or(self.new_mission.len());
        self.new_mission.drain(self.cursor..next);
    }

    pub fn move_cursor_left(&mut self) {
        if self.selected != LaunchpadChoice::NewMission || self.cursor == 0 {
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
        if self.selected != LaunchpadChoice::NewMission || self.cursor >= self.new_mission.len() {
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
