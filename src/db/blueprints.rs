use anyhow::{anyhow, Result};
use chrono::{DateTime, Utc};
use rusqlite::{params, OptionalExtension};
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

    /// Seed the builtin blueprints (the proven 5-node pattern) if missing.
    /// Idempotent: a builtin already present (by name) is left untouched, so
    /// this is safe to call on every daemon startup.
    pub fn seed_builtin_blueprints(&self) -> Result<()> {
        for (name, kind, config) in builtin_blueprint_specs() {
            if self.get_blueprint_by_name(name)?.is_some() {
                continue;
            }
            self.insert_blueprint(&Blueprint {
                id: uuid::Uuid::new_v4().to_string(),
                name: name.to_string(),
                kind,
                config,
                builtin: true,
                created_at: Utc::now(),
            })?;
        }
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
        assert!(db.delete_blueprint_by_name("implementer-claude").unwrap());
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
