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
            .is_none_or(|&closed_at| closed_at < intent.since)
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

    #[test]
    fn message_kind_as_str_roundtrip() {
        assert_eq!(MessageKind::Info.as_str(), "info");
        assert_eq!(MessageKind::Query.as_str(), "query");
        assert_eq!(MessageKind::Answer.as_str(), "answer");
        assert_eq!(MessageKind::Intent.as_str(), "intent");
        assert_eq!(MessageKind::Status.as_str(), "status");
    }

    #[test]
    fn message_kind_from_str_valid() {
        assert_eq!(MessageKind::from_str("info"), Some(MessageKind::Info));
        assert_eq!(MessageKind::from_str("query"), Some(MessageKind::Query));
        assert_eq!(MessageKind::from_str("answer"), Some(MessageKind::Answer));
        assert_eq!(MessageKind::from_str("intent"), Some(MessageKind::Intent));
        assert_eq!(MessageKind::from_str("status"), Some(MessageKind::Status));
    }

    #[test]
    fn message_kind_from_str_invalid() {
        assert!(MessageKind::from_str("invalid").is_none());
    }

    #[test]
    fn message_kind_is_chatter() {
        assert!(MessageKind::Info.is_chatter());
        assert!(MessageKind::Query.is_chatter());
        assert!(MessageKind::Answer.is_chatter());
        assert!(!MessageKind::Intent.is_chatter());
        assert!(!MessageKind::Status.is_chatter());
    }

    #[test]
    fn mission_impact_as_str() {
        assert_eq!(MissionImpact::Low.as_str(), "low");
        assert_eq!(MissionImpact::High.as_str(), "high");
        assert_eq!(MissionImpact::Breaking.as_str(), "breaking");
    }

    #[test]
    fn mission_impact_from_str() {
        assert_eq!(MissionImpact::from_str("low"), Some(MissionImpact::Low));
        assert_eq!(MissionImpact::from_str("high"), Some(MissionImpact::High));
        assert_eq!(
            MissionImpact::from_str("breaking"),
            Some(MissionImpact::Breaking)
        );
        assert!(MissionImpact::from_str("invalid").is_none());
    }

    #[test]
    fn workspace_status_as_str() {
        assert_eq!(WorkspaceStatus::Stable.as_str(), "stable");
        assert_eq!(WorkspaceStatus::Unstable.as_str(), "unstable");
        assert_eq!(WorkspaceStatus::Testing.as_str(), "testing");
    }

    #[test]
    fn workspace_status_from_str() {
        assert_eq!(
            WorkspaceStatus::from_str("stable"),
            Some(WorkspaceStatus::Stable)
        );
        assert_eq!(
            WorkspaceStatus::from_str("unstable"),
            Some(WorkspaceStatus::Unstable)
        );
        assert_eq!(
            WorkspaceStatus::from_str("testing"),
            Some(WorkspaceStatus::Testing)
        );
        assert!(WorkspaceStatus::from_str("invalid").is_none());
    }

    #[test]
    fn parse_intent_payload_valid_json() {
        let json = serde_json::to_string(&IntentPayload {
            mission: "Test mission".into(),
            impact: MissionImpact::High,
            description: "details".into(),
        })
        .unwrap();
        let msg = SyncMessage {
            id: 1,
            workdir: "/repo".into(),
            agent_id: "agent-a".into(),
            agent_name: "copilot".into(),
            kind: MessageKind::Intent,
            message: "test".into(),
            payload: Some(json),
            created_at: 10,
        };
        let result = parse_intent_payload(&msg);
        assert!(result.is_some());
        let payload = result.unwrap();
        assert_eq!(payload.mission, "Test mission");
        assert_eq!(payload.impact, MissionImpact::High);
    }

    #[test]
    fn parse_intent_payload_no_payload() {
        let msg = SyncMessage {
            id: 1,
            workdir: "/repo".into(),
            agent_id: "agent-a".into(),
            agent_name: "copilot".into(),
            kind: MessageKind::Intent,
            message: "test".into(),
            payload: None,
            created_at: 10,
        };
        assert!(parse_intent_payload(&msg).is_none());
    }

    #[test]
    fn parse_intent_payload_invalid_json() {
        let msg = SyncMessage {
            id: 1,
            workdir: "/repo".into(),
            agent_id: "agent-a".into(),
            agent_name: "copilot".into(),
            kind: MessageKind::Intent,
            message: "test".into(),
            payload: Some("not json".into()),
            created_at: 10,
        };
        assert!(parse_intent_payload(&msg).is_none());
    }

    #[test]
    fn parse_status_payload_valid_json() {
        let json = serde_json::to_string(&StatusPayload {
            status: WorkspaceStatus::Testing,
            message: "running tests".into(),
        })
        .unwrap();
        let msg = SyncMessage {
            id: 1,
            workdir: "/repo".into(),
            agent_id: "agent-a".into(),
            agent_name: "copilot".into(),
            kind: MessageKind::Status,
            message: "test".into(),
            payload: Some(json),
            created_at: 10,
        };
        let result = parse_status_payload(&msg);
        assert!(result.is_some());
        let payload = result.unwrap();
        assert_eq!(payload.status, WorkspaceStatus::Testing);
        assert_eq!(payload.message, "running tests");
    }

    #[test]
    fn parse_status_payload_invalid_json() {
        let msg = SyncMessage {
            id: 1,
            workdir: "/repo".into(),
            agent_id: "agent-a".into(),
            agent_name: "copilot".into(),
            kind: MessageKind::Status,
            message: "test".into(),
            payload: Some("not json".into()),
            created_at: 10,
        };
        assert!(parse_status_payload(&msg).is_none());
    }

    #[test]
    fn is_mission_closed_detects_close_payload() {
        let json = serde_json::json!({
            "mission_closed": true,
            "mission": "done",
            "impact": "low"
        })
        .to_string();
        let msg = SyncMessage {
            id: 1,
            workdir: "/repo".into(),
            agent_id: "agent-a".into(),
            agent_name: "copilot".into(),
            kind: MessageKind::Intent,
            message: "session ended".into(),
            payload: Some(json),
            created_at: 10,
        };
        assert!(is_mission_closed(&msg));
    }

    #[test]
    fn is_mission_closed_ignores_non_close() {
        let json = serde_json::json!({
            "mission": "ongoing",
            "impact": "high",
            "description": "working"
        })
        .to_string();
        let msg = SyncMessage {
            id: 1,
            workdir: "/repo".into(),
            agent_id: "agent-a".into(),
            agent_name: "copilot".into(),
            kind: MessageKind::Intent,
            message: "test".into(),
            payload: Some(json),
            created_at: 10,
        };
        assert!(!is_mission_closed(&msg));
    }

    #[test]
    fn is_mission_closed_no_payload() {
        let msg = SyncMessage {
            id: 1,
            workdir: "/repo".into(),
            agent_id: "agent-a".into(),
            agent_name: "copilot".into(),
            kind: MessageKind::Intent,
            message: "test".into(),
            payload: None,
            created_at: 10,
        };
        assert!(!is_mission_closed(&msg));
    }

    #[test]
    fn is_mission_closed_false_value() {
        let json = serde_json::json!({
            "mission_closed": false,
            "mission": "still going"
        })
        .to_string();
        let msg = SyncMessage {
            id: 1,
            workdir: "/repo".into(),
            agent_id: "agent-a".into(),
            agent_name: "copilot".into(),
            kind: MessageKind::Intent,
            message: "test".into(),
            payload: Some(json),
            created_at: 10,
        };
        assert!(!is_mission_closed(&msg));
    }

    #[test]
    fn default_status_for_impact_breaking() {
        assert_eq!(
            default_status_for_impact(MissionImpact::Breaking),
            WorkspaceStatus::Unstable
        );
    }

    #[test]
    fn default_status_for_impact_low_high() {
        assert_eq!(
            default_status_for_impact(MissionImpact::Low),
            WorkspaceStatus::Stable
        );
        assert_eq!(
            default_status_for_impact(MissionImpact::High),
            WorkspaceStatus::Stable
        );
    }

    #[test]
    fn summarize_sync_context_empty_messages() {
        let summary = summarize_sync_context(&[], &HashSet::new(), 10);
        assert!(summary.active_intents.is_empty());
        assert!(summary.recent_chatter.is_empty());
        assert_eq!(summary.vibe, WorkspaceStatus::Stable);
    }

    #[test]
    fn summarize_sync_context_mission_close_removes_intent() {
        let intent_payload = serde_json::to_string(&IntentPayload {
            mission: "Refactor auth".into(),
            impact: MissionImpact::High,
            description: "touching login flow".into(),
        })
        .unwrap();
        // Close marker uses mission_closed: true
        let close_payload = serde_json::json!({
            "mission_closed": true,
            "mission": "Refactor auth",
            "impact": "low"
        })
        .to_string();

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
                MessageKind::Info,
                Some(close_payload),
                11, // close after intent
            ),
        ];

        let active = HashSet::from([String::from("agent-a")]);
        let summary = summarize_sync_context(&messages, &active, 10);

        // The close marker should remove the intent
        assert!(summary.active_intents.is_empty());
    }

    #[test]
    fn summarize_sync_context_mission_close_before_intent_keeps_it() {
        // Close at ts=5, intent at ts=10: intent is newer, so it survives.
        let intent_payload = serde_json::to_string(&IntentPayload {
            mission: "New mission".into(),
            impact: MissionImpact::Low,
            description: "details".into(),
        })
        .unwrap();
        let close_payload = serde_json::json!({
            "mission_closed": true,
            "mission": "Old mission",
            "impact": "low"
        })
        .to_string();

        let messages = vec![
            sync_message(
                1,
                "agent-a",
                "copilot",
                MessageKind::Info,
                Some(close_payload),
                5,
            ),
            sync_message(
                2,
                "agent-a",
                "copilot",
                MessageKind::Intent,
                Some(intent_payload),
                10,
            ),
        ];

        let active = HashSet::from([String::from("agent-a")]);
        let summary = summarize_sync_context(&messages, &active, 10);
        assert_eq!(summary.active_intents.len(), 1);
        assert_eq!(summary.active_intents[0].mission, "New mission");
    }

    #[test]
    fn summarize_sync_context_multiple_agents_sorted_by_since() {
        let intent_a = serde_json::to_string(&IntentPayload {
            mission: "Mission A".into(),
            impact: MissionImpact::Low,
            description: "a".into(),
        })
        .unwrap();
        let intent_b = serde_json::to_string(&IntentPayload {
            mission: "Mission B".into(),
            impact: MissionImpact::Low,
            description: "b".into(),
        })
        .unwrap();

        let messages = vec![
            sync_message(1, "agent-b", "claude", MessageKind::Intent, Some(intent_b), 20),
            sync_message(2, "agent-a", "copilot", MessageKind::Intent, Some(intent_a), 10),
        ];

        let active =
            HashSet::from([String::from("agent-a"), String::from("agent-b")]);
        let summary = summarize_sync_context(&messages, &active, 10);

        assert_eq!(summary.active_intents.len(), 2);
        // sorted by since: agent-a at 10, agent-b at 20
        assert_eq!(summary.active_intents[0].agent_id, "agent-a");
        assert_eq!(summary.active_intents[1].agent_id, "agent-b");
    }

    #[test]
    fn summarize_sync_context_chatter_takes_last_n_reversed() {
        let messages = vec![
            sync_message(1, "a", "a", MessageKind::Query, None, 1),
            sync_message(2, "b", "b", MessageKind::Answer, None, 2),
            sync_message(3, "c", "c", MessageKind::Query, None, 3),
            sync_message(4, "d", "d", MessageKind::Answer, None, 4),
            sync_message(5, "e", "e", MessageKind::Query, None, 5),
        ];

        let summary = summarize_sync_context(&messages, &HashSet::new(), 3);
        assert_eq!(summary.recent_chatter.len(), 3);
        // Should be the last 3 in chronological order
        assert_eq!(summary.recent_chatter[0].id, 3);
        assert_eq!(summary.recent_chatter[1].id, 4);
        assert_eq!(summary.recent_chatter[2].id, 5);
    }

    #[test]
    fn summarize_sync_context_vibe_prefers_unstable_over_testing() {
        // Agent A has Unstable status (Breaking impact, no explicit status msg)
        let intent_a = serde_json::to_string(&IntentPayload {
            mission: "Breaking change".into(),
            impact: MissionImpact::Breaking,
            description: "a".into(),
        })
        .unwrap();
        // Agent B has Testing status (explicit status message)
        let intent_b = serde_json::to_string(&IntentPayload {
            mission: "Testing".into(),
            impact: MissionImpact::Low,
            description: "b".into(),
        })
        .unwrap();
        let status_testing = serde_json::to_string(&StatusPayload {
            status: WorkspaceStatus::Testing,
            message: "tests".into(),
        })
        .unwrap();

        let messages = vec![
            sync_message(1, "agent-a", "a", MessageKind::Intent, Some(intent_a), 10),
            sync_message(2, "agent-b", "b", MessageKind::Intent, Some(intent_b), 11),
            sync_message(3, "agent-b", "b", MessageKind::Status, Some(status_testing), 12),
        ];

        let active = HashSet::from([String::from("agent-a"), String::from("agent-b")]);
        let summary = summarize_sync_context(&messages, &active, 10);
        assert_eq!(summary.vibe, WorkspaceStatus::Unstable);
    }

    #[test]
    fn summarize_sync_context_intent_with_no_status_uses_default() {
        let intent = serde_json::to_string(&IntentPayload {
            mission: "Just intent".into(),
            impact: MissionImpact::High,
            description: "no status update".into(),
        })
        .unwrap();

        let messages = vec![sync_message(
            1,
            "agent-a",
            "copilot",
            MessageKind::Intent,
            Some(intent),
            10,
        )];

        let active = HashSet::from([String::from("agent-a")]);
        let summary = summarize_sync_context(&messages, &active, 10);
        assert_eq!(summary.active_intents[0].status, WorkspaceStatus::Stable);
    }

    #[test]
    fn message_kind_from_str_case_sensitive() {
        assert!(MessageKind::from_str("Info").is_none());
        assert!(MessageKind::from_str("INFO").is_none());
    }

    #[test]
    fn mission_impact_from_str_case_sensitive() {
        assert!(MissionImpact::from_str("Low").is_none());
        assert!(MissionImpact::from_str("HIGH").is_none());
    }

    #[test]
    fn workspace_status_from_str_case_sensitive() {
        assert!(WorkspaceStatus::from_str("Stable").is_none());
        assert!(WorkspaceStatus::from_str("TESTING").is_none());
    }

    #[test]
    fn sync_message_serde_roundtrip() {
        let msg = SyncMessage {
            id: 42,
            workdir: "/repo".into(),
            agent_id: "agent-x".into(),
            agent_name: "test-agent".into(),
            kind: MessageKind::Query,
            message: "hello".into(),
            payload: Some(r#"{"key":"value"}"#.into()),
            created_at: 12345,
        };
        let json = serde_json::to_string(&msg).unwrap();
        let deserialized: SyncMessage = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized.id, 42);
        assert_eq!(deserialized.kind, MessageKind::Query);
        assert_eq!(deserialized.payload, Some(r#"{"key":"value"}"#.into()));
    }

    #[test]
    fn intent_payload_serde_roundtrip() {
        let payload = IntentPayload {
            mission: "test".into(),
            impact: MissionImpact::Breaking,
            description: "desc".into(),
        };
        let json = serde_json::to_string(&payload).unwrap();
        let deserialized: IntentPayload = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized.mission, "test");
        assert_eq!(deserialized.impact, MissionImpact::Breaking);
    }

    #[test]
    fn status_payload_serde_roundtrip() {
        let payload = StatusPayload {
            status: WorkspaceStatus::Unstable,
            message: "unstable now".into(),
        };
        let json = serde_json::to_string(&payload).unwrap();
        let deserialized: StatusPayload = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized.status, WorkspaceStatus::Unstable);
        assert_eq!(deserialized.message, "unstable now");
    }

    #[test]
    fn active_intent_serde_roundtrip() {
        let intent = ActiveIntent {
            agent_id: "a".into(),
            agent_name: "b".into(),
            mission: "m".into(),
            impact: MissionImpact::High,
            description: "d".into(),
            status: WorkspaceStatus::Testing,
            since: 99,
        };
        let json = serde_json::to_string(&intent).unwrap();
        let deserialized: ActiveIntent = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized.since, 99);
        assert_eq!(deserialized.impact, MissionImpact::High);
    }

    #[test]
    fn sync_context_snapshot_serde_roundtrip() {
        let snapshot = SyncContextSnapshot {
            active_intents: vec![],
            recent_chatter: vec![],
            vibe: WorkspaceStatus::Stable,
        };
        let json = serde_json::to_string(&snapshot).unwrap();
        let deserialized: SyncContextSnapshot = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized.vibe, WorkspaceStatus::Stable);
        assert!(deserialized.active_intents.is_empty());
    }

    #[test]
    fn is_mission_closed_malformed_json() {
        let msg = SyncMessage {
            id: 1,
            workdir: "/repo".into(),
            agent_id: "a".into(),
            agent_name: "b".into(),
            kind: MessageKind::Info,
            message: "test".into(),
            payload: Some("{not valid json".into()),
            created_at: 10,
        };
        assert!(!is_mission_closed(&msg));
    }

    #[test]
    fn is_mission_closed_missing_mission_closed_key() {
        let json = serde_json::json!({"other_key": true}).to_string();
        let msg = SyncMessage {
            id: 1,
            workdir: "/repo".into(),
            agent_id: "a".into(),
            agent_name: "b".into(),
            kind: MessageKind::Info,
            message: "test".into(),
            payload: Some(json),
            created_at: 10,
        };
        assert!(!is_mission_closed(&msg));
    }

    #[test]
    fn default_status_for_impact_all_variants() {
        assert_eq!(
            default_status_for_impact(MissionImpact::Low),
            WorkspaceStatus::Stable
        );
        assert_eq!(
            default_status_for_impact(MissionImpact::High),
            WorkspaceStatus::Stable
        );
        assert_eq!(
            default_status_for_impact(MissionImpact::Breaking),
            WorkspaceStatus::Unstable
        );
    }
}
