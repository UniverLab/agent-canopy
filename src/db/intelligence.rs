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
pub struct IntelligenceSearchResult {
    pub results: Vec<IntelligenceNodeRecord>,
    pub examined_count: i64,
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
    /// Resolve a node id prefix (e.g. "3a476c63") to exactly one node.
    /// - Ok(Some(full_id)) if exactly one node matches
    /// - Ok(None) if no nodes match
    /// - Err with candidate listing if multiple nodes match (ambiguous)
    ///
    /// Search is global (no project_hash filter) so a prefix that lives in a
    /// different project hash still resolves — the corpus is split across two
    /// hashes and callers cite ids by prefix regardless of project.
    pub fn resolve_node_id_by_prefix(&self, prefix: &str) -> Result<Option<String>> {
        if prefix.is_empty() {
            return Ok(None);
        }
        // If exact match exists, return it without LIKE scan — handles full
        // UUIDs and avoids ambiguous error when the exact id happens to share
        // a prefix with others.
        if let Some(_node) = self.get_intelligence_node(prefix)? {
            return Ok(Some(prefix.to_string()));
        }
        let conn = self
            .conn
            .lock()
            .map_err(|e| anyhow!("Lock poisoned: {}", e))?;
        // Escape LIKE metacharacters so a prefix that happens to contain '%' or
        // '_' (or the escape char itself) is matched literally rather than as a
        // wildcard — otherwise a stray '_' would silently over-match and turn a
        // single valid target into a spurious "ambiguous" error.
        let escaped_prefix = prefix
            .replace('\\', "\\\\")
            .replace('%', "\\%")
            .replace('_', "\\_");
        let mut stmt =
            conn.prepare("SELECT id FROM intelligence_nodes WHERE id LIKE ?1 || '%' ESCAPE '\\'")?;
        let ids: Vec<String> = stmt
            .query_map(rusqlite::params![escaped_prefix], |row| row.get(0))?
            .filter_map(|r| r.ok())
            .collect();
        match ids.len() {
            0 => Ok(None),
            1 => Ok(Some(ids.into_iter().next().unwrap())),
            _ => Err(anyhow!(
                "Ambiguous node id prefix '{}' matches {} nodes: {}",
                prefix,
                ids.len(),
                ids.join(", ")
            )),
        }
    }

