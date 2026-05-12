//! Collaborative sync domain models and advisory context derivation.

use std::collections::{HashMap, HashSet};

use serde::{Deserialize, Serialize};

/// Kind of message in the sync channel.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MessageKind {
    Intent,
    Status,
    Query,
    Answer,
    Info,
}

impl MessageKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Intent => "intent",
            Self::Status => "status",
            Self::Query => "query",
            Self::Answer => "answer",
            Self::Info => "info",
        }
    }

    pub fn from_str(s: &str) -> Option<Self> {
        match s {
            "intent" => Some(Self::Intent),
            "status" => Some(Self::Status),
            "query" => Some(Self::Query),
            "answer" => Some(Self::Answer),
            "info" => Some(Self::Info),
            _ => None,
        }
    }

    pub fn is_chatter(self) -> bool {
        matches!(self, Self::Query | Self::Answer | Self::Info)
    }
}

/// Impact level of an active mission.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MissionImpact {
    Low,
    High,
    Breaking,
}

impl MissionImpact {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Low => "low",
            Self::High => "high",
            Self::Breaking => "breaking",
        }
    }

    pub fn from_str(s: &str) -> Option<Self> {
        match s {
            "low" => Some(Self::Low),
            "high" => Some(Self::High),
            "breaking" => Some(Self::Breaking),
            _ => None,
        }
    }
}

/// Status reported by an agent for the shared workspace.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkspaceStatus {
    Stable,
    Unstable,
    Testing,
}

impl WorkspaceStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Stable => "stable",
            Self::Unstable => "unstable",
            Self::Testing => "testing",
        }
    }

    pub fn from_str(s: &str) -> Option<Self> {
        match s {
            "stable" => Some(Self::Stable),
            "unstable" => Some(Self::Unstable),
            "testing" => Some(Self::Testing),
            _ => None,
        }
    }
}

