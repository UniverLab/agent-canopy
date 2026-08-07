use anyhow::{anyhow, Result};
use chrono::{DateTime, Utc};
use rusqlite::{params, OptionalExtension};
use serde_json::Value;
use std::collections::HashSet;
use std::io::{Error as IoError, ErrorKind};

use crate::db::Database;
use crate::domain::blueprints::{builtin_blueprint_specs, Blueprint};
use crate::domain::loops::LoopNodeKind;

impl Database {
    pub fn insert_blueprint(&self, blueprint: &Blueprint) -> Result<()> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| anyhow!("Lock poisoned: {}", e))?;
        conn.execute(
            "INSERT INTO blueprints (id, name, kind, config, builtin, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                &blueprint.id,
                &blueprint.name,
                blueprint.kind.as_str(),
                serde_json::to_string(&blueprint.config)?,
                blueprint.builtin,
                blueprint.created_at.timestamp(),
            ],
        )?;
        Ok(())
    }

    pub fn get_blueprint_by_name(&self, name: &str) -> Result<Option<Blueprint>> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| anyhow!("Lock poisoned: {}", e))?;
        let mut stmt = conn.prepare(
            "SELECT id, name, kind, config, builtin, created_at FROM blueprints WHERE name = ?1",
        )?;
        stmt.query_row(params![name], map_blueprint_row)
            .optional()
            .map_err(Into::into)
    }

    pub fn list_blueprints(&self) -> Result<Vec<Blueprint>> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| anyhow!("Lock poisoned: {}", e))?;
        let mut stmt = conn.prepare(
            "SELECT id, name, kind, config, builtin, created_at
             FROM blueprints ORDER BY builtin DESC, name ASC",
        )?;
        let rows = stmt.query_map([], map_blueprint_row)?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(Into::into)
    }

    /// Delete a blueprint by name. Callers must enforce the builtin guard
    /// (see `validate_blueprint_deletable`) before calling this — this
    /// function performs no such check itself.
    pub fn delete_blueprint_by_name(&self, name: &str) -> Result<bool> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| anyhow!("Lock poisoned: {}", e))?;
        let rows = conn.execute("DELETE FROM blueprints WHERE name = ?1", params![name])?;
        Ok(rows > 0)
    }

    /// Seed the builtin blueprints (the proven 5-node pattern) and keep
    /// already-seeded builtin rows in sync with the current
    /// `builtin_blueprint_specs()` shape. Idempotent and safe on every
    /// daemon startup:
    ///
    /// - A name the spec no longer produces (e.g. the pre-rename
    ///   `implementer-claude`) is deleted, provided the row is still
    ///   `builtin` — a renamed builtin must not linger forever alongside its
    ///   replacement (see the harness-free blueprint rename).
    /// - A name the spec produces that already exists as a `builtin` row has
    ///   its `kind`/`config` overwritten to match the spec exactly, so a
    ///   stale shape (e.g. an old inline `prompt` instead of
    ///   `prompt_preset`, or a leftover `platform`) gets reconciled instead
    ///   of living on forever. Builtins have no update tool, so this startup
    ///   sync is the only path that can ever change one.
    /// - A name that exists as a *non-builtin* (custom, user-created) row is
    ///   left completely alone — never overwritten, never deleted.
    pub fn seed_builtin_blueprints(&self) -> Result<()> {
        let specs = builtin_blueprint_specs();
        let current_names: HashSet<&str> = specs.iter().map(|(name, _, _)| *name).collect();

        for existing in self.list_blueprints()? {
            if existing.builtin && !current_names.contains(existing.name.as_str()) {
                self.delete_blueprint_by_name(&existing.name)?;
            }
        }

        for (name, kind, config) in specs {
            match self.get_blueprint_by_name(name)? {
                Some(existing) if existing.builtin => {
                    if existing.kind != kind || existing.config != config {
                        self.update_builtin_blueprint(&existing.id, kind, &config)?;
                    }
                }
                // Name already claimed by a custom blueprint — leave it be.
                Some(_) => {}
                None => {
                    self.insert_blueprint(&Blueprint {
                        id: uuid::Uuid::new_v4().to_string(),
                        name: name.to_string(),
                        kind,
                        config,
                        builtin: true,
                        created_at: Utc::now(),
                    })?;
                }
            }
        }
        Ok(())
    }

    /// Overwrite a builtin blueprint row's `kind`/`config` in place. Only
    /// ever called by [`Self::seed_builtin_blueprints`] to reconcile a
    /// stale builtin shape with the current spec — builtins have no
    /// caller-facing update tool.
    fn update_builtin_blueprint(&self, id: &str, kind: LoopNodeKind, config: &Value) -> Result<()> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| anyhow!("Lock poisoned: {}", e))?;
        conn.execute(
            "UPDATE blueprints SET kind = ?1, config = ?2 WHERE id = ?3",
            params![kind.as_str(), serde_json::to_string(config)?, id],
        )?;
        Ok(())
    }
}

