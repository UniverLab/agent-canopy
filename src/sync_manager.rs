//! `SyncManager` — in-memory fan-out plus advisory sync context.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use tokio::sync::{broadcast, Mutex};

use crate::db::Database;
use crate::domain::sync::{
    summarize_sync_context, IntentPayload, MessageKind, MissionImpact, StatusPayload,
    SyncContextSnapshot, SyncMessage, WorkspaceStatus,
};

const BROADCAST_CAPACITY: usize = 64;
const CONTEXT_WINDOW: usize = 100;

struct WorkdirState {
    tx: broadcast::Sender<SyncMessage>,
}

pub struct SyncManager {
    db: Arc<Database>,
    state: Mutex<HashMap<String, WorkdirState>>,
}

impl SyncManager {
    pub fn new(db: Arc<Database>) -> Self {
        Self {
            db,
            state: Mutex::new(HashMap::new()),
        }
    }

    #[allow(dead_code)]
    pub async fn subscribe(&self, workdir: &str) -> broadcast::Receiver<SyncMessage> {
        self.ensure_sender(workdir).await.subscribe()
    }

    pub async fn declare_intent(
        &self,
        workdir: &str,
        agent_id: &str,
        client_name: Option<&str>,
        mission: &str,
        impact: MissionImpact,
        description: &str,
    ) -> anyhow::Result<SyncMessage> {
        let agent_name = self.build_display_name(workdir, agent_id, client_name)?;
        let payload = serde_json::to_string(&IntentPayload {
            mission: mission.to_owned(),
            impact,
            description: description.to_owned(),
        })?;

        let message = self
            .publish(
                workdir,
                agent_id,
                &agent_name,
                MessageKind::Intent,
                &format!("{agent_name}: {mission}"),
                Some(&payload),
            )
            .await?;
        self.upsert_sync_intelligence_node(crate::db::intelligence::IntelligenceNodeInput {
            id: Some(format!("sync:{workdir}:{agent_id}")),
            kind: "session".to_owned(),
            title: mission.to_owned(),
            body: description.to_owned(),
            metadata: Some(serde_json::json!({
                "source": "sync",
                "workdir": workdir,
                "agent_id": agent_id,
                "agent_name": agent_name,
                "kind": "intent",
                "payload": payload,
            })),
            project_hash: None,
            session_id: Some(format!("sync:{workdir}:{agent_id}")),
            relations: None,
        })?;
        Ok(message)
    }

    pub async fn report_status(
        &self,
        workdir: &str,
        agent_id: &str,
        client_name: Option<&str>,
        status: WorkspaceStatus,
        message: &str,
    ) -> anyhow::Result<SyncMessage> {
        let agent_name = self.build_display_name(workdir, agent_id, client_name)?;
        let payload = serde_json::to_string(&StatusPayload {
            status,
            message: message.to_owned(),
        })?;

        let sync_message = self
            .publish(
                workdir,
                agent_id,
                &agent_name,
                MessageKind::Status,
                message,
                Some(&payload),
            )
            .await?;
        self.upsert_sync_intelligence_node(crate::db::intelligence::IntelligenceNodeInput {
            id: Some(format!("sync:{workdir}:{agent_id}")),
            kind: "session".to_owned(),
            title: message.to_owned(),
            body: message.to_owned(),
            metadata: Some(serde_json::json!({
                "source": "sync",
                "workdir": workdir,
                "agent_id": agent_id,
                "agent_name": agent_name,
                "kind": "status",
                "payload": payload,
            })),
            project_hash: None,
            session_id: Some(format!("sync:{workdir}:{agent_id}")),
            relations: None,
        })?;
        Ok(sync_message)
    }

    pub async fn broadcast(
        &self,
        workdir: &str,
        agent_id: &str,
        client_name: Option<&str>,
        kind: MessageKind,
        message: &str,
        payload: Option<&str>,
    ) -> anyhow::Result<SyncMessage> {
        let agent_name = self.build_display_name(workdir, agent_id, client_name)?;
        self.publish(workdir, agent_id, &agent_name, kind, message, payload)
            .await
    }

    pub fn get_context(
        &self,
        workdir: &str,
        chatter_limit: usize,
    ) -> anyhow::Result<SyncContextSnapshot> {
        let recent_messages = self.db.list_sync_messages(workdir, CONTEXT_WINDOW)?;
        let active_agent_ids: HashSet<String> = self
            .db
            .list_active_sync_agent_ids(workdir)?
            .into_iter()
            .collect();

        Ok(summarize_sync_context(
            &recent_messages,
            &active_agent_ids,
            chatter_limit,
        ))
    }

