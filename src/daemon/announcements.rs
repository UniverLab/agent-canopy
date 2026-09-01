use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;

use crate::application::notification_service::NotificationService;
use crate::application::ports::StateRepository;
use crate::db::Database;

const ANNOUNCEMENTS_ENDPOINT: &str = "wss://announcements.univerlab.org/ws";
const SEEN_IDS_STATE_KEY: &str = "announcements_seen";
const SEEN_IDS_CAP: usize = 1000;
const SEEN_IDS_PRUNE_TARGET: usize = 500;

const INITIAL_BACKOFF: Duration = Duration::from_secs(5);
const BACKOFF_MULTIPLIER: u32 = 2;
const MAX_BACKOFF: Duration = Duration::from_secs(300);

pub(crate) fn next_backoff_delay(current: Duration) -> Duration {
    let next = current.saturating_mul(BACKOFF_MULTIPLIER);
    if next > MAX_BACKOFF {
        MAX_BACKOFF
    } else {
        next
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Announcement {
    id: String,
    title: String,
    body: String,
    published_at: String,
}

pub(crate) struct AnnouncementsClient {
    db: Arc<Database>,
    notification_service: Arc<dyn NotificationService>,
}

impl AnnouncementsClient {
    pub(crate) fn new(
        db: Arc<Database>,
        notification_service: Arc<dyn NotificationService>,
    ) -> Self {
        Self {
            db,
            notification_service,
        }
    }

    pub(crate) fn start(self: Arc<Self>) -> CancellationToken {
        let cancel = CancellationToken::new();
        let cancel_run = cancel.clone();
        let client = Arc::clone(&self);
        tokio::spawn(async move {
            tracing::info!("Announcements client started");
            client.run_loop(cancel_run).await;
            tracing::info!("Announcements client stopped");
        });
        cancel
    }

    async fn run_loop(&self, cancel: CancellationToken) {
        let mut backoff = INITIAL_BACKOFF;
        loop {
            let result = tokio::select! {
                _ = cancel.cancelled() => break,
                result = self.connect_and_read(&cancel) => result,
            };

            if cancel.is_cancelled() {
                break;
            }

            match &result {
                Ok(()) => {
                    // A session that delivered at least one message counts as
                    // healthy: drop back to the initial delay. We still wait it
                    // out rather than reconnecting instantly, so a server that
                    // closes the socket right after each message cannot push us
                    // into a tight reconnect loop.
                    backoff = INITIAL_BACKOFF;
                    tracing::info!(
                        "Announcements client disconnected, reconnecting in {backoff:?}"
                    );
                }
                Err(reason) => {
                    tracing::info!(
                        "Announcements client disconnected ({reason}), reconnecting in {backoff:?}"
                    );
                }
            }

            tokio::select! {
                _ = cancel.cancelled() => break,
                _ = tokio::time::sleep(backoff) => {}
            }

            if result.is_err() {
                backoff = next_backoff_delay(backoff);
            }
        }
    }

    async fn connect_and_read(&self, cancel: &CancellationToken) -> Result<(), String> {
        use futures::StreamExt;

        let ws = tokio_tungstenite::connect_async(ANNOUNCEMENTS_ENDPOINT)
            .await
            .map_err(|e| format!("connect failed: {e}"))?;

        let mut read = ws.0;
        let mut received_message = false;

        loop {
            let msg = tokio::select! {
                _ = cancel.cancelled() => return Ok(()),
                next = read.next() => next,
            };

            let Some(Ok(msg)) = msg else {
                return if received_message {
                    Ok(())
                } else {
                    Err("connection closed before any message".to_string())
                };
            };

            if let tokio_tungstenite::tungstenite::Message::Text(text) = msg {
                received_message = true;
                self.process_announcement_text(&text);
            }
        }
    }

    fn process_announcement_text(&self, text: &str) {
        let announcement: Announcement = match serde_json::from_str(text) {
            Ok(a) => a,
            Err(e) => {
                tracing::warn!("Announcements client: malformed message: {e}");
                return;
            }
        };

        if self.is_already_seen(&announcement.id) {
            return;
        }

        self.notification_service
            .notify_announcement(&announcement.title, &announcement.body);
        self.mark_seen(&announcement.id);
    }

    fn is_already_seen(&self, id: &str) -> bool {
        self.load_seen_ids().iter().any(|seen| seen == id)
    }

    fn mark_seen(&self, id: &str) {
        let mut seen = self.load_seen_ids();
        if seen.iter().any(|s| s == id) {
            return;
        }
        seen.push(id.to_string());
        self.save_seen_ids(&seen);
    }

    /// Seen announcement IDs in the order they were first observed: oldest at
    /// the front. Order matters so pruning can drop the oldest and keep the
    /// most recent — a `HashSet` round-trip would lose it and prune at random.
    fn load_seen_ids(&self) -> Vec<String> {
        self.db
            .get_state(SEEN_IDS_STATE_KEY)
            .ok()
            .flatten()
            .and_then(|json| serde_json::from_str::<Vec<String>>(&json).ok())
            .unwrap_or_default()
    }

    fn save_seen_ids(&self, ids: &[String]) {
        let pruned: &[String] = if ids.len() > SEEN_IDS_CAP {
            &ids[ids.len() - SEEN_IDS_PRUNE_TARGET..]
        } else {
            ids
        };
        if let Ok(json) = serde_json::to_string(pruned) {
            let _ = self.db.set_state(SEEN_IDS_STATE_KEY, &json);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::application::notification_service::{LoopFinishOutcome, NotificationService};
    use crate::db::Database;
    use std::sync::Mutex;
    use tempfile::tempdir;

    fn test_db() -> Database {
        let dir = tempdir().unwrap();
        Database::new(&dir.path().join("test.db")).unwrap()
    }

    #[derive(Default, Clone)]
    struct RecordingNotificationService {
        announcements: Arc<Mutex<Vec<(String, String)>>>,
    }

    impl RecordingNotificationService {
        fn announcement_count(&self) -> usize {
            self.announcements.lock().unwrap().len()
        }
    }

    impl NotificationService for RecordingNotificationService {
        fn notify_task_completed(&self, _t: &str, _s: bool, _e: Option<i32>) {}
        fn notify_task_failed(&self, _t: &str, _e: i32, _m: &str) {}
        fn notify_watcher_triggered(&self, _w: &str, _p: &str, _e: &str) {}
        fn notify_agent_failed(&self, _a: &str, _c: &str, _e: i32, _o: &str) {}
        fn notify_nursery_failed(&self, _e: &str) {}
        fn notify_loop_started(&self, _l: &str, _s: usize, _r: bool, _f: Option<&str>) {}
        fn notify_spec_completed(
            &self,
            _l: &str,
            _s: &str,
            _d: usize,
            _t: usize,
            _n: Option<&str>,
        ) {
        }
        fn notify_loop_finished(&self, _l: &str, _o: LoopFinishOutcome<'_>) {}
        fn notify_loop_completion_hook_failed(&self, _l: &str, _e: &str) {}
        fn notify_announcement(&self, title: &str, body: &str) {
            self.announcements
                .lock()
                .unwrap()
                .push((title.to_string(), body.to_string()));
        }
    }

    #[test]
    fn announcement_deserialization() {
        let json = r#"{"id":"ann-001","title":"Test","body":"Hello","published_at":"2026-09-01T12:00:00Z"}"#;
        let a: Announcement = serde_json::from_str(json).unwrap();
        assert_eq!(a.id, "ann-001");
        assert_eq!(a.title, "Test");
        assert_eq!(a.body, "Hello");
        assert_eq!(a.published_at, "2026-09-01T12:00:00Z");

        let missing_id = r#"{"title":"Test","body":"Hello","published_at":"2026-09-01T12:00:00Z"}"#;
        assert!(serde_json::from_str::<Announcement>(missing_id).is_err());
    }

    #[test]
    fn seen_ids_persisted_via_db_state() {
        let db = test_db();
        let client = AnnouncementsClient::new(
            Arc::new(db),
            Arc::new(RecordingNotificationService::default()),
        );

        client.save_seen_ids(&["id-1".to_string(), "id-2".to_string()]);

        let loaded = client.load_seen_ids();
        assert!(loaded.iter().any(|s| s == "id-1"));
        assert!(loaded.iter().any(|s| s == "id-2"));
        assert_eq!(loaded.len(), 2);

        client.mark_seen("id-3");
        let loaded2 = client.load_seen_ids();
        assert!(loaded2.iter().any(|s| s == "id-1"));
        assert!(loaded2.iter().any(|s| s == "id-2"));
        assert!(loaded2.iter().any(|s| s == "id-3"));
        assert_eq!(loaded2.len(), 3);

        // Re-marking a known id is a no-op, not a duplicate.
        client.mark_seen("id-3");
        assert_eq!(client.load_seen_ids().len(), 3);
    }

    #[test]
    fn save_seen_ids_prunes_oldest_and_keeps_most_recent() {
        let db = test_db();
        let client = AnnouncementsClient::new(
            Arc::new(db),
            Arc::new(RecordingNotificationService::default()),
        );

        let ids: Vec<String> = (0..SEEN_IDS_CAP + 10).map(|i| format!("id-{i}")).collect();
        client.save_seen_ids(&ids);

        let loaded = client.load_seen_ids();
        assert_eq!(loaded.len(), SEEN_IDS_PRUNE_TARGET);
        assert!(loaded
            .iter()
            .any(|s| s == &format!("id-{}", SEEN_IDS_CAP + 9)));
        assert!(!loaded.iter().any(|s| s == "id-0"));
    }

    #[test]
    fn is_already_seen_returns_false_for_unknown_id() {
        let db = test_db();
        let client = AnnouncementsClient::new(
            Arc::new(db),
            Arc::new(RecordingNotificationService::default()),
        );
        assert!(!client.is_already_seen("unknown-id"));
    }

    #[test]
    fn mark_seen_makes_id_seen() {
        let db = test_db();
        let client = AnnouncementsClient::new(
            Arc::new(db),
            Arc::new(RecordingNotificationService::default()),
        );
        client.mark_seen("new-id");
        assert!(client.is_already_seen("new-id"));
    }

    #[test]
    fn backoff_delays_double_up_to_cap() {
        assert_eq!(
            next_backoff_delay(Duration::from_secs(5)),
            Duration::from_secs(10)
        );
        assert_eq!(
            next_backoff_delay(Duration::from_secs(10)),
            Duration::from_secs(20)
        );
        assert_eq!(
            next_backoff_delay(Duration::from_secs(20)),
            Duration::from_secs(40)
        );
        assert_eq!(
            next_backoff_delay(Duration::from_secs(40)),
            Duration::from_secs(80)
        );
        assert_eq!(
            next_backoff_delay(Duration::from_secs(80)),
            Duration::from_secs(160)
        );
        assert_eq!(
            next_backoff_delay(Duration::from_secs(160)),
            Duration::from_secs(300)
        );
        assert_eq!(
            next_backoff_delay(Duration::from_secs(300)),
            Duration::from_secs(300)
        );
    }

    #[test]
    fn announcement_with_duplicate_id_is_not_notified_twice() {
        let db = test_db();
        let notif = Arc::new(RecordingNotificationService::default());
        let client = AnnouncementsClient::new(
            Arc::new(db),
            Arc::clone(&notif) as Arc<dyn NotificationService>,
        );

        let json = r#"{"id":"ann-dup","title":"Hello","body":"World","published_at":"2026-09-01T12:00:00Z"}"#;
        client.process_announcement_text(json);
        client.process_announcement_text(json);

        assert_eq!(notif.announcement_count(), 1);
    }
}
