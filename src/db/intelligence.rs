//! SQLite repositories for the Project Intelligence Layer.

use std::collections::{HashMap, HashSet, VecDeque};

use anyhow::{anyhow, Result};
use chrono::Utc;
use serde::{Deserialize, Serialize};

use crate::db::Database;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IntelligenceNodeRecord {
    pub id: String,
    pub kind: String,
    pub title: String,
    pub body: String,
    pub metadata: Option<String>,
    pub project_hash: Option<String>,
    pub session_id: Option<String>,
    pub created_at: i64,
    pub updated_at: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IntelligenceEdgeRecord {
    pub id: i64,
    pub from_node_id: String,
    pub to_node_id: String,
    pub relation: String,
    pub weight: f64,
    pub created_at: i64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct IntelligenceRelationInput {
    pub to_node_id: String,
    pub relation: String,
    pub weight: Option<f64>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct IntelligenceNodeInput {
    pub id: Option<String>,
    pub kind: String,
    pub title: String,
    pub body: String,
    pub metadata: Option<serde_json::Value>,
    pub project_hash: Option<String>,
    pub session_id: Option<String>,
    pub relations: Option<Vec<IntelligenceRelationInput>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IntelligenceGraphWalk {
    pub root: IntelligenceNodeRecord,
    pub nodes: Vec<IntelligenceNodeRecord>,
    pub edges: Vec<IntelligenceEdgeRecord>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IntelligenceProjectDependencyRecord {
    pub from_node_id: String,
    pub from_title: String,
    pub from_project_hash: Option<String>,
    pub to_node_id: String,
    pub to_title: String,
    pub to_project_hash: Option<String>,
    pub relation: String,
    pub weight: f64,
    pub created_at: i64,
}

impl Database {
    pub fn upsert_intelligence_node(
        &self,
        input: IntelligenceNodeInput,
    ) -> Result<IntelligenceNodeRecord> {
        let node_id = input.id.unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
        let now = Utc::now().timestamp();
        let metadata = input.metadata.map(|value| value.to_string());
        let conn = self
            .conn
            .lock()
            .map_err(|e| anyhow!("Lock poisoned: {}", e))?;

        conn.execute(
            "INSERT INTO intelligence_nodes (
                id, kind, title, body, metadata, project_hash, session_id, created_at, updated_at
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
            ON CONFLICT(id) DO UPDATE SET
                kind = excluded.kind,
                title = excluded.title,
                body = excluded.body,
                metadata = excluded.metadata,
                project_hash = excluded.project_hash,
                session_id = excluded.session_id,
                updated_at = excluded.updated_at",
            rusqlite::params![
                node_id,
                input.kind,
                input.title,
                input.body,
                metadata,
                input.project_hash,
                input.session_id,
                now,
                now
            ],
        )?;

        conn.execute(
            "DELETE FROM intelligence_edges WHERE from_node_id = ?1",
            rusqlite::params![&node_id],
        )?;

        if let Some(relations) = input.relations {
            for relation in relations {
                conn.execute(
                    "INSERT INTO intelligence_edges (
                        from_node_id, to_node_id, relation, weight, created_at
                    ) VALUES (?1, ?2, ?3, ?4, ?5)",
                    rusqlite::params![
                        &node_id,
                        relation.to_node_id,
                        relation.relation,
                        relation.weight.unwrap_or(1.0_f64),
                        now
                    ],
                )?;
            }
        }

        drop(conn);
        self.get_intelligence_node(&node_id)?
            .ok_or_else(|| anyhow!("Failed to load intelligence node '{}'", node_id))
    }

    pub fn get_intelligence_node(&self, id: &str) -> Result<Option<IntelligenceNodeRecord>> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| anyhow!("Lock poisoned: {}", e))?;
        let mut stmt = conn.prepare(
            "SELECT id, kind, title, body, metadata, project_hash, session_id, created_at, updated_at
             FROM intelligence_nodes WHERE id = ?1",
        )?;
        let mut rows = stmt.query(rusqlite::params![id])?;
        if let Some(row) = rows.next()? {
            Ok(Some(Self::read_intelligence_node(row)?))
        } else {
            Ok(None)
        }
    }

    pub fn list_intelligence_nodes(
        &self,
        kind: Option<&str>,
        limit: usize,
    ) -> Result<Vec<IntelligenceNodeRecord>> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| anyhow!("Lock poisoned: {}", e))?;
        let mut stmt = if kind.is_some() {
            conn.prepare(
                "SELECT id, kind, title, body, metadata, project_hash, session_id, created_at, updated_at
                 FROM intelligence_nodes
                 WHERE kind = ?1
                 ORDER BY updated_at DESC
                 LIMIT ?2",
            )?
        } else {
            conn.prepare(
                "SELECT id, kind, title, body, metadata, project_hash, session_id, created_at, updated_at
                 FROM intelligence_nodes
                 ORDER BY updated_at DESC
                 LIMIT ?1",
            )?
        };

        let rows = if let Some(kind) = kind {
            stmt.query_map(
                rusqlite::params![kind, limit as i64],
                Self::read_intelligence_node,
            )?
        } else {
            stmt.query_map(
                rusqlite::params![limit as i64],
                Self::read_intelligence_node,
            )?
        };

        Ok(rows.filter_map(|row| row.ok()).collect())
    }

    pub fn search_intelligence_nodes(
        &self,
        query: &str,
        kind: Option<&str>,
        limit: usize,
    ) -> Result<Vec<IntelligenceNodeRecord>> {
        let query = query.trim();
        if query.is_empty() {
            return Ok(Vec::new());
        }

        let needle = query.to_lowercase();
        let conn = self
            .conn
            .lock()
            .map_err(|e| anyhow!("Lock poisoned: {}", e))?;
        let mut stmt = conn.prepare(
            "SELECT id, kind, title, body, metadata, project_hash, session_id, created_at, updated_at
             FROM intelligence_nodes
             WHERE (?2 IS NULL OR kind = ?2)
               AND (
                   instr(lower(id), ?1) > 0 OR
                   instr(lower(kind), ?1) > 0 OR
                   instr(lower(title), ?1) > 0 OR
                   instr(lower(body), ?1) > 0 OR
                   instr(lower(coalesce(metadata, '')), ?1) > 0
               )
             ORDER BY updated_at DESC
             LIMIT ?3",
        )?;
        let rows = stmt.query_map(
            rusqlite::params![needle, kind, limit as i64],
            Self::read_intelligence_node,
        )?;
        Ok(rows.filter_map(|row| row.ok()).collect())
    }

    pub fn list_recent_intelligence_edges(
        &self,
        node_id: &str,
    ) -> Result<Vec<IntelligenceEdgeRecord>> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| anyhow!("Lock poisoned: {}", e))?;
        let mut stmt = conn.prepare(
            "SELECT id, from_node_id, to_node_id, relation, weight, created_at
             FROM intelligence_edges
             WHERE from_node_id = ?1 OR to_node_id = ?1
             ORDER BY id ASC",
        )?;
        let rows = stmt.query_map(rusqlite::params![node_id], Self::read_intelligence_edge)?;
        Ok(rows.filter_map(|row| row.ok()).collect())
    }

    pub fn walk_intelligence_graph(
        &self,
        node_id: &str,
        depth: usize,
    ) -> Result<Option<IntelligenceGraphWalk>> {
        let Some(root) = self.get_intelligence_node(node_id)? else {
            return Ok(None);
        };

        let mut visited_nodes: HashSet<String> = HashSet::from([root.id.clone()]);
        let mut collected_nodes: HashMap<String, IntelligenceNodeRecord> =
            HashMap::from([(root.id.clone(), root.clone())]);
        let mut collected_edges: Vec<IntelligenceEdgeRecord> = Vec::new();
        let mut frontier = VecDeque::from([root.id.clone()]);

        for _ in 0..depth {
            let current_level: Vec<String> = frontier.drain(..).collect();
            if current_level.is_empty() {
                break;
            }

            let mut next_frontier = VecDeque::new();
            for current in current_level {
                for edge in self.list_recent_intelligence_edges(&current)? {
                    if collected_edges
                        .iter()
                        .all(|existing| existing.id != edge.id)
                    {
                        collected_edges.push(edge.clone());
                    }

                    for neighbor in [edge.from_node_id.clone(), edge.to_node_id.clone()] {
                        if visited_nodes.insert(neighbor.clone()) {
                            if let Some(node) = self.get_intelligence_node(&neighbor)? {
                                collected_nodes.insert(neighbor.clone(), node);
                                next_frontier.push_back(neighbor);
                            }
                        }
                    }
                }
            }
            frontier = next_frontier;
        }

        Ok(Some(IntelligenceGraphWalk {
            root,
            nodes: collected_nodes.into_values().collect(),
            edges: collected_edges,
        }))
    }

    pub fn list_cross_project_dependencies(
        &self,
        limit: usize,
    ) -> Result<Vec<IntelligenceProjectDependencyRecord>> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| anyhow!("Lock poisoned: {}", e))?;
        let mut stmt = conn.prepare(
            "SELECT
                e.from_node_id,
                from_node.title,
                from_node.project_hash,
                e.to_node_id,
                to_node.title,
                to_node.project_hash,
                e.relation,
                e.weight,
                e.created_at
             FROM intelligence_edges e
             JOIN intelligence_nodes from_node ON from_node.id = e.from_node_id
             JOIN intelligence_nodes to_node ON to_node.id = e.to_node_id
             WHERE from_node.kind = 'project'
               AND to_node.kind = 'project'
               AND (
                   instr(lower(e.relation), 'depend') > 0 OR
                   instr(lower(e.relation), 'require') > 0 OR
                   instr(lower(e.relation), 'block') > 0
               )
             ORDER BY e.created_at DESC
             LIMIT ?1",
        )?;
        let rows = stmt.query_map(
            rusqlite::params![limit as i64],
            |row| -> rusqlite::Result<IntelligenceProjectDependencyRecord> {
                Ok(IntelligenceProjectDependencyRecord {
                    from_node_id: row.get(0)?,
                    from_title: row.get(1)?,
                    from_project_hash: row.get(2)?,
                    to_node_id: row.get(3)?,
                    to_title: row.get(4)?,
                    to_project_hash: row.get(5)?,
                    relation: row.get(6)?,
                    weight: row.get(7)?,
                    created_at: row.get(8)?,
                })
            },
        )?;
        Ok(rows.filter_map(|row| row.ok()).collect())
    }

    fn read_intelligence_node(row: &rusqlite::Row<'_>) -> rusqlite::Result<IntelligenceNodeRecord> {
        Ok(IntelligenceNodeRecord {
            id: row.get(0)?,
            kind: row.get(1)?,
            title: row.get(2)?,
            body: row.get(3)?,
            metadata: row.get(4)?,
            project_hash: row.get(5)?,
            session_id: row.get(6)?,
            created_at: row.get(7)?,
            updated_at: row.get(8)?,
        })
    }

    fn read_intelligence_edge(row: &rusqlite::Row<'_>) -> rusqlite::Result<IntelligenceEdgeRecord> {
        Ok(IntelligenceEdgeRecord {
            id: row.get(0)?,
            from_node_id: row.get(1)?,
            to_node_id: row.get(2)?,
            relation: row.get(3)?,
            weight: row.get(4)?,
            created_at: row.get(5)?,
        })
    }
}
