//! Achievement persistence via `daemon_state` keys.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use anyhow::Result;

use crate::application::ports::StateRepository;
use crate::db::Database;
use crate::domain::gamification::{MissionId, MISSIONS};

const ACHIEVEMENT_PREFIX: &str = "achievement:";

/// Reads and writes unlocked missions in SQLite `daemon_state`.
pub struct AchievementStore {
    db: Arc<Database>,
    unlocked: HashSet<MissionId>,
    /// Unix timestamps (seconds) at which each mission was unlocked.
    unlock_timestamps: HashMap<MissionId, i64>,
}

impl AchievementStore {
    pub fn load(db: Arc<Database>) -> Result<Self> {
        let mut unlocked = HashSet::new();
        let mut unlock_timestamps = HashMap::new();
        for def in MISSIONS {
            let key = achievement_key(def.id);
            if let Some(ts_str) = db.get_state(&key)? {
                unlocked.insert(def.id);
                if let Ok(ts) = ts_str.parse::<i64>() {
                    unlock_timestamps.insert(def.id, ts);
                }
            }
        }
        Ok(Self {
            db,
            unlocked,
            unlock_timestamps,
        })
    }

    pub fn is_unlocked(&self, id: MissionId) -> bool {
        self.unlocked.contains(&id)
    }

    pub fn unlocked_count(&self) -> usize {
        self.unlocked.len()
    }

    /// Returns the Unix timestamp (seconds) when the mission was unlocked, if available.
    pub fn unlock_timestamp(&self, id: MissionId) -> Option<i64> {
        self.unlock_timestamps.get(&id).copied()
    }

    /// Unlock a mission if not already unlocked. Returns `true` when newly unlocked.
    pub fn unlock(&mut self, id: MissionId) -> Result<bool> {
        if self.unlocked.contains(&id) {
            return Ok(false);
        }
        let now = chrono::Utc::now().timestamp();
        self.db.set_state(&achievement_key(id), &now.to_string())?;
        self.unlocked.insert(id);
        self.unlock_timestamps.insert(id, now);
        Ok(true)
    }
}

fn achievement_key(id: MissionId) -> String {
    format!("{ACHIEVEMENT_PREFIX}{}", id.as_str())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn unlock_persists_and_reloads() {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("test.db");
        let db = Arc::new(Database::new(&db_path).unwrap());

        let mut store = AchievementStore::load(Arc::clone(&db)).unwrap();
        assert!(!store.is_unlocked(MissionId::FireflyCatcher));

        assert!(store.unlock(MissionId::FireflyCatcher).unwrap());
        assert!(!store.unlock(MissionId::FireflyCatcher).unwrap());

        let reloaded = AchievementStore::load(db).unwrap();
        assert!(reloaded.is_unlocked(MissionId::FireflyCatcher));
        assert_eq!(reloaded.unlocked_count(), 1);
    }

    #[test]
    fn load_empty_store_has_zero_unlocked() {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("test.db");
        let db = Arc::new(Database::new(&db_path).unwrap());
        let store = AchievementStore::load(db).unwrap();
        assert_eq!(store.unlocked_count(), 0);
        assert!(!store.is_unlocked(MissionId::FireflyCatcher));
    }

    #[test]
    fn unlock_multiple_missions() {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("test.db");
        let db = Arc::new(Database::new(&db_path).unwrap());
        let mut store = AchievementStore::load(Arc::clone(&db)).unwrap();

        let first = store.unlock(MissionId::FireflyCatcher).unwrap();
        assert!(first);

        // Try all mission variants
        for mission in MISSIONS {
            store.unlock(mission.id).unwrap();
        }
        assert_eq!(store.unlocked_count(), MISSIONS.len());
        // Second time: all are already unlocked
        for mission in MISSIONS {
            assert!(!store.unlock(mission.id).unwrap());
        }
    }

    #[test]
    fn unlock_timestamp_is_recorded() {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("test.db");
        let db = Arc::new(Database::new(&db_path).unwrap());
        let mut store = AchievementStore::load(Arc::clone(&db)).unwrap();

        let before = chrono::Utc::now().timestamp();
        store.unlock(MissionId::FireflyCatcher).unwrap();
        let after = chrono::Utc::now().timestamp();

        let ts = store.unlock_timestamp(MissionId::FireflyCatcher).unwrap();
        assert!(ts >= before && ts <= after);
    }

    #[test]
    fn unlock_timestamp_persists_and_reloads() {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("test.db");
        let db = Arc::new(Database::new(&db_path).unwrap());
        let mut store = AchievementStore::load(Arc::clone(&db)).unwrap();

        store.unlock(MissionId::FireflyCatcher).unwrap();

        let reloaded = AchievementStore::load(db).unwrap();
        assert!(reloaded
            .unlock_timestamp(MissionId::FireflyCatcher)
            .is_some());
    }

    #[test]
    fn unlock_timestamp_none_for_locked_mission() {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("test.db");
        let db = Arc::new(Database::new(&db_path).unwrap());
        let store = AchievementStore::load(db).unwrap();
        assert!(store.unlock_timestamp(MissionId::FireflyCatcher).is_none());
    }

    #[test]
    fn achievement_key_format() {
        let key = achievement_key(MissionId::FireflyCatcher);
        assert!(key.starts_with("achievement:"));
        assert!(key.contains(MissionId::FireflyCatcher.as_str()));
    }

    #[test]
    fn is_unlocked_returns_false_for_nonexistent() {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("test.db");
        let db = Arc::new(Database::new(&db_path).unwrap());
        let store = AchievementStore::load(db).unwrap();
        // A mission that doesn't exist should still return false
        assert!(!store.is_unlocked(MissionId::FireflyCatcher));
    }
}