    pub fn upsert_intelligence_node(
        &self,
        input: IntelligenceNodeInput,
    ) -> Result<(IntelligenceNodeRecord, bool)> {
        let node_id = input.id.unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
        let now = Utc::now().timestamp();
        let metadata = input.metadata.map(|value| value.to_string());
        let conn = self
            .conn
            .lock()
            .map_err(|e| anyhow!("Lock poisoned: {}", e))?;
        // Determine creation vs update before the INSERT — ON CONFLICT would
        // otherwise hide the distinction. For auto-generated ids this is always
        // creation. Done inside the same lock as the INSERT to avoid a race
        // where another thread inserts between the check and the insert.
        let was_created: bool = {
            let mut stmt =
                conn.prepare("SELECT 1 FROM intelligence_nodes WHERE id = ?1 LIMIT 1")?;
            let mut rows = stmt.query(rusqlite::params![&node_id])?;
            rows.next()?.is_none()
        };

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
        let record = self
            .get_intelligence_node(&node_id)?
            .ok_or_else(|| anyhow!("Failed to load intelligence node '{}'", node_id))?;
        Ok((record, was_created))
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

    /// Remove an intelligence node and every relation touching it, in one
    /// transaction.
    ///
    /// Hard delete, not a soft-delete/tombstone: the graph is meant to be
    /// able to forget bad or superseded knowledge, and a tombstone column
    /// would require every read path (search, graph walk, context assembly)
    /// to filter it out in the same change — a half-applied filter would
    /// hide deleted nodes from search while `intelligence_get_context` kept
    /// injecting them, which is worse than no deletion at all.
    ///
    /// Edges are not deleted by hand here: `intelligence_edges` declares
    /// `ON DELETE CASCADE` on both `from_node_id` and `to_node_id`, and
    /// `PRAGMA foreign_keys=ON` is set on every connection (see
    /// `Database::new`), so deleting the node row cascades through both
    /// directions automatically — a manual `DELETE FROM intelligence_edges`
    /// alongside it would be duplicated logic that can drift. The relation
    /// count returned to callers is captured before the delete since the
    /// cascade itself doesn't report how many rows it removed. Both
    /// `intelligence_edges` FK columns already have covering indexes
    /// (`idx_intelligence_edges_from` / `idx_intelligence_edges_to`), so
    /// this lookup and the cascade are both index-driven rather than a full
    /// table scan.
    ///
    /// Returns `Ok(None)` if no node with `id` exists, so callers can
    /// distinguish "already gone" from a successful delete rather than
    /// treating a no-op as success.
    pub fn delete_intelligence_node(&self, id: &str) -> Result<Option<usize>> {
        let mut conn = self
            .conn
            .lock()
            .map_err(|e| anyhow!("Lock poisoned: {}", e))?;
        let tx = conn.transaction()?;

        let relations_removed: i64 = tx.query_row(
            "SELECT COUNT(*) FROM intelligence_edges WHERE from_node_id = ?1 OR to_node_id = ?1",
            rusqlite::params![id],
            |row| row.get(0),
        )?;

        let rows_deleted = tx.execute(
            "DELETE FROM intelligence_nodes WHERE id = ?1",
            rusqlite::params![id],
        )?;

        if rows_deleted == 0 {
            return Ok(None);
        }

        tx.commit()?;
        Ok(Some(relations_removed as usize))
    }

    /// Fetch a single relation (edge) by ID.
    pub fn get_intelligence_edge(&self, edge_id: i64) -> Result<Option<IntelligenceEdgeRecord>> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| anyhow!("Lock poisoned: {}", e))?;
        let mut stmt = conn.prepare(
            "SELECT id, from_node_id, to_node_id, relation, weight, created_at
             FROM intelligence_edges WHERE id = ?1",
        )?;
        let mut rows = stmt.query(rusqlite::params![edge_id])?;
        if let Some(row) = rows.next()? {
            Ok(Some(Self::read_intelligence_edge(row)?))
        } else {
            Ok(None)
        }
    }

    /// Remove a single relation (edge) without touching either endpoint
    /// node. Returns `false` if no edge with `edge_id` exists.
    pub fn delete_intelligence_edge(&self, edge_id: i64) -> Result<bool> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| anyhow!("Lock poisoned: {}", e))?;
        let rows_deleted = conn.execute(
            "DELETE FROM intelligence_edges WHERE id = ?1",
            rusqlite::params![edge_id],
        )?;
        Ok(rows_deleted > 0)
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
    ) -> Result<IntelligenceSearchResult> {
        let terms: Vec<String> = query.split_whitespace().map(str::to_lowercase).collect();

        // Always report how many nodes would be considered for this kind filter.
        let examined_count = self.count_intelligence_nodes_for_search(kind)?;

        if terms.is_empty() {
            return Ok(IntelligenceSearchResult {
                results: Vec::new(),
                examined_count,
            });
        }

        let conn = self
            .conn
            .lock()
            .map_err(|e| anyhow!("Lock poisoned: {}", e))?;

        // Ranking follows the spec guideline: order primarily by how many
        // distinct query terms match anywhere, then break ties by *where* they
        // match (a title hit outweighs a body hit), then by recency. The WHERE
        // clause keeps OR semantics — any term matching any field is enough, so
        // extra words degrade a node's rank but never drop it from the results.
        let mut match_count_parts: Vec<String> = Vec::with_capacity(terms.len());
        let mut score_parts: Vec<String> = Vec::with_capacity(terms.len() * 5);
        let mut or_clauses: Vec<String> = Vec::with_capacity(terms.len() * 5);
        for i in 0..terms.len() {
            let p = i + 2;
            score_parts.push(format!(
                "(CASE WHEN instr(lower(title), ?{p}) > 0 THEN 3 ELSE 0 END)"
            ));
            score_parts.push(format!(
                "(CASE WHEN instr(lower(body), ?{p}) > 0 THEN 1 ELSE 0 END)"
            ));
            score_parts.push(format!(
                "(CASE WHEN instr(lower(coalesce(metadata, '')), ?{p}) > 0 THEN 1 ELSE 0 END)"
            ));
            score_parts.push(format!(
                "(CASE WHEN instr(lower(id), ?{p}) > 0 THEN 1 ELSE 0 END)"
            ));
            score_parts.push(format!(
                "(CASE WHEN instr(lower(kind), ?{p}) > 0 THEN 1 ELSE 0 END)"
            ));

            let term_fields: Vec<String> =
                ["id", "kind", "title", "body", "coalesce(metadata, '')"]
                    .into_iter()
                    .map(|field| format!("instr(lower({field}), ?{p}) > 0"))
                    .collect();
            match_count_parts.push(format!(
                "(CASE WHEN {} THEN 1 ELSE 0 END)",
                term_fields.join(" OR ")
            ));
            or_clauses.extend(term_fields);
        }

        let match_count_expr = match_count_parts.join(" + ");
        let score_expr = score_parts.join(" + ");
        let or_clause = or_clauses.join(" OR ");
        let limit_placeholder = terms.len() + 2;
        let sql = format!(
            "SELECT id, kind, title, body, metadata, project_hash, session_id, created_at, updated_at, \
             ({match_count_expr}) AS match_count, ({score_expr}) AS score \
             FROM intelligence_nodes \
             WHERE (?1 IS NULL OR kind = ?1) AND ({or_clause}) \
             ORDER BY match_count DESC, score DESC, updated_at DESC \
             LIMIT ?{limit_placeholder}"
        );

        let mut stmt = conn.prepare(&sql)?;
        let mut params: Vec<Box<dyn rusqlite::ToSql>> = Vec::with_capacity(terms.len() + 2);
        params.push(Box::new(kind.map(str::to_string)));
        for term in &terms {
            params.push(Box::new(term.clone()));
        }
        params.push(Box::new(limit as i64));
        let param_refs: Vec<&dyn rusqlite::ToSql> = params.iter().map(Box::as_ref).collect();

        let rows = stmt.query_map(param_refs.as_slice(), Self::read_intelligence_node)?;
        let results: Vec<IntelligenceNodeRecord> = rows.filter_map(|row| row.ok()).collect();
        Ok(IntelligenceSearchResult {
            results,
            examined_count,
        })
    }

    fn count_intelligence_nodes_for_search(&self, kind: Option<&str>) -> Result<i64> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| anyhow!("Lock poisoned: {}", e))?;
        let mut stmt = conn
            .prepare("SELECT COUNT(*) FROM intelligence_nodes WHERE (?1 IS NULL OR kind = ?1)")?;
        Ok(stmt.query_row(rusqlite::params![kind], |row| row.get(0))?)
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

    // ── Intelligence V2: Project-Linked Knowledge ──

    /// Upsert the intelligence root node (`kind='project'`) for a registered
    /// project. Idempotent: node id is derived from the project hash.
    pub fn ensure_project_node(
        &self,
        project: &crate::domain::project::Project,
    ) -> Result<IntelligenceNodeRecord> {
        self.upsert_intelligence_node(IntelligenceNodeInput {
            id: Some(format!("project:{}", project.hash)),
            kind: "project".to_string(),
            title: project.name.clone(),
            body: project
                .description
                .clone()
                .unwrap_or_else(|| project.path.clone()),
            metadata: Some(serde_json::json!({
                "source": "registry",
                "path": project.path,
            })),
            project_hash: Some(project.hash.clone()),
            session_id: None,
            relations: None,
        })
        .map(|(record, _created)| record)
    }

    /// Create missing `kind='project'` root nodes for already-registered
    /// projects. Runs at database open so graphs created before this code
    /// existed become linkable.
    pub fn backfill_project_nodes(&self) -> Result<usize> {
        let projects = self.list_projects()?;
        let mut created = 0;
        for project in &projects {
            if self.find_project_node(&project.hash)?.is_none() {
                self.ensure_project_node(project)?;
                created += 1;
            }
        }
        Ok(created)
    }

    /// List all indexed project nodes for the project picker.
    pub fn list_intelligence_projects(
        &self,
        query: Option<&str>,
        limit: usize,
    ) -> Result<Vec<IntelligenceNodeRecord>> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| anyhow!("Lock poisoned: {}", e))?;

        let sql = if query.is_some() {
            "SELECT id, kind, title, body, metadata, project_hash, session_id, created_at, updated_at
             FROM intelligence_nodes
             WHERE kind = 'project'
               AND (instr(lower(title), ?1) > 0 OR instr(lower(body), ?1) > 0 OR instr(lower(coalesce(metadata, '')), ?1) > 0)
             ORDER BY updated_at DESC
             LIMIT ?2"
        } else {
            "SELECT id, kind, title, body, metadata, project_hash, session_id, created_at, updated_at
             FROM intelligence_nodes
             WHERE kind = 'project'
             ORDER BY updated_at DESC
             LIMIT ?1"
        };

        let mut stmt = conn.prepare(sql)?;
        let rows = if let Some(q) = query {
            stmt.query_map(
                rusqlite::params![q.to_lowercase(), limit as i64],
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

    /// Create a relationship edge between two project nodes.
    /// If an edge with the same from/to/relation already exists, returns it.
    pub fn link_projects(
        &self,
        from_project_hash: &str,
        to_project_hash: &str,
        relation: &str,
        weight: Option<f64>,
    ) -> Result<IntelligenceEdgeRecord> {
        let from_node = self
            .find_project_node(from_project_hash)?
            .ok_or_else(|| anyhow!("Project node not found for hash '{}'", from_project_hash))?;
        let to_node = self
            .find_project_node(to_project_hash)?
            .ok_or_else(|| anyhow!("Project node not found for hash '{}'", to_project_hash))?;

        let weight = weight.unwrap_or(1.0);
        let conn = self
            .conn
            .lock()
            .map_err(|e| anyhow!("Lock poisoned: {}", e))?;

        // Check for existing edge with the same from/to/relation.
        let mut stmt = conn.prepare(
            "SELECT id, from_node_id, to_node_id, relation, weight, created_at
             FROM intelligence_edges
             WHERE from_node_id = ?1 AND to_node_id = ?2 AND relation = ?3
             LIMIT 1",
        )?;
        let existing = stmt
            .query_row(
                rusqlite::params![from_node.id, to_node.id, relation],
                Self::read_intelligence_edge,
            )
            .ok();
        if let Some(edge) = existing {
            return Ok(edge);
        }

        let now = Utc::now().timestamp();
        drop(stmt);
        let mut stmt = conn.prepare(
            "INSERT INTO intelligence_edges (from_node_id, to_node_id, relation, weight, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5)",
        )?;
        stmt.execute(rusqlite::params![
            from_node.id,
            to_node.id,
            relation,
            weight,
            now
        ])?;
        drop(stmt);

        let edge_id = conn.last_insert_rowid();
        Ok(IntelligenceEdgeRecord {
            id: edge_id,
            from_node_id: from_node.id,
            to_node_id: to_node.id,
            relation: relation.to_string(),
            weight,
            created_at: now,
        })
    }

    /// Find a project node by its hash (project_hash column or id match).
    fn find_project_node(&self, project_hash: &str) -> Result<Option<IntelligenceNodeRecord>> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| anyhow!("Lock poisoned: {}", e))?;
        let mut stmt = conn.prepare(
            "SELECT id, kind, title, body, metadata, project_hash, session_id, created_at, updated_at
             FROM intelligence_nodes
             WHERE kind = 'project'
               AND (project_hash = ?1 OR id = ?1)
             LIMIT 1",
        )?;
        let mut rows = stmt.query(rusqlite::params![project_hash])?;
        if let Some(row) = rows.next()? {
            Ok(Some(Self::read_intelligence_node(row)?))
        } else {
            Ok(None)
        }
    }

    /// Get facts and patterns linked to a specific project.
    pub fn list_project_knowledge(
        &self,
        project_hash: &str,
        kind: Option<&str>,
        limit: usize,
    ) -> Result<Vec<IntelligenceNodeRecord>> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| anyhow!("Lock poisoned: {}", e))?;

        let sql = match kind {
            Some(_k) => "SELECT id, kind, title, body, metadata, project_hash, session_id, created_at, updated_at
                        FROM intelligence_nodes
                        WHERE project_hash = ?1 AND kind = ?2
                        ORDER BY updated_at DESC
                        LIMIT ?3",
            None => "SELECT id, kind, title, body, metadata, project_hash, session_id, created_at, updated_at
                     FROM intelligence_nodes
                     WHERE project_hash = ?1 AND kind IN ('fact', 'pattern')
                     ORDER BY updated_at DESC
                     LIMIT ?2",
        };

        let mut stmt = conn.prepare(sql)?;
        let rows = match kind {
            Some(_) => stmt.query_map(
                rusqlite::params![project_hash, kind.unwrap(), limit as i64],
                Self::read_intelligence_node,
            )?,
            None => stmt.query_map(
                rusqlite::params![project_hash, limit as i64],
                Self::read_intelligence_node,
            )?,
        };
        Ok(rows.filter_map(|row| row.ok()).collect())
    }

    /// List projects related to a given project via edges.
    /// Returns (related_node, edge) where edge is reoriented so
    /// from_node_id always equals the queried project's node id.
    pub fn list_related_projects(
        &self,
        project_hash: &str,
        limit: usize,
    ) -> Result<Vec<(IntelligenceNodeRecord, IntelligenceEdgeRecord)>> {
        let Some(project_node) = self.find_project_node(project_hash)? else {
            return Ok(Vec::new());
        };

        let conn = self
            .conn
            .lock()
            .map_err(|e| anyhow!("Lock poisoned: {}", e))?;
        let mut stmt = conn.prepare(
            "SELECT
                 n.id, n.kind, n.title, n.body, n.metadata, n.project_hash, n.session_id, n.created_at, n.updated_at,
                 e.id, e.from_node_id, e.to_node_id, e.relation, e.weight, e.created_at
             FROM intelligence_edges e
             JOIN intelligence_nodes n ON (
                 (e.from_node_id = ?1 AND n.id = e.to_node_id) OR
                 (e.to_node_id = ?1 AND n.id = e.from_node_id)
             )
             WHERE n.kind = 'project'
             ORDER BY e.weight DESC, e.created_at DESC
             LIMIT ?2",
        )?;
        let project_id = project_node.id.clone();
        let rows = stmt.query_map(
            rusqlite::params![project_node.id, limit as i64],
            move |row| -> rusqlite::Result<_> {
                let node = Self::read_intelligence_node(row)?;
                let raw_from: String = row.get(10)?;
                let raw_to: String = row.get(11)?;
                // Reorient so from_node_id is always the queried project.
                let (oriented_from, oriented_to) = if raw_from == project_id {
                    (raw_from, raw_to)
                } else {
                    (raw_to, raw_from)
                };
                let edge = IntelligenceEdgeRecord {
                    id: row.get(9)?,
                    from_node_id: oriented_from,
                    to_node_id: oriented_to,
                    relation: row.get(12)?,
                    weight: row.get(13)?,
                    created_at: row.get(14)?,
                };
                Ok((node, edge))
            },
        )?;
        Ok(rows.filter_map(|row| row.ok()).collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn test_db() -> Database {
        let dir = tempdir().unwrap();
        Database::new(&dir.path().join("test.db")).unwrap()
    }

    fn sample_node_input(id: &str) -> IntelligenceNodeInput {
        IntelligenceNodeInput {
            id: Some(id.to_string()),
            kind: "fact".to_string(),
            title: format!("Node {id}"),
            body: "Test body".to_string(),
            project_hash: Some("proj1".to_string()),
            session_id: None,
            metadata: None,
            relations: None,
        }
    }

    #[test]
    fn upsert_and_get_intelligence_node() {
        let db = test_db();
        let input = sample_node_input("node1");
        db.upsert_intelligence_node(input).unwrap();

        let retrieved = db.get_intelligence_node("node1").unwrap();
        assert!(retrieved.is_some());
        let retrieved = retrieved.unwrap();
        assert_eq!(retrieved.id, "node1");
        assert_eq!(retrieved.title, "Node node1");
    }

    #[test]
    fn get_intelligence_node_not_found() {
        let db = test_db();
        let result = db.get_intelligence_node("nonexistent").unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn list_intelligence_nodes_empty() {
        let db = test_db();
        let nodes = db.list_intelligence_nodes(None, 100).unwrap();
        assert!(nodes.is_empty());
    }

    #[test]
    fn list_intelligence_nodes_with_nodes() {
        let db = test_db();
        let input1 = sample_node_input("node1");
        let input2 = sample_node_input("node2");
        db.upsert_intelligence_node(input1).unwrap();
        db.upsert_intelligence_node(input2).unwrap();

        let nodes = db.list_intelligence_nodes(None, 100).unwrap();
        assert_eq!(nodes.len(), 2);
    }

    #[test]
    fn list_intelligence_nodes_by_kind() {
        let db = test_db();
        let mut input1 = sample_node_input("node1");
        input1.kind = "fact".to_string();
        let mut input2 = sample_node_input("node2");
        input2.kind = "pattern".to_string();
        db.upsert_intelligence_node(input1).unwrap();
        db.upsert_intelligence_node(input2).unwrap();

        let nodes = db.list_intelligence_nodes(Some("fact"), 100).unwrap();
        assert_eq!(nodes.len(), 1);
        assert_eq!(nodes[0].id, "node1");
    }

    #[test]
    fn delete_intelligence_node() {
        let db = test_db();
        let input = sample_node_input("node1");
        db.upsert_intelligence_node(input).unwrap();

        let relations_removed = db.delete_intelligence_node("node1").unwrap();
        assert_eq!(relations_removed, Some(0));
        let retrieved = db.get_intelligence_node("node1").unwrap();
        assert!(retrieved.is_none());
    }

    #[test]
    fn delete_intelligence_node_missing_returns_none() {
        let db = test_db();
        let result = db.delete_intelligence_node("nonexistent").unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn delete_intelligence_node_removes_edges_on_both_sides() {
        let db = test_db();
        db.upsert_intelligence_node(sample_node_input("a")).unwrap();
        db.upsert_intelligence_node(IntelligenceNodeInput {
            relations: Some(vec![IntelligenceRelationInput {
                to_node_id: "a".to_string(),
                relation: "supports".to_string(),
                weight: None,
            }]),
            ..sample_node_input("b")
        })
        .unwrap();
        db.upsert_intelligence_node(IntelligenceNodeInput {
            relations: Some(vec![IntelligenceRelationInput {
                to_node_id: "b".to_string(),
                relation: "supports".to_string(),
                weight: None,
            }]),
            ..sample_node_input("c")
        })
        .unwrap();

        // "b" sits in the middle of a -> b, b -> c: two edges touch it.
        let relations_removed = db.delete_intelligence_node("b").unwrap();
        assert_eq!(relations_removed, Some(2));
        assert!(db.get_intelligence_node("b").unwrap().is_none());
        assert!(db.get_intelligence_node("a").unwrap().is_some());
        assert!(db.get_intelligence_node("c").unwrap().is_some());
        assert!(db.list_recent_intelligence_edges("a").unwrap().is_empty());
        assert!(db.list_recent_intelligence_edges("c").unwrap().is_empty());
    }

    #[test]
    fn graph_walk_after_deleting_middle_node_does_not_reference_it() {
        let db = test_db();
        db.upsert_intelligence_node(sample_node_input("a")).unwrap();
        db.upsert_intelligence_node(IntelligenceNodeInput {
            relations: Some(vec![IntelligenceRelationInput {
                to_node_id: "a".to_string(),
                relation: "supports".to_string(),
                weight: None,
            }]),
            ..sample_node_input("b")
        })
        .unwrap();
        db.upsert_intelligence_node(IntelligenceNodeInput {
            relations: Some(vec![IntelligenceRelationInput {
                to_node_id: "b".to_string(),
                relation: "supports".to_string(),
                weight: None,
            }]),
            ..sample_node_input("c")
        })
        .unwrap();

        db.delete_intelligence_node("b").unwrap();

        let from_a = db.walk_intelligence_graph("a", 3).unwrap().unwrap();
        assert!(!from_a.nodes.iter().any(|n| n.id == "b"));
        assert!(!from_a
            .edges
            .iter()
            .any(|e| e.from_node_id == "b" || e.to_node_id == "b"));

        let from_c = db.walk_intelligence_graph("c", 3).unwrap().unwrap();
        assert!(!from_c.nodes.iter().any(|n| n.id == "b"));
        assert!(!from_c
            .edges
            .iter()
            .any(|e| e.from_node_id == "b" || e.to_node_id == "b"));
    }

    #[test]
    fn delete_intelligence_edge_removes_single_relation_only() {
        let db = test_db();
        db.upsert_intelligence_node(sample_node_input("a")).unwrap();
        db.upsert_intelligence_node(IntelligenceNodeInput {
            relations: Some(vec![IntelligenceRelationInput {
                to_node_id: "a".to_string(),
                relation: "supports".to_string(),
                weight: None,
            }]),
            ..sample_node_input("b")
        })
        .unwrap();

        let edges = db.list_recent_intelligence_edges("a").unwrap();
        assert_eq!(edges.len(), 1);
        let edge_id = edges[0].id;

        let removed = db.delete_intelligence_edge(edge_id).unwrap();
        assert!(removed);
        assert!(db.get_intelligence_edge(edge_id).unwrap().is_none());
        assert!(db.get_intelligence_node("a").unwrap().is_some());
        assert!(db.get_intelligence_node("b").unwrap().is_some());
    }

    #[test]
    fn delete_intelligence_edge_missing_returns_false() {
        let db = test_db();
        let removed = db.delete_intelligence_edge(999999).unwrap();
        assert!(!removed);
    }

    #[test]
    fn list_intelligence_projects_empty() {
        let db = test_db();
        let projects = db.list_intelligence_projects(None, 100).unwrap();
        assert!(projects.is_empty());
    }

    #[test]
    fn list_intelligence_projects_with_projects() {
        let db = test_db();
        let mut input1 = sample_node_input("proj1");
        input1.kind = "project".to_string();
        let mut input2 = sample_node_input("proj2");
        input2.kind = "project".to_string();
        db.upsert_intelligence_node(input1).unwrap();
        db.upsert_intelligence_node(input2).unwrap();

        let projects = db.list_intelligence_projects(None, 100).unwrap();
        assert_eq!(projects.len(), 2);
    }

    #[test]
    fn resolve_prefix_unique_match() {
        let db = test_db();
        let full = "3a476c63-6b4a-4860-9c18-1c784b40a4b2";
        // A decoy that shares the first six characters but diverges inside the
        // 8-char prefix — the LIKE must actually discriminate, not just return
        // the only row in the table.
        for id in [full, "3a476c99-dead-4860-9c18-1c784b40a4b2"] {
            db.upsert_intelligence_node(IntelligenceNodeInput {
                id: Some(id.to_string()),
                kind: "fact".to_string(),
                title: "Original".to_string(),
                body: "body".to_string(),
                metadata: None,
                project_hash: Some("proj-a".to_string()),
                session_id: None,
                relations: None,
            })
            .unwrap();
        }
        let resolved = db.resolve_node_id_by_prefix("3a476c63").unwrap();
        assert_eq!(resolved, Some(full.to_string()));
    }

    #[test]
    fn resolve_prefix_ambiguous() {
        let db = test_db();
        let id1 = "abc123-0000-0000-0000-000000000001";
        let id2 = "abc456-0000-0000-0000-000000000002";
        for id in [id1, id2] {
            db.upsert_intelligence_node(IntelligenceNodeInput {
                id: Some(id.to_string()),
                kind: "fact".to_string(),
                title: format!("Node {id}"),
                body: "body".to_string(),
                metadata: None,
                project_hash: Some("proj-a".to_string()),
                session_id: None,
                relations: None,
            })
            .unwrap();
        }
        let err = db.resolve_node_id_by_prefix("abc").unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("Ambiguous"),
            "expected ambiguous error, got: {msg}"
        );
        assert!(msg.contains(id1), "expected candidate {id1} in: {msg}");
        assert!(msg.contains(id2), "expected candidate {id2} in: {msg}");
    }

    #[test]
    fn resolve_prefix_no_match() {
        let db = test_db();
        db.upsert_intelligence_node(IntelligenceNodeInput {
            id: Some("xyz-0000-0000-0000-000000000001".to_string()),
            kind: "fact".to_string(),
            title: "X".to_string(),
            body: "body".to_string(),
            metadata: None,
            project_hash: Some("proj-a".to_string()),
            session_id: None,
            relations: None,
        })
        .unwrap();
        let resolved = db.resolve_node_id_by_prefix("abc").unwrap();
        assert_eq!(resolved, None);
    }

    #[test]
    fn resolve_prefix_treats_like_wildcards_literally() {
        let db = test_db();
        // Two ids that differ only at position 2. A naive `LIKE prefix || '%'`
        // where `prefix` contains '_' would match BOTH and raise a bogus
        // "ambiguous" error; escaped, "ab_cd" matches neither.
        for id in [
            "ab1cd-0000-0000-0000-000000000001",
            "ab2cd-0000-0000-0000-000000000002",
        ] {
            db.upsert_intelligence_node(IntelligenceNodeInput {
                id: Some(id.to_string()),
                kind: "fact".to_string(),
                title: "N".to_string(),
                body: "body".to_string(),
                metadata: None,
                project_hash: Some("proj-a".to_string()),
                session_id: None,
                relations: None,
            })
            .unwrap();
        }
        let resolved = db.resolve_node_id_by_prefix("ab_cd").unwrap();
        assert_eq!(resolved, None, "'_' must be a literal, not a wildcard");
    }

    #[test]
    fn upsert_returns_created_true_for_new_node() {
        let db = test_db();
        let (_record, created) = db
            .upsert_intelligence_node(IntelligenceNodeInput {
                id: Some("new-node-1".to_string()),
                kind: "fact".to_string(),
                title: "First".to_string(),
                body: "body".to_string(),
                metadata: None,
                project_hash: Some("proj-a".to_string()),
                session_id: None,
                relations: None,
            })
            .unwrap();
        assert!(created, "expected created=true for new node");
    }

    #[test]
    fn upsert_returns_created_false_for_existing_node() {
        let db = test_db();
        let id = "existing-node-1";
        db.upsert_intelligence_node(IntelligenceNodeInput {
            id: Some(id.to_string()),
            kind: "fact".to_string(),
            title: "First".to_string(),
            body: "body".to_string(),
            metadata: None,
            project_hash: Some("proj-a".to_string()),
            session_id: None,
            relations: None,
        })
        .unwrap();
        let (record, created) = db
            .upsert_intelligence_node(IntelligenceNodeInput {
                id: Some(id.to_string()),
                kind: "fact".to_string(),
                title: "Updated".to_string(),
                body: "body2".to_string(),
                metadata: None,
                project_hash: Some("proj-a".to_string()),
                session_id: None,
                relations: None,
            })
            .unwrap();
        assert!(!created, "expected created=false for update");
        assert_eq!(record.title, "Updated");
    }

    #[test]
    fn intelligence_upsert_with_prefix_updates_existing() {
        let db = test_db();
        let full = "3a476c63-6b4a-4860-9c18-1c784b40a4b2";
        db.upsert_intelligence_node(IntelligenceNodeInput {
            id: Some(full.to_string()),
            kind: "fact".to_string(),
            title: "Original".to_string(),
            body: "body".to_string(),
            metadata: None,
            project_hash: Some("proj-a".to_string()),
            session_id: None,
            relations: None,
        })
        .unwrap();
        // Simulate handler prefix resolution: 8-char prefix should resolve to full.
        let resolved = db.resolve_node_id_by_prefix("3a476c63").unwrap().unwrap();
        assert_eq!(resolved, full);
        let (record, created) = db
            .upsert_intelligence_node(IntelligenceNodeInput {
                id: Some(resolved),
                kind: "fact".to_string(),
                title: "Updated via prefix".to_string(),
                body: "body2".to_string(),
                metadata: None,
                project_hash: Some("proj-a".to_string()),
                session_id: None,
                relations: None,
            })
            .unwrap();
        assert!(!created, "prefix upsert should be update, not create");
        assert_eq!(record.id, full);
        assert_eq!(record.title, "Updated via prefix");
        // Ensure no duplicate was created.
        let all = db.list_intelligence_nodes(None, 100).unwrap();
        assert_eq!(all.len(), 1);
    }

    #[test]
    fn intelligence_upsert_with_ambiguous_prefix_errors() {
        let db = test_db();
        let id1 = "abc123-0000-0000-0000-000000000001";
        let id2 = "abc456-0000-0000-0000-000000000002";
        for id in [id1, id2] {
            db.upsert_intelligence_node(IntelligenceNodeInput {
                id: Some(id.to_string()),
                kind: "fact".to_string(),
                title: format!("Node {id}"),
                body: "body".to_string(),
                metadata: None,
                project_hash: Some("proj-a".to_string()),
                session_id: None,
                relations: None,
            })
            .unwrap();
        }
        let err = db.resolve_node_id_by_prefix("abc").unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("Ambiguous"));
        assert!(msg.contains(id1));
        assert!(msg.contains(id2));
    }

    #[test]
    fn intelligence_graph_walk_with_prefix() {
        let db = test_db();
        let full = "deadbeef-0000-0000-0000-000000000001";
        db.upsert_intelligence_node(IntelligenceNodeInput {
            id: Some(full.to_string()),
            kind: "fact".to_string(),
            title: "Root".to_string(),
            body: "body".to_string(),
            metadata: None,
            project_hash: Some("proj-a".to_string()),
            session_id: None,
            relations: None,
        })
        .unwrap();
        let resolved = db.resolve_node_id_by_prefix("deadbeef").unwrap().unwrap();
        let walk = db.walk_intelligence_graph(&resolved, 1).unwrap().unwrap();
        assert_eq!(walk.root.id, full);
    }

    #[test]
    fn intelligence_delete_node_with_prefix() {
        let db = test_db();
        let full = "feedface-0000-0000-0000-000000000001";
        db.upsert_intelligence_node(IntelligenceNodeInput {
            id: Some(full.to_string()),
            kind: "fact".to_string(),
            title: "To delete".to_string(),
            body: "body".to_string(),
            metadata: None,
            project_hash: Some("proj-a".to_string()),
            session_id: None,
            relations: None,
        })
        .unwrap();
        let resolved = db.resolve_node_id_by_prefix("feedface").unwrap().unwrap();
        let removed = db.delete_intelligence_node(&resolved).unwrap();
        assert_eq!(removed, Some(0));
        assert!(db.get_intelligence_node(full).unwrap().is_none());
    }
}