/// A message in the per-workdir sync channel.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SyncMessage {
    pub id: i64,
    pub workdir: String,
    pub agent_id: String,
    pub agent_name: String,
    pub kind: MessageKind,
    pub message: String,
    /// Optional JSON payload with structured sync metadata.
    pub payload: Option<String>,
    pub created_at: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IntentPayload {
    pub mission: String,
    pub impact: MissionImpact,
    pub description: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StatusPayload {
    pub status: WorkspaceStatus,
    pub message: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ActiveIntent {
    pub agent_id: String,
    pub agent_name: String,
    pub mission: String,
    pub impact: MissionImpact,
    pub description: String,
    pub status: WorkspaceStatus,
    pub since: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SyncContextSnapshot {
    pub active_intents: Vec<ActiveIntent>,
    pub recent_chatter: Vec<SyncMessage>,
    pub vibe: WorkspaceStatus,
}

#[derive(Debug, Clone)]
struct IntentState {
    agent_id: String,
    agent_name: String,
    mission: String,
    impact: MissionImpact,
    description: String,
    since: i64,
}

pub fn summarize_sync_context(
    messages: &[SyncMessage],
    active_agent_ids: &HashSet<String>,
    chatter_limit: usize,
) -> SyncContextSnapshot {
    let mut intents_by_agent: HashMap<&str, IntentState> = HashMap::new();
    let mut statuses_by_agent: HashMap<&str, WorkspaceStatus> = HashMap::new();
    // Track the timestamp of the last explicit mission-closed marker per agent.
    let mut closed_at_by_agent: HashMap<&str, i64> = HashMap::new();

    for message in messages {
        if !active_agent_ids.contains(&message.agent_id) {
            continue;
        }

        match message.kind {
            MessageKind::Intent => {
                let Some(payload) = parse_intent_payload(message) else {
                    continue;
                };
                intents_by_agent.insert(
                    &message.agent_id,
                    IntentState {
                        agent_id: message.agent_id.clone(),
                        agent_name: message.agent_name.clone(),
                        mission: payload.mission,
                        impact: payload.impact,
                        description: payload.description,
                        since: message.created_at,
                    },
                );
            }
            MessageKind::Status => {
                let Some(payload) = parse_status_payload(message) else {
                    continue;
                };
                statuses_by_agent.insert(&message.agent_id, payload.status);
            }
            MessageKind::Info => {
                if is_mission_closed(message) {
                    closed_at_by_agent.insert(&message.agent_id, message.created_at);
                }
            }
            MessageKind::Query | MessageKind::Answer => {}
        }
    }

    // Remove intents for agents whose close marker arrived after their last intent.
    intents_by_agent.retain(|agent_id, intent| {
        closed_at_by_agent
            .get(agent_id)
            .map_or(true, |&closed_at| closed_at < intent.since)
    });

    let mut active_intents: Vec<ActiveIntent> = intents_by_agent
        .into_values()
        .map(|intent| ActiveIntent {
            status: statuses_by_agent
                .get(intent.agent_id.as_str())
                .copied()
                .unwrap_or_else(|| default_status_for_impact(intent.impact)),
            agent_id: intent.agent_id,
            agent_name: intent.agent_name,
            mission: intent.mission,
            impact: intent.impact,
            description: intent.description,
            since: intent.since,
        })
        .collect();

    active_intents.sort_by_key(|intent| intent.since);

    let recent_chatter: Vec<SyncMessage> = messages
        .iter()
        .filter(|message| message.kind.is_chatter())
        .rev()
        .take(chatter_limit)
        .cloned()
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();

    let vibe = if active_intents
        .iter()
        .any(|intent| intent.status == WorkspaceStatus::Unstable)
    {
        WorkspaceStatus::Unstable
    } else if active_intents
        .iter()
        .any(|intent| intent.status == WorkspaceStatus::Testing)
    {
        WorkspaceStatus::Testing
    } else {
        WorkspaceStatus::Stable
    };

    SyncContextSnapshot {
        active_intents,
        recent_chatter,
        vibe,
    }
}

pub fn parse_intent_payload(message: &SyncMessage) -> Option<IntentPayload> {
    serde_json::from_str(message.payload.as_deref()?).ok()
}

pub fn parse_status_payload(message: &SyncMessage) -> Option<StatusPayload> {
    serde_json::from_str(message.payload.as_deref()?).ok()
}

/// Returns true if the message is an explicit mission-closed marker inserted by
/// the daemon when an agent exits (payload contains `"mission_closed": true`).
fn is_mission_closed(message: &SyncMessage) -> bool {
    message
        .payload
        .as_deref()
        .and_then(|p| serde_json::from_str::<serde_json::Value>(p).ok())
        .and_then(|v| v.get("mission_closed").and_then(|b| b.as_bool()))
        .unwrap_or(false)
}

fn default_status_for_impact(impact: MissionImpact) -> WorkspaceStatus {
    match impact {
        MissionImpact::Breaking => WorkspaceStatus::Unstable,
        MissionImpact::Low | MissionImpact::High => WorkspaceStatus::Stable,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sync_message(
        id: i64,
        agent_id: &str,
        agent_name: &str,
        kind: MessageKind,
        payload: Option<String>,
        created_at: i64,
    ) -> SyncMessage {
        SyncMessage {
            id,
            workdir: "/repo".into(),
            agent_id: agent_id.into(),
            agent_name: agent_name.into(),
            kind,
            message: "test".into(),
            payload,
            created_at,
        }
    }

    #[test]
    fn summarize_sync_context_tracks_active_intents_and_vibe() {
        let intent_payload = serde_json::to_string(&IntentPayload {
            mission: "Refactor auth".into(),
            impact: MissionImpact::Breaking,
            description: "touching login flow".into(),
        })
        .unwrap();
        let status_payload = serde_json::to_string(&StatusPayload {
            status: WorkspaceStatus::Testing,
            message: "running smoke tests".into(),
        })
        .unwrap();

        let messages = vec![
            sync_message(
                1,
                "agent-a",
                "copilot",
                MessageKind::Intent,
                Some(intent_payload),
                10,
            ),
            sync_message(
                2,
                "agent-a",
                "copilot",
                MessageKind::Status,
                Some(status_payload),
                11,
            ),
        ];

        let active = HashSet::from([String::from("agent-a")]);
        let summary = summarize_sync_context(&messages, &active, 10);

        assert_eq!(summary.active_intents.len(), 1);
        assert_eq!(summary.active_intents[0].mission, "Refactor auth");
        assert_eq!(summary.active_intents[0].status, WorkspaceStatus::Testing);
        assert_eq!(summary.vibe, WorkspaceStatus::Testing);
    }

    #[test]
    fn summarize_sync_context_ignores_inactive_agents() {
        let payload = serde_json::to_string(&IntentPayload {
            mission: "Refactor auth".into(),
            impact: MissionImpact::High,
            description: "touching login flow".into(),
        })
        .unwrap();
        let messages = vec![sync_message(
            1,
            "agent-a",
            "copilot",
            MessageKind::Intent,
            Some(payload),
            10,
        )];

        let summary = summarize_sync_context(&messages, &HashSet::new(), 10);

        assert!(summary.active_intents.is_empty());
        assert_eq!(summary.vibe, WorkspaceStatus::Stable);
    }

    #[test]
    fn summarize_sync_context_uses_default_status_for_breaking_missions() {
        let payload = serde_json::to_string(&IntentPayload {
            mission: "Schema rewrite".into(),
            impact: MissionImpact::Breaking,
            description: "migrating tables".into(),
        })
        .unwrap();
        let messages = vec![sync_message(
            1,
            "agent-a",
            "copilot",
            MessageKind::Intent,
            Some(payload),
            10,
        )];

        let active = HashSet::from([String::from("agent-a")]);
        let summary = summarize_sync_context(&messages, &active, 10);

        assert_eq!(summary.active_intents[0].status, WorkspaceStatus::Unstable);
        assert_eq!(summary.vibe, WorkspaceStatus::Unstable);
    }

    #[test]
    fn summarize_sync_context_limits_recent_chatter() {
        let messages = vec![
            sync_message(1, "agent-a", "copilot", MessageKind::Info, None, 10),
            sync_message(2, "agent-b", "claude", MessageKind::Query, None, 11),
            sync_message(3, "agent-c", "gemini", MessageKind::Answer, None, 12),
        ];

        let summary = summarize_sync_context(&messages, &HashSet::new(), 2);

        assert_eq!(summary.recent_chatter.len(), 2);
        assert_eq!(summary.recent_chatter[0].id, 2);
        assert_eq!(summary.recent_chatter[1].id, 3);
    }
}