    async fn publish(
        &self,
        workdir: &str,
        agent_id: &str,
        agent_name: &str,
        kind: MessageKind,
        message: &str,
        payload: Option<&str>,
    ) -> anyhow::Result<SyncMessage> {
        let sync_message = self
            .db
            .insert_sync_message(workdir, agent_id, agent_name, kind, message, payload)?;
        let sender = self.ensure_sender(workdir).await;
        let _ = sender.send(sync_message.clone());
        Ok(sync_message)
    }

    /// Resolves the TUI session name from the DB and appends the client harness name when known.
    /// Result: "laetiporus · copilot" or just "laetiporus" if client_name is unavailable.
    /// The DB name already includes "· cli" for interactive sessions; the header is only appended
    /// when its value differs (avoids duplicating "cortinarius · copilot · copilot").
    fn build_display_name(
        &self,
        workdir: &str,
        agent_id: &str,
        client_name: Option<&str>,
    ) -> anyhow::Result<String> {
        let session_name = self.db.resolve_sync_actor_display_name(workdir, agent_id)?;
        Ok(match client_name {
            Some(c) if !c.is_empty() && !session_name.contains(c) => {
                format!("{session_name} · {c}")
            }
            _ => session_name,
        })
    }

    fn upsert_sync_intelligence_node(
        &self,
        node: crate::db::intelligence::IntelligenceNodeInput,
    ) -> anyhow::Result<()> {
        self.db.upsert_intelligence_node(node)?;
        Ok(())
    }

    async fn ensure_sender(&self, workdir: &str) -> broadcast::Sender<SyncMessage> {
        let mut state = self.state.lock().await;
        state
            .entry(workdir.to_owned())
            .or_insert_with(|| {
                let (tx, _) = broadcast::channel(BROADCAST_CAPACITY);
                WorkdirState { tx }
            })
            .tx
            .clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_manager() -> (SyncManager, tempfile::TempDir) {
        let dir = tempfile::tempdir().expect("create temp dir");
        let db_path = dir.path().join("sync_manager_test.db");
        let db = Database::new(&db_path).expect("create test db");
        (SyncManager::new(Arc::new(db)), dir)
    }

    /// A bridge session that resolved a real identity (mirrors what
    /// `daemon::bridge::run_bridge` registers for a known `agent_id`) must
    /// have its actual session name land in `sync_messages.agent_name`.
    #[tokio::test]
    async fn report_status_uses_real_session_name_not_standalone() {
        let (manager, _dir) = test_manager();
        let workdir = "/tmp/known-workdir";
        let agent_id = "sess-boletus";

        manager
            .db
            .insert_interactive_session(
                agent_id,
                "boletus",
                "claude",
                workdir,
                None,
                Some(4242),
                "interactive",
                None,
            )
            .expect("seed known session");

        let message = manager
            .report_status(
                workdir,
                agent_id,
                None,
                WorkspaceStatus::Stable,
                "all green",
            )
            .await
            .expect("report_status");

        assert_eq!(message.agent_name, "boletus · claude");
        assert_ne!(message.agent_name, "standalone");
    }

    /// A bridge with no resolvable identity (mirrors
    /// `daemon::bridge::register_standalone_session` after the fix) must
    /// still be distinguishable from a real session and must not collapse
    /// to the bare, collidable literal "standalone".
    #[tokio::test]
    async fn broadcast_from_unidentified_bridge_is_explicit_and_distinguishable() {
        let (manager, _dir) = test_manager();
        let workdir = "/tmp/unknown-workdir";
        let fallback_agent_id = format!("standalone-{}", uuid::Uuid::new_v4());

        // Mirrors `register_standalone_session`: the fallback agent_id is
        // reused as the session name so it stays unique per bridge instance.
        manager
            .db
            .insert_interactive_session(
                &fallback_agent_id,
                &fallback_agent_id,
                "bridge",
                workdir,
                Some("canopy bridge"),
                Some(4343),
                "bridge",
                None,
            )
            .expect("seed standalone session");

        let message = manager
            .broadcast(
                workdir,
                &fallback_agent_id,
                None,
                MessageKind::Info,
                "hello",
                None,
            )
            .await
            .expect("broadcast");

        assert_ne!(message.agent_name, "standalone");
        assert!(
            message.agent_name.starts_with("standalone-"),
            "expected an explicit, distinguishable fallback name, got {:?}",
            message.agent_name
        );
        assert_eq!(message.agent_name, format!("{fallback_agent_id} · bridge"));
    }
}