fn map_blueprint_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Blueprint> {
    let kind = LoopNodeKind::from_str(&row.get::<_, String>(2)?).ok_or_else(|| {
        rusqlite::Error::FromSqlConversionFailure(
            2,
            rusqlite::types::Type::Text,
            Box::new(IoError::new(
                ErrorKind::InvalidData,
                "Invalid blueprint kind",
            )),
        )
    })?;
    let config_raw: String = row.get(3)?;
    let config = serde_json::from_str(&config_raw).map_err(|error| {
        rusqlite::Error::FromSqlConversionFailure(3, rusqlite::types::Type::Text, Box::new(error))
    })?;

    Ok(Blueprint {
        id: row.get(0)?,
        name: row.get(1)?,
        kind,
        config,
        builtin: row.get(4)?,
        created_at: from_timestamp(row.get(5)?)?,
    })
}

fn from_timestamp(value: i64) -> rusqlite::Result<DateTime<Utc>> {
    DateTime::<Utc>::from_timestamp(value, 0).ok_or_else(|| {
        rusqlite::Error::FromSqlConversionFailure(
            0,
            rusqlite::types::Type::Integer,
            Box::new(IoError::new(
                ErrorKind::InvalidData,
                "Invalid timestamp value",
            )),
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn seed_builtin_blueprints_is_idempotent_across_fresh_startups() {
        let dir = tempdir().unwrap();
        let db = Database::new(&dir.path().join("test.db")).unwrap();

        // Database::new already seeds once; seed again to simulate a second
        // daemon startup against the same database.
        db.seed_builtin_blueprints().unwrap();

        let blueprints = db.list_blueprints().unwrap();
        let mut names: Vec<&str> = blueprints.iter().map(|b| b.name.as_str()).collect();
        names.sort_unstable();
        let mut expected: Vec<&str> = builtin_blueprint_specs()
            .into_iter()
            .map(|(name, _, _)| name)
            .collect();
        expected.sort_unstable();
        assert_eq!(names, expected);
        assert!(blueprints.iter().all(|b| b.builtin));
    }

    /// The migration case the spec calls out explicitly: an existing
    /// installation whose DB already has the old harness-bearing builtin
    /// rows (`implementer-claude`, `reviewer-committer-mimo`,
    /// `resilience-mimo`, one-line inline `prompt` instead of
    /// `prompt_preset`) must end up with exactly the current builtin shape
    /// after a startup reseed — old names gone, new names present, no
    /// `platform`/`cli`/`model` anywhere, and it must be idempotent across a
    /// second reseed.
    #[test]
    fn seed_builtin_blueprints_migrates_pre_rename_harness_bearing_rows() {
        let dir = tempdir().unwrap();
        let db = Database::new(&dir.path().join("test.db")).unwrap();

        // Simulate an installation from before this change: wipe the
        // freshly-seeded current-shape builtins and replace them with the
        // old harness-bearing shape under the old names.
        for (name, _, _) in builtin_blueprint_specs() {
            db.delete_blueprint_by_name(name).unwrap();
        }
        let legacy = [
            (
                "implementer-claude",
                LoopNodeKind::Agent,
                serde_json::json!({"platform": "claude", "prompt": "Implement this spec: …"}),
            ),
            (
                "cargo-gates",
                LoopNodeKind::Check,
                serde_json::json!({"command": "cargo test"}),
            ),
            (
                "reviewer-committer-mimo",
                LoopNodeKind::Agent,
                serde_json::json!({"platform": "mimo", "prompt": "Review the changes …"}),
            ),
            (
                "commit-check",
                LoopNodeKind::Check,
                serde_json::json!({"command": "test \"$(git rev-parse HEAD)\" != \"{{spec_start_head}}\""}),
            ),
            (
                "resilience-mimo",
                LoopNodeKind::Agent,
                serde_json::json!({"platform": "mimo", "prompt": "A node in this loop failed …"}),
            ),
        ];
        for (name, kind, config) in legacy {
            db.insert_blueprint(&Blueprint {
                id: uuid::Uuid::new_v4().to_string(),
                name: name.to_string(),
                kind,
                config,
                builtin: true,
                created_at: Utc::now(),
            })
            .unwrap();
        }

        db.seed_builtin_blueprints().unwrap();

        let after = db.list_blueprints().unwrap();
        let mut names: Vec<&str> = after.iter().map(|b| b.name.as_str()).collect();
        names.sort_unstable();
        let mut expected: Vec<&str> = builtin_blueprint_specs()
            .into_iter()
            .map(|(name, _, _)| name)
            .collect();
        expected.sort_unstable();
        assert_eq!(names, expected, "old-named builtins must not linger");

        for blueprint in &after {
            assert!(blueprint.config.get("platform").is_none());
            assert!(blueprint.config.get("cli").is_none());
            assert!(blueprint.config.get("model").is_none());
        }
        let implementer = db.get_blueprint_by_name("implementer").unwrap().unwrap();
        assert_eq!(implementer.config["prompt_preset"], "implementer");
        assert!(implementer.config.get("prompt").is_none());

        // Idempotent: a second reseed changes nothing further.
        db.seed_builtin_blueprints().unwrap();
        let after_second = db.list_blueprints().unwrap();
        assert_eq!(after.len(), after_second.len());
    }

    /// A custom blueprint that happens to have claimed a name the builtin
    /// spec now also wants (e.g. a user's own "implementer" predating this
    /// rename) must never be overwritten or deleted by the migration.
    #[test]
    fn seed_builtin_blueprints_never_touches_a_custom_blueprint_with_a_builtin_name() {
        let dir = tempdir().unwrap();
        let db = Database::new(&dir.path().join("test.db")).unwrap();
        db.delete_blueprint_by_name("implementer").unwrap();

        let custom = Blueprint {
            id: uuid::Uuid::new_v4().to_string(),
            name: "implementer".to_string(),
            kind: LoopNodeKind::Agent,
            config: serde_json::json!({"platform": "codex", "prompt": "my own take"}),
            builtin: false,
            created_at: Utc::now(),
        };
        db.insert_blueprint(&custom).unwrap();

        db.seed_builtin_blueprints().unwrap();

        let fetched = db.get_blueprint_by_name("implementer").unwrap().unwrap();
        assert!(!fetched.builtin, "custom blueprint must stay custom");
        assert_eq!(fetched.config["platform"], "codex");
        assert_eq!(fetched.config["prompt"], "my own take");
    }

    #[test]
    fn custom_blueprint_create_list_delete_round_trip() {
        let dir = tempdir().unwrap();
        let db = Database::new(&dir.path().join("test.db")).unwrap();

        let custom = Blueprint {
            id: uuid::Uuid::new_v4().to_string(),
            name: "my-custom-gate".to_string(),
            kind: LoopNodeKind::Gate,
            config: serde_json::json!({ "evaluate": "output_contains", "value": "ok" }),
            builtin: false,
            created_at: Utc::now(),
        };
        db.insert_blueprint(&custom).unwrap();

        let fetched = db.get_blueprint_by_name("my-custom-gate").unwrap().unwrap();
        assert_eq!(fetched.name, "my-custom-gate");
        assert!(!fetched.builtin);

        let all = db.list_blueprints().unwrap();
        assert!(all.iter().any(|b| b.name == "my-custom-gate"));

        let deleted = db.delete_blueprint_by_name("my-custom-gate").unwrap();
        assert!(deleted);
        assert!(db
            .get_blueprint_by_name("my-custom-gate")
            .unwrap()
            .is_none());
    }

    #[test]
    fn deleting_a_builtin_blueprint_from_db_still_removes_it_caller_must_guard() {
        // The DB layer itself performs no builtin check (that's
        // `validate_blueprint_deletable`'s job at the handler layer); pin
        // that behavior so the guard isn't accidentally assumed here.
        let dir = tempdir().unwrap();
        let db = Database::new(&dir.path().join("test.db")).unwrap();
        assert!(db.delete_blueprint_by_name("implementer").unwrap());
    }

    #[test]
    fn get_blueprint_by_name_not_found() {
        let dir = tempdir().unwrap();
        let db = Database::new(&dir.path().join("test.db")).unwrap();
        let result = db.get_blueprint_by_name("nonexistent").unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn delete_blueprint_by_name_not_found() {
        let dir = tempdir().unwrap();
        let db = Database::new(&dir.path().join("test.db")).unwrap();
        let deleted = db.delete_blueprint_by_name("nonexistent").unwrap();
        assert!(!deleted);
    }

    #[test]
    fn insert_and_retrieve_blueprint() {
        let dir = tempdir().unwrap();
        let db = Database::new(&dir.path().join("test.db")).unwrap();

        let bp = Blueprint {
            id: uuid::Uuid::new_v4().to_string(),
            name: "test-blueprint-unique".to_string(),
            kind: LoopNodeKind::Agent,
            config: serde_json::json!({ "key": "value" }),
            builtin: false,
            created_at: Utc::now(),
        };
        db.insert_blueprint(&bp).unwrap();

        let fetched = db
            .get_blueprint_by_name("test-blueprint-unique")
            .unwrap()
            .unwrap();
        assert_eq!(fetched.name, "test-blueprint-unique");
        assert_eq!(fetched.kind, LoopNodeKind::Agent);
    }
}
