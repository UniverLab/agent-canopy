//! Achievement persistence via `daemon_state` keys.

use std::collections::HashSet;
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
}

impl AchievementStore {
    pub fn load(db: Arc<Database>) -> Result<Self> {
        let mut unlocked = HashSet::new();
        for def in MISSIONS {
            let key = achievement_key(def.id);
            if db.get_state(&key)?.is_some() {
                unlocked.insert(def.id);
            }
        }
        Ok(Self { db, unlocked })
    }

    pub fn is_unlocked(&self, id: MissionId) -> bool {
        self.unlocked.contains(&id)
    }

    pub fn unlocked_count(&self) -> usize {
        self.unlocked.len()
    }

    /// Unlock a mission if not already unlocked. Returns `true` when newly unlocked.
    pub fn unlock(&mut self, id: MissionId) -> Result<bool> {
        if self.unlocked.contains(&id) {
            return Ok(false);
        }
        let ts = chrono::Utc::now().timestamp().to_string();
        self.db.set_state(&achievement_key(id), &ts)?;
        self.unlocked.insert(id);
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
}
