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
    pub status: String,
    pub title: String,
    pub body: String,
    pub metadata: Option<String>,
    pub project_hash: Option<String>,
    pub session_id: Option<String>,
    pub created_at: i64,
    pub updated_at: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OperationalSessionRecord {
    pub id: String,
    pub title: String,
    pub body: String,
    pub metadata: Option<String>,
    pub project_hash: Option<String>,
    pub session_id: Option<String>,
    pub created_at: i64,
    pub updated_at: i64,
}

impl OperationalSessionRecord {
    pub fn into_intelligence_node_record(self) -> IntelligenceNodeRecord {
        IntelligenceNodeRecord {
            id: self.id,
            kind: "session".to_string(),
            status: "noted".to_string(),
            title: self.title,
            body: self.body,
            metadata: self.metadata,
            project_hash: self.project_hash,
            session_id: self.session_id,
            created_at: self.created_at,
            updated_at: self.updated_at,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct OperationalSessionInput {
    pub id: Option<String>,
    pub title: String,
    pub body: String,
    pub metadata: Option<serde_json::Value>,
    pub project_hash: Option<String>,
    pub session_id: Option<String>,
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
    pub status: Option<String>,
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

/// One project reached by [`Database::traverse_project_scope`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TraversedProject {
    pub hash: String,
    /// Relation of the edge that reached this project; `None` for the root.
    pub via_relation: Option<String>,
    /// BFS depth from the root (root = 0).
    pub depth: usize,
}

/// Bounded project-scope traversal result.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TraversalScope {
    pub projects: Vec<TraversedProject>,
    /// Number of projects reached excluding the root.
    pub reached: usize,
}

/// Hard bound on traversal depth: one query never walks further than this.
pub const MAX_TRAVERSAL_DEPTH: usize = 5;

/// Knowledge-node kinds. `project` is structural (CM9) and exempt from this validation.
pub const KNOWLEDGE_KINDS: &[&str] = &["fact", "pattern", "idea", "decision", "defect"];

/// Node status — applies to all nodes (knowledge + project).
pub const NODE_STATUSES: &[&str] = &["noted", "verified", "resolved", "superseded", "deprecated"];

/// Edge relation types for knowledge-node edges.
/// Project edges use their own vocabulary (`PROJECT_RELATIONS` in `domain/project.rs`).
pub const KNOWLEDGE_EDGE_RELATIONS: &[&str] = &[
    "supersedes",
    "contradicts",
    "evidence_for",
    "depends_on",
    "extends",
    "part_of",
];

/// Minimum lexical search score for a same-kind node to count as a
/// duplicate candidate at write time. A `pub const` so it can be tuned
/// without touching the query; a config-file setting can come later.
pub const DUPLICATE_SIMILARITY_THRESHOLD: i64 = 5;

/// Lexical duplicate detection does not catch paraphrase and does not
/// cross languages — surfaced in the upsert output wherever candidates
/// are reported.
pub const LEXICAL_DUPLICATE_LIMITATION: &str = "Lexical duplicate detection does not catch paraphrase and does not cross languages, so a bilingual corpus will pass duplicates through.";

pub fn validate_knowledge_kind(kind: &str) -> Result<()> {
    if kind == "project" || KNOWLEDGE_KINDS.contains(&kind) {
        Ok(())
    } else {
        anyhow::bail!(
            "unknown node kind '{kind}'; allowed: fact, pattern, idea, decision, defect (structural: project)"
        )
    }
}

pub fn validate_node_status(status: &str) -> Result<()> {
    if NODE_STATUSES.contains(&status) {
        Ok(())
    } else {
        anyhow::bail!(
            "unknown node status '{status}'; allowed: noted, verified, resolved, superseded, deprecated"
        )
    }
}

pub fn validate_knowledge_edge_relation(relation: &str) -> Result<()> {
    if KNOWLEDGE_EDGE_RELATIONS.contains(&relation) {
        Ok(())
    } else {
        anyhow::bail!(
            "unknown edge relation '{relation}'; allowed: {}",
            KNOWLEDGE_EDGE_RELATIONS.join(", ")
        )
    }
}

/// `file:line` citations in a body (`src/foo.rs:42`). Word-ish path chars
/// plus a colon plus a line number; the trailing boundary keeps `a:b`
/// prose and clock times (`12:30`) mostly out.
fn extract_citations(body: &str) -> Vec<String> {
    let re = regex::Regex::new(r"(?x)\b([A-Za-z0-9_./-]*[A-Za-z0-9_-]:\d+)\b")
        .expect("citation regex is static and valid");
    let mut out: Vec<String> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    for cap in re.captures_iter(body) {
        let citation = cap[1].to_string();
        if seen.insert(citation.clone()) {
            out.push(citation);
        }
    }
    out
}

/// Mirror of the ranked-search weighting (title hit = 3, body hit = 1)
/// over the upsert query terms. A candidate at or above
/// [`DUPLICATE_SIMILARITY_THRESHOLD`] is reported.
fn lexical_duplicate_score(query_terms: &[String], candidate: &IntelligenceNodeRecord) -> i64 {
    let title = candidate.title.to_lowercase();
    let body = candidate.body.to_lowercase();
    let mut score = 0i64;
    for term in query_terms {
        if title.contains(term) {
            score += 3;
        }
        if body.contains(term) {
            score += 1;
        }
    }
    score
}

/// Node-id-shaped references in a body: `[[wiki-links]]`, full UUIDs, and
/// 8+ char hex strings (short citations like `→ f9f487d9`, `Ver 74bdcdd2`).
fn extract_node_id_references(body: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    let mut push = |candidate: String| {
        if !candidate.is_empty() && seen.insert(candidate.clone()) {
            out.push(candidate);
        }
    };

    let wiki =
        regex::Regex::new(r"\[\[([^\[\]]+)\]\]").expect("wiki-link regex is static and valid");
    for cap in wiki.captures_iter(body) {
        push(cap[1].trim().to_string());
    }
    let uuid = regex::Regex::new(
        r"\b([0-9a-fA-F]{8}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{12})\b",
    )
    .expect("uuid regex is static and valid");
    for cap in uuid.captures_iter(body) {
        push(cap[1].to_string());
    }
    let hex = regex::Regex::new(r"\b([0-9a-fA-F]{8,64})\b").expect("hex regex is static and valid");
    for cap in hex.captures_iter(body) {
        let token = cap[1].to_string();
        // A pure decimal run is a number, not a node id.
        if token.chars().all(|c| c.is_ascii_digit()) {
            continue;
        }
        push(token);
    }
    out
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
    ) -> Result<(
        IntelligenceNodeRecord,
        bool,
        Vec<IntelligenceNodeRecord>,
        Vec<String>,
    )> {
        // Destructure by value: every field is consumed below, so the
        // input stays pass-by-value without tripping needless_pass_by_value.
        let IntelligenceNodeInput {
            id,
            kind,
            status: input_status,
            title,
            body,
            metadata,
            project_hash,
            session_id,
            relations,
        } = input;
        // 1. Kind is validated first: cheapest check, most likely mistake.
        validate_knowledge_kind(&kind)?;

        // 2. A provided status must be a known value. `None` resolves below
        // (create → "noted", update → preserve existing).
        if let Some(ref s) = input_status {
            validate_node_status(s)?;
        }

        // 3. New edges must use the knowledge-edge vocabulary. Existing
        // stored edges are NOT validated retroactively: rejecting them on
        // read would break every graph walk over the old corpus.
        if let Some(ref rels) = relations {
            for rel in rels {
                validate_knowledge_edge_relation(&rel.relation)?;
            }
        }

        let node_id = id.unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
        let now = Utc::now().timestamp();
        let metadata = metadata.map(|value| value.to_string());
        let conn = self
            .conn
            .lock()
            .map_err(|e| anyhow!("Lock poisoned: {}", e))?;
        // Determine creation vs update before the INSERT — ON CONFLICT would
        // otherwise hide the distinction. For auto-generated ids this is always
        // creation. Done inside the same lock as the INSERT to avoid a race
        // where another thread inserts between the check and the insert.
        // The existing status is read here too so an update that omits
        // `status` preserves it without a second lock acquisition.
        let existing_status: Option<String> = {
            let mut stmt =
                conn.prepare("SELECT status FROM intelligence_nodes WHERE id = ?1 LIMIT 1")?;
            let mut rows = stmt.query(rusqlite::params![&node_id])?;
            match rows.next()? {
                Some(row) => Some(row.get(0)?),
                None => None,
            }
        };
        let was_created = existing_status.is_none();

        let status = match input_status {
            Some(s) => s,
            None => existing_status.unwrap_or_else(|| "noted".to_string()),
        };

        // 4. `superseded` is meaningless without the pointer to what holds
        // instead: it requires a `supersedes` edge in the same write.
        if status == "superseded" {
            let has_supersedes = relations
                .as_ref()
                .map(|rels| rels.iter().any(|r| r.relation == "supersedes"))
                .unwrap_or(false);
            if !has_supersedes {
                anyhow::bail!(
                    "status 'superseded' requires a 'supersedes' edge pointing to the replacing node"
                );
            }
        }

        conn.execute(
            "INSERT INTO intelligence_nodes (
                id, kind, status, title, body, metadata, project_hash, session_id, created_at, updated_at
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)
            ON CONFLICT(id) DO UPDATE SET
                kind = excluded.kind,
                status = excluded.status,
                title = excluded.title,
                body = excluded.body,
                metadata = excluded.metadata,
                project_hash = excluded.project_hash,
                session_id = excluded.session_id,
                updated_at = excluded.updated_at",
            rusqlite::params![
                node_id,
                kind,
                status,
                title,
                body,
                metadata,
                project_hash,
                session_id,
                now,
                now
            ],
        )?;

        conn.execute(
            "DELETE FROM intelligence_edges WHERE from_node_id = ?1",
            rusqlite::params![&node_id],
        )?;

        if let Some(relations) = relations {
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
        // Write-time signals, computed after the write lands: duplicate
        // candidates (shared citations, then lexical ranking) and references
        // to existing nodes that were mentioned but not declared as edges.
        let duplicates = self.detect_duplicate_candidates(&record);
        let undeclared_references = self.extract_undeclared_references(&record)?;
        Ok((record, was_created, duplicates, undeclared_references))
    }

    /// Write-time duplicate candidates for `node`, strongest signal first.
    ///
    /// 1. **Shared citations** — same-kind nodes citing the same `file:line`
    ///    or the same node ids. Deterministic and precise for this corpus.
    /// 2. **Lexical ranking** — same-kind nodes scoring at or above
    ///    [`DUPLICATE_SIMILARITY_THRESHOLD`] against the node's title plus
    ///    the first ~100 chars of its body.
    ///
    /// Candidates are a warning, never a rejection. Best-effort: any
    /// internal lookup failure yields fewer candidates, not a failed write.
    fn detect_duplicate_candidates(
        &self,
        node: &IntelligenceNodeRecord,
    ) -> Vec<IntelligenceNodeRecord> {
        let mut seen: HashSet<String> = HashSet::from([node.id.clone()]);
        let mut candidates: Vec<IntelligenceNodeRecord> = Vec::new();

        // Signal 1: shared citations. This includes both file:line citations
        // and references to other knowledge-node ids.
        let mut shared_references = extract_citations(&node.body);
        shared_references.extend(extract_node_id_references(&node.body));
        for citation in shared_references {
            if let Ok(neighbours) = self.list_nodes_citing(&citation, &node.kind, &node.id) {
                for neighbour in neighbours {
                    if seen.insert(neighbour.id.clone()) {
                        candidates.push(neighbour);
                    }
                }
            }
        }

        // Signal 2: lexical ranking over title + body head, same kind only.
        let body_head: String = node.body.chars().take(100).collect();
        let query = format!("{} {body_head}", node.title);
        if let Ok(results) = self.search_intelligence_nodes(&query, Some(&node.kind), 10) {
            let terms: Vec<String> = query.split_whitespace().map(str::to_lowercase).collect();
            for hit in results.results {
                if !seen.insert(hit.id.clone()) {
                    continue;
                }
                if lexical_duplicate_score(&terms, &hit) >= DUPLICATE_SIMILARITY_THRESHOLD {
                    candidates.push(hit);
                }
            }
        }

        candidates
    }

    /// Same-kind nodes (excluding `exclude_id`) whose body mentions
    /// `citation`. Backs signal 1 of [`Database::detect_duplicate_candidates`].
    fn list_nodes_citing(
        &self,
        citation: &str,
        kind: &str,
        exclude_id: &str,
    ) -> Result<Vec<IntelligenceNodeRecord>> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| anyhow!("Lock poisoned: {}", e))?;
        let mut stmt = conn.prepare(
            "SELECT id, kind, status, title, body, metadata, project_hash, session_id, created_at, updated_at
             FROM intelligence_nodes
             WHERE kind = ?1 AND id != ?2 AND instr(body, ?3) > 0
             LIMIT 10",
        )?;
        let rows = stmt.query_map(
            rusqlite::params![kind, exclude_id, citation],
            Self::read_intelligence_node,
        )?;
        Ok(rows.filter_map(|row| row.ok()).collect())
    }

    /// Ids of existing nodes mentioned in `node`'s body that are not
    /// declared as outgoing edges. Returns the extracted candidates so
    /// declaring them costs nothing. Empty when the node has no edges at
    /// all — a generic orphan warning fires constantly and gets ignored.
    fn extract_undeclared_references(&self, node: &IntelligenceNodeRecord) -> Result<Vec<String>> {
        let declared: HashSet<String> = self
            .list_recent_intelligence_edges(&node.id)?
            .into_iter()
            .filter(|edge| edge.from_node_id == node.id)
            .map(|e| e.to_node_id)
            .collect();
        let mut refs: Vec<String> = Vec::new();
        let mut seen: HashSet<String> = HashSet::new();
        let mut reported: HashSet<String> = HashSet::new();
        for candidate in extract_node_id_references(&node.body) {
            if candidate == node.id || !seen.insert(candidate.clone()) {
                continue;
            }
            if declared.contains(&candidate) {
                continue;
            }
            // Only warn about references to nodes that actually exist —
            // anything else is prose, not a missing edge. Prefixes resolve
            // so short citations (`→ f9f487d9`) still match. Ambiguous or
            // absent prefixes are skipped: a warning must never fail a write.
            let Ok(Some(resolved)) = self.resolve_node_id_by_prefix(&candidate) else {
                continue;
            };
            if resolved == node.id || declared.contains(&resolved) {
                continue;
            }
            if reported.insert(resolved.clone()) {
                refs.push(resolved);
            }
        }
        Ok(refs)
    }

    pub fn get_intelligence_node(&self, id: &str) -> Result<Option<IntelligenceNodeRecord>> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| anyhow!("Lock poisoned: {}", e))?;
        let mut stmt = conn.prepare(
            "SELECT id, kind, status, title, body, metadata, project_hash, session_id, created_at, updated_at
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
                "SELECT id, kind, status, title, body, metadata, project_hash, session_id, created_at, updated_at
                 FROM intelligence_nodes
                 WHERE kind = ?1
                 ORDER BY updated_at DESC
                 LIMIT ?2",
            )?
        } else {
            conn.prepare(
                "SELECT id, kind, status, title, body, metadata, project_hash, session_id, created_at, updated_at
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
            "SELECT id, kind, status, title, body, metadata, project_hash, session_id, created_at, updated_at, \
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
            status: row.get(2)?,
            title: row.get(3)?,
            body: row.get(4)?,
            metadata: row.get(5)?,
            project_hash: row.get(6)?,
            session_id: row.get(7)?,
            created_at: row.get(8)?,
            updated_at: row.get(9)?,
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
            status: None,
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
        .map(|(record, _created, _duplicates, _undeclared)| record)
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
            "SELECT id, kind, status, title, body, metadata, project_hash, session_id, created_at, updated_at
             FROM intelligence_nodes
             WHERE kind = 'project'
               AND (instr(lower(title), ?1) > 0 OR instr(lower(body), ?1) > 0 OR instr(lower(coalesce(metadata, '')), ?1) > 0)
             ORDER BY updated_at DESC
             LIMIT ?2"
        } else {
            "SELECT id, kind, status, title, body, metadata, project_hash, session_id, created_at, updated_at
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
        crate::domain::project::validate_project_relation(relation)?;
        if relation == "contains" {
            anyhow::bail!(
                "relation 'contains' is derived from registry paths — it cannot be hand-linked"
            );
        }
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
            "SELECT id, kind, status, title, body, metadata, project_hash, session_id, created_at, updated_at
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

    /// Recompute derived `contains` (parent → child) edges from registry paths.
    ///
    /// Deletes all existing `contains` edges between project nodes, then links
    /// each project to its deepest container (component-wise path prefix via
    /// `Path::starts_with` — never a string prefix). Pure recompute: hand
    /// edits never survive, and only the direct parent is stored (transitivity
    /// is recovered via the depth parameter at read time). Returns edges inserted.
    pub fn rebuild_containment_edges(&self) -> Result<usize> {
        {
            let conn = self
                .conn
                .lock()
                .map_err(|e| anyhow!("Lock poisoned: {}", e))?;
            conn.execute(
                "DELETE FROM intelligence_edges WHERE relation = 'contains'
                 AND from_node_id IN (SELECT id FROM intelligence_nodes WHERE kind = 'project')
                 AND to_node_id IN (SELECT id FROM intelligence_nodes WHERE kind = 'project')",
                [],
            )?;
        }

        let projects = self.list_projects()?;
        let mut pairs: Vec<(String, String)> = Vec::new();
        for child in &projects {
            let child_path = std::path::Path::new(&child.path);
            let mut best: Option<&crate::domain::project::Project> = None;
            let mut best_depth = 0usize;
            for parent in &projects {
                if parent.hash == child.hash {
                    continue;
                }
                let parent_path = std::path::Path::new(&parent.path);
                if child_path.starts_with(parent_path) {
                    let depth = parent_path.components().count();
                    if depth > best_depth {
                        best_depth = depth;
                        best = Some(parent);
                    }
                }
            }
            if let Some(parent) = best {
                pairs.push((parent.hash.clone(), child.hash.clone()));
            }
        }

        let mut inserted = 0usize;
        for (parent_hash, child_hash) in pairs {
            let from_node = self.find_project_node(&parent_hash)?;
            let to_node = self.find_project_node(&child_hash)?;
            let (Some(from_node), Some(to_node)) = (from_node, to_node) else {
                continue;
            };
            let conn = self
                .conn
                .lock()
                .map_err(|e| anyhow!("Lock poisoned: {}", e))?;
            let exists: bool = conn.query_row(
                "SELECT EXISTS(SELECT 1 FROM intelligence_edges WHERE from_node_id = ?1 AND to_node_id = ?2 AND relation = 'contains')",
                rusqlite::params![from_node.id, to_node.id],
                |row| row.get::<_, i64>(0),
            )? != 0;
            if exists {
                continue;
            }
            let now = Utc::now().timestamp();
            conn.execute(
                "INSERT INTO intelligence_edges (from_node_id, to_node_id, relation, weight, created_at)
                 VALUES (?1, ?2, 'contains', 1.0, ?3)",
                rusqlite::params![from_node.id, to_node.id, now],
            )?;
            inserted += 1;
        }
        Ok(inserted)
    }

    /// BFS project-scope traversal from `root_hash` up to `depth` hops.
    ///
    /// Follows outbound edges for `depends_on`, `extends`, `publishes`,
    /// `relates_to`; `complements` in both directions; `contains` in both
    /// directions (reverse = inherited-upward rule). Inbound `depends_on`
    /// is never followed (see [`Database::list_project_dependents`]).
    /// Depth is clamped to `0..=MAX_TRAVERSAL_DEPTH`.
    pub fn traverse_project_scope(&self, root_hash: &str, depth: usize) -> Result<TraversalScope> {
        let depth = depth.min(MAX_TRAVERSAL_DEPTH);
        let mut projects = vec![TraversedProject {
            hash: root_hash.to_string(),
            via_relation: None,
            depth: 0,
        }];
        if depth == 0 {
            return Ok(TraversalScope {
                projects,
                reached: 0,
            });
        }
        let Some(root_node) = self.find_project_node(root_hash)? else {
            return Ok(TraversalScope {
                projects,
                reached: 0,
            });
        };

        // Load project↔project edges once; BFS in Rust over node ids.
        let edges: Vec<IntelligenceEdgeRecord> = {
            let conn = self
                .conn
                .lock()
                .map_err(|e| anyhow!("Lock poisoned: {}", e))?;
            let mut stmt = conn.prepare(
                "SELECT e.id, e.from_node_id, e.to_node_id, e.relation, e.weight, e.created_at
                 FROM intelligence_edges e
                 JOIN intelligence_nodes a ON a.id = e.from_node_id
                 JOIN intelligence_nodes b ON b.id = e.to_node_id
                 WHERE a.kind = 'project' AND b.kind = 'project'",
            )?;
            let rows: Vec<IntelligenceEdgeRecord> = stmt
                .query_map([], Self::read_intelligence_edge)?
                .filter_map(|r| r.ok())
                .collect();
            rows
        };
        // Node id → project hash for reached nodes.
        let node_hash: std::collections::HashMap<String, String> = {
            let conn = self
                .conn
                .lock()
                .map_err(|e| anyhow!("Lock poisoned: {}", e))?;
            let mut stmt = conn.prepare(
                "SELECT id, project_hash FROM intelligence_nodes WHERE kind = 'project'",
            )?;
            let mut map = std::collections::HashMap::new();
            let rows = stmt.query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?))
            })?;
            for row in rows.filter_map(|r| r.ok()) {
                if let Some(hash) = row.1 {
                    map.insert(row.0, hash);
                }
            }
            map
        };

        let mut visited: HashSet<String> = HashSet::from([root_node.id.clone()]);
        // (node_id, depth) frontier; root depth = 0.
        let mut frontier: Vec<(String, usize)> = vec![(root_node.id, 0)];
        for _ in 0..depth {
            let mut next_frontier: Vec<(String, usize)> = Vec::new();
            for (current, cur_depth) in &frontier {
                for edge in &edges {
                    let neighbor: Option<(&str, &str)> = if edge.from_node_id == *current {
                        // Outbound: all relations followed.
                        Some((edge.to_node_id.as_str(), edge.relation.as_str()))
                    } else if edge.to_node_id == *current
                        && (edge.relation == "complements" || edge.relation == "contains")
                    {
                        // Reverse: only symmetric complements + upward contains.
                        Some((edge.from_node_id.as_str(), edge.relation.as_str()))
                    } else {
                        None
                    };
                    if let Some((neighbor_id, relation)) = neighbor {
                        if visited.insert(neighbor_id.to_string()) {
                            if let Some(hash) = node_hash.get(neighbor_id) {
                                projects.push(TraversedProject {
                                    hash: hash.clone(),
                                    via_relation: Some(relation.to_string()),
                                    depth: cur_depth + 1,
                                });
                            }
                            next_frontier.push((neighbor_id.to_string(), cur_depth + 1));
                        }
                    }
                }
            }
            if next_frontier.is_empty() {
                break;
            }
            frontier = next_frontier;
        }
        let reached = projects.len().saturating_sub(1);
        Ok(TraversalScope { projects, reached })
    }

    /// Impact warning source: projects that declare `depends_on` ON `hash`.
    /// Inbound dependents never pull context — they only warn.
    pub fn list_project_dependents(
        &self,
        hash: &str,
    ) -> Result<Vec<IntelligenceProjectDependencyRecord>> {
        let Some(node) = self.find_project_node(hash)? else {
            return Ok(Vec::new());
        };
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
             WHERE e.to_node_id = ?1 AND e.relation = 'depends_on'
             ORDER BY e.created_at DESC",
        )?;
        let rows = stmt.query_map(
            rusqlite::params![node.id],
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

    /// Scoped variant of [`Database::search_intelligence_nodes`]: same ranking
    /// SQL, plus `AND project_hash IN (...)`. Empty `hashes` yields an empty
    /// result (with the examined count). The unscoped original is untouched.
    pub fn search_intelligence_nodes_scoped(
        &self,
        query: &str,
        kind: Option<&str>,
        limit: usize,
        hashes: &[String],
    ) -> Result<IntelligenceSearchResult> {
        let terms: Vec<String> = query.split_whitespace().map(str::to_lowercase).collect();
        let examined_count = self.count_intelligence_nodes_for_search(kind)?;

        if terms.is_empty() || hashes.is_empty() {
            return Ok(IntelligenceSearchResult {
                results: Vec::new(),
                examined_count,
            });
        }

        let conn = self
            .conn
            .lock()
            .map_err(|e| anyhow!("Lock poisoned: {}", e))?;

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
        // project_hash placeholders shift with the term count.
        let hash_start = terms.len() + 3;
        let hash_placeholders: Vec<String> = (0..hashes.len())
            .map(|i| format!("?{}", hash_start + i))
            .collect();
        let hash_clause = hash_placeholders.join(", ");
        let sql = format!(
            "SELECT id, kind, status, title, body, metadata, project_hash, session_id, created_at, updated_at, \
             ({match_count_expr}) AS match_count, ({score_expr}) AS score \
             FROM intelligence_nodes \
             WHERE (?1 IS NULL OR kind = ?1) AND project_hash IN ({hash_clause}) AND ({or_clause}) \
             ORDER BY match_count DESC, score DESC, updated_at DESC \
             LIMIT ?{limit_placeholder}"
        );

        let mut stmt = conn.prepare(&sql)?;
        // Placeholder index order: ?1 kind, ?2.. terms, ?{limit} limit,
        // then hash IN-list placeholders (rusqlite binds by index).
        let mut params: Vec<Box<dyn rusqlite::ToSql>> =
            Vec::with_capacity(terms.len() + hashes.len() + 2);
        params.push(Box::new(kind.map(str::to_string)));
        for term in &terms {
            params.push(Box::new(term.clone()));
        }
        params.push(Box::new(limit as i64));
        for hash in hashes {
            params.push(Box::new(hash.clone()));
        }
        let param_refs: Vec<&dyn rusqlite::ToSql> = params.iter().map(Box::as_ref).collect();

        let rows = stmt.query_map(param_refs.as_slice(), Self::read_intelligence_node)?;
        let results: Vec<IntelligenceNodeRecord> = rows.filter_map(|row| row.ok()).collect();
        Ok(IntelligenceSearchResult {
            results,
            examined_count,
        })
    }

    /// Retype the two backlog-note nodes (`d8c3230b`, `508e398c`) from
    /// `kind='project'` to `kind='fact'`. Skips absent or already-fixed nodes.
    /// Refuses (Err) if a target looks like a real registry node
    /// (`metadata.source == "registry"`) — retyping that would corrupt the graph.
    pub fn retype_backlog_project_nodes(&self) -> Result<usize> {
        let mut retyped = 0usize;
        for prefix in ["d8c3230b", "508e398c"] {
            let Some(id) = self.resolve_node_id_by_prefix(prefix)? else {
                tracing::warn!("cm9 retype: no node matching prefix '{prefix}', skipping");
                continue;
            };
            let Some(node) = self.get_intelligence_node(&id)? else {
                continue;
            };
            if node.kind != "project" {
                continue;
            }
            if let Some(meta) = node.metadata.as_deref() {
                if let Ok(value) = serde_json::from_str::<serde_json::Value>(meta) {
                    if value.get("source").and_then(|s| s.as_str()) == Some("registry") {
                        anyhow::bail!(
                            "refusing to retype node '{id}': it is a registry project node, not a backlog note"
                        );
                    }
                }
            }
            let conn = self
                .conn
                .lock()
                .map_err(|e| anyhow!("Lock poisoned: {}", e))?;
            conn.execute(
                "UPDATE intelligence_nodes SET kind = 'fact' WHERE id = ?1",
                rusqlite::params![id],
            )?;
            retyped += 1;
        }
        Ok(retyped)
    }

    /// Seed the four known cross-project `depends_on` edges (best-effort).
    /// Each side resolves by `path.contains(substr)` against registered
    /// projects (falling back to loop/spec workdirs for ticket-tagged
    /// from-sides); missing or ambiguous sides are skipped with a warning and
    /// never fail. `link_projects` dedup makes this idempotent.
    pub fn seed_cm9_project_dependencies(&self) -> Result<usize> {
        // (from_substrs, from_ticket, to_substr): CB3, CX1 x2, CX2, AD11.
        let pairs: &[(&[&str], &str, &str)] = &[
            (&["harness-canopy"], "CB3", "canopy-registry"),
            (&["cx1"], "CX1", "ghscaff"),
            (&["cx1"], "CX1", "demostage"),
            (&["cx2"], "CX2", "univerlab"),
            (&["astro-denoise"], "AD11", "harness-canopy"),
        ];
        let mut created = 0usize;
        for (from_substrs, ticket, to_substr) in pairs {
            let from_hash = self.resolve_seed_project(from_substrs, ticket);
            let to_hash = self.resolve_seed_project(&[to_substr], "");
            match (from_hash, to_hash) {
                (Some(from), Some(to)) => {
                    let before = self.list_related_projects(&from, 1000)?.len();
                    self.link_projects(&from, &to, "depends_on", None)?;
                    let after = self.list_related_projects(&from, 1000)?.len();
                    if after > before {
                        created += 1;
                    }
                }
                (from, to) => {
                    tracing::warn!(
                        "cm9 seed: skipping {ticket} edge (from={from:?}, to={to:?}): side not registered or ambiguous"
                    );
                }
            }
        }
        Ok(created)
    }

    /// Resolve one seed endpoint: first registered project whose path contains
    /// any of `substrs` (exactly one match required); else the workdir of a
    /// loop/spec whose name mentions `ticket`.
    fn resolve_seed_project(&self, substrs: &[&str], ticket: &str) -> Option<String> {
        let projects = self.list_projects().ok()?;
        let mut matches: Vec<String> = Vec::new();
        for project in &projects {
            if substrs.iter().any(|s| project.path.contains(s)) {
                matches.push(project.hash.clone());
            }
        }
        if matches.len() == 1 {
            return Some(matches.into_iter().next().unwrap());
        }
        if !ticket.is_empty() {
            // Fall back to loop/spec workdirs tagged with the ticket id.
            let workdir: Option<String> = {
                let conn = self.conn.lock().ok()?;
                let mut stmt = conn
                    .prepare(
                        "SELECT workdir FROM loop_specs WHERE name LIKE ?1 AND workdir IS NOT NULL LIMIT 1",
                    )
                    .ok()?;
                let pattern = format!("%{ticket}%");
                stmt.query_row(rusqlite::params![pattern], |row| row.get(0))
                    .ok()
            };
            if let Some(workdir) = workdir {
                let hit = projects.into_iter().find(|p| p.path == workdir);
                if let Some(project) = hit {
                    return Some(project.hash);
                }
            }
        }
        None
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
            Some(_k) => "SELECT id, kind, status, title, body, metadata, project_hash, session_id, created_at, updated_at
                        FROM intelligence_nodes
                        WHERE project_hash = ?1 AND kind = ?2
                        ORDER BY updated_at DESC
                        LIMIT ?3",
            None => "SELECT id, kind, status, title, body, metadata, project_hash, session_id, created_at, updated_at
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
                 n.id, n.kind, n.status, n.title, n.body, n.metadata, n.project_hash, n.session_id, n.created_at, n.updated_at,
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
                let raw_from: String = row.get(11)?;
                let raw_to: String = row.get(12)?;
                // Reorient so from_node_id is always the queried project.
                let (oriented_from, oriented_to) = if raw_from == project_id {
                    (raw_from, raw_to)
                } else {
                    (raw_to, raw_from)
                };
                let edge = IntelligenceEdgeRecord {
                    id: row.get(10)?,
                    from_node_id: oriented_from,
                    to_node_id: oriented_to,
                    relation: row.get(13)?,
                    weight: row.get(14)?,
                    created_at: row.get(15)?,
                };
                Ok((node, edge))
            },
        )?;
        Ok(rows.filter_map(|row| row.ok()).collect())
    }

    // ── Operational sessions (CM8) ──

    pub fn upsert_operational_session(
        &self,
        input: OperationalSessionInput,
    ) -> Result<(OperationalSessionRecord, bool)> {
        let node_id = input.id.unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
        let now = Utc::now().timestamp();
        let metadata = input.metadata.map(|value| value.to_string());
        let conn = self
            .conn
            .lock()
            .map_err(|e| anyhow!("Lock poisoned: {}", e))?;
        let was_created: bool = {
            let mut stmt =
                conn.prepare("SELECT 1 FROM operational_sessions WHERE id = ?1 LIMIT 1")?;
            let mut rows = stmt.query(rusqlite::params![&node_id])?;
            rows.next()?.is_none()
        };

        conn.execute(
            "INSERT INTO operational_sessions (
                id, title, body, metadata, project_hash, session_id, created_at, updated_at
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
            ON CONFLICT(id) DO UPDATE SET
                title = excluded.title,
                body = excluded.body,
                metadata = excluded.metadata,
                project_hash = excluded.project_hash,
                session_id = excluded.session_id,
                updated_at = excluded.updated_at",
            rusqlite::params![
                node_id,
                input.title,
                input.body,
                metadata,
                input.project_hash,
                input.session_id,
                now,
                now
            ],
        )?;

        drop(conn);
        let record = self
            .get_operational_session(&node_id)?
            .ok_or_else(|| anyhow!("Failed to load operational session '{}'", node_id))?;
        Ok((record, was_created))
    }

    pub fn get_operational_session(&self, id: &str) -> Result<Option<OperationalSessionRecord>> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| anyhow!("Lock poisoned: {}", e))?;
        let mut stmt = conn.prepare(
            "SELECT id, title, body, metadata, project_hash, session_id, created_at, updated_at
             FROM operational_sessions WHERE id = ?1",
        )?;
        let mut rows = stmt.query(rusqlite::params![id])?;
        if let Some(row) = rows.next()? {
            Ok(Some(Self::read_operational_session(row)?))
        } else {
            Ok(None)
        }
    }

    pub fn list_operational_sessions(&self, limit: usize) -> Result<Vec<OperationalSessionRecord>> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| anyhow!("Lock poisoned: {}", e))?;
        let mut stmt = conn.prepare(
            "SELECT id, title, body, metadata, project_hash, session_id, created_at, updated_at
             FROM operational_sessions
             ORDER BY updated_at DESC
             LIMIT ?1",
        )?;
        let rows = stmt.query_map(
            rusqlite::params![limit as i64],
            Self::read_operational_session,
        )?;
        Ok(rows.filter_map(|row| row.ok()).collect())
    }

    #[allow(dead_code)]
    pub fn list_operational_sessions_by_project(
        &self,
        workdir: &str,
        limit: usize,
    ) -> Result<Vec<OperationalSessionRecord>> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| anyhow!("Lock poisoned: {}", e))?;
        let mut stmt = conn.prepare(
            "SELECT id, title, body, metadata, project_hash, session_id, created_at, updated_at
             FROM operational_sessions
             WHERE project_hash = ?1 OR json_extract(metadata, '$.workdir') = ?1
             ORDER BY updated_at DESC
             LIMIT ?2",
        )?;
        let rows = stmt.query_map(
            rusqlite::params![workdir, limit as i64],
            Self::read_operational_session,
        )?;
        Ok(rows.filter_map(|row| row.ok()).collect())
    }

    pub fn search_operational_sessions(
        &self,
        query: &str,
        limit: usize,
    ) -> Result<Vec<OperationalSessionRecord>> {
        let terms: Vec<String> = query.split_whitespace().map(str::to_lowercase).collect();
        if terms.is_empty() {
            return Ok(Vec::new());
        }
        let conn = self
            .conn
            .lock()
            .map_err(|e| anyhow!("Lock poisoned: {}", e))?;

        let mut match_count_parts: Vec<String> = Vec::with_capacity(terms.len());
        let mut score_parts: Vec<String> = Vec::with_capacity(terms.len() * 4);
        let mut or_clauses: Vec<String> = Vec::with_capacity(terms.len() * 4);
        for i in 0..terms.len() {
            let p = i + 1;
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

            let term_fields: Vec<String> = ["id", "title", "body", "coalesce(metadata, '')"]
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
        let limit_placeholder = terms.len() + 1;
        let sql = format!(
            "SELECT id, title, body, metadata, project_hash, session_id, created_at, updated_at, \
             ({match_count_expr}) AS match_count, ({score_expr}) AS score \
             FROM operational_sessions \
             WHERE ({or_clause}) \
             ORDER BY match_count DESC, score DESC, updated_at DESC \
             LIMIT ?{limit_placeholder}"
        );

        let mut stmt = conn.prepare(&sql)?;
        let mut params: Vec<Box<dyn rusqlite::ToSql>> = Vec::with_capacity(terms.len() + 1);
        for term in &terms {
            params.push(Box::new(term.clone()));
        }
        params.push(Box::new(limit as i64));
        let param_refs: Vec<&dyn rusqlite::ToSql> = params.iter().map(Box::as_ref).collect();

        let rows = stmt.query_map(param_refs.as_slice(), Self::read_operational_session)?;
        Ok(rows.filter_map(|row| row.ok()).collect())
    }

    pub fn prune_operational_sessions(&self, max_age_days: i64) -> Result<u64> {
        let cutoff = Utc::now().timestamp() - (max_age_days * 86400);
        let conn = self
            .conn
            .lock()
            .map_err(|e| anyhow!("Lock poisoned: {}", e))?;
        let count = conn.execute(
            "DELETE FROM operational_sessions WHERE updated_at < ?1",
            [cutoff],
        )?;
        Ok(count as u64)
    }

    fn read_operational_session(
        row: &rusqlite::Row<'_>,
    ) -> rusqlite::Result<OperationalSessionRecord> {
        Ok(OperationalSessionRecord {
            id: row.get(0)?,
            title: row.get(1)?,
            body: row.get(2)?,
            metadata: row.get(3)?,
            project_hash: row.get(4)?,
            session_id: row.get(5)?,
            created_at: row.get(6)?,
            updated_at: row.get(7)?,
        })
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
            status: None,
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
                relation: "extends".to_string(),
                weight: None,
            }]),
            ..sample_node_input("b")
        })
        .unwrap();
        db.upsert_intelligence_node(IntelligenceNodeInput {
            relations: Some(vec![IntelligenceRelationInput {
                to_node_id: "b".to_string(),
                relation: "extends".to_string(),
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
                relation: "extends".to_string(),
                weight: None,
            }]),
            ..sample_node_input("b")
        })
        .unwrap();
        db.upsert_intelligence_node(IntelligenceNodeInput {
            relations: Some(vec![IntelligenceRelationInput {
                to_node_id: "b".to_string(),
                relation: "extends".to_string(),
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
                relation: "extends".to_string(),
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
                status: None,
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
                status: None,
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
            status: None,
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
                status: None,
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
        let (_record, created, _, _) = db
            .upsert_intelligence_node(IntelligenceNodeInput {
                id: Some("new-node-1".to_string()),
                kind: "fact".to_string(),
                status: None,
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
            status: None,
            title: "First".to_string(),
            body: "body".to_string(),
            metadata: None,
            project_hash: Some("proj-a".to_string()),
            session_id: None,
            relations: None,
        })
        .unwrap();
        let (record, created, _, _) = db
            .upsert_intelligence_node(IntelligenceNodeInput {
                id: Some(id.to_string()),
                kind: "fact".to_string(),
                status: None,
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
            status: None,
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
        let (record, created, _, _) = db
            .upsert_intelligence_node(IntelligenceNodeInput {
                id: Some(resolved),
                kind: "fact".to_string(),
                status: None,
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
                status: None,
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
            status: None,
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
            status: None,
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

    #[test]
    fn operational_migration_moves_session_nodes_to_dedicated_table() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("test.db");

        // Seed pre-migration state directly in `intelligence_nodes`: 3
        // operational ids (the id prefixes the three writers use) plus 2
        // knowledge nodes. Before CM8 the writers wrote them this way.
        // Raw SQL (not upsert): these rows predate the CM10 kind vocabulary,
        // and the test guards the migration, not the write path.
        {
            let db = Database::new(&path).unwrap();
            {
                let conn = db.conn.lock().unwrap();
                for id in ["run:abc", "sync:/tmp:agent1", "launchpad:uuid-1"] {
                    conn.execute(
                        "INSERT INTO intelligence_nodes (id, kind, status, title, body, metadata, project_hash, session_id, created_at, updated_at)
                         VALUES (?1, 'session', 'noted', ?2, 'op body', NULL, NULL, NULL, strftime('%s','now'), strftime('%s','now'))",
                        rusqlite::params![id, format!("Op {id}")],
                    )
                    .unwrap();
                }
            }
            for (id, kind) in [("fact1", "fact"), ("pattern1", "pattern")] {
                db.upsert_intelligence_node(IntelligenceNodeInput {
                    id: Some(id.to_string()),
                    kind: kind.to_string(),
                    status: None,
                    title: format!("Knowledge {id}"),
                    body: "knowledge body".to_string(),
                    metadata: None,
                    project_hash: None,
                    session_id: None,
                    relations: None,
                })
                .unwrap();
            }
        }

        // Reopening runs `init()`, which runs the CM8 migration. This test
        // guards the production migration in `db/mod.rs` — inlining the SQL
        // here would let a deleted migration still pass.
        let db = Database::new(&path).unwrap();

        let ops = db.list_operational_sessions(10).unwrap();
        assert_eq!(ops.len(), 3, "operational_sessions should have 3 rows");
        let ids: std::collections::HashSet<_> = ops.iter().map(|r| r.id.as_str()).collect();
        assert!(ids.contains("run:abc"));
        assert!(ids.contains("sync:/tmp:agent1"));
        assert!(ids.contains("launchpad:uuid-1"));
        // Row content is carried across unchanged.
        let run_row = db.get_operational_session("run:abc").unwrap().unwrap();
        assert_eq!(run_row.title, "Op run:abc");
        assert_eq!(run_row.body, "op body");

        let knowledge = db.list_intelligence_nodes(None, 10).unwrap();
        assert_eq!(
            knowledge.len(),
            2,
            "intelligence_nodes should have 2 rows after migration"
        );
        let k_ids: std::collections::HashSet<_> = knowledge.iter().map(|r| r.id.as_str()).collect();
        assert!(k_ids.contains("fact1"));
        assert!(k_ids.contains("pattern1"));

        // Idempotent: reopening again (migration re-runs) is a no-op.
        drop(db);
        let db = Database::new(&path).unwrap();
        assert_eq!(db.list_operational_sessions(10).unwrap().len(), 3);
        assert_eq!(db.list_intelligence_nodes(None, 10).unwrap().len(), 2);
    }

    #[test]
    fn run_upsert_writes_to_operational_sessions_not_intelligence_nodes() {
        let db = test_db();
        let (rec, _) = db
            .upsert_operational_session(OperationalSessionInput {
                id: Some("run:test-run-1".to_string()),
                title: "Run title".to_string(),
                body: "Run body".to_string(),
                metadata: Some(serde_json::json!({"source":"run"})),
                project_hash: None,
                session_id: Some("run:test-run-1".to_string()),
            })
            .unwrap();
        assert_eq!(rec.id, "run:test-run-1");
        assert!(db
            .get_operational_session("run:test-run-1")
            .unwrap()
            .is_some());
        assert!(db
            .get_intelligence_node("run:test-run-1")
            .unwrap()
            .is_none());
    }

    #[test]
    fn search_operational_sessions_returns_operational_records() {
        let db = test_db();
        db.upsert_operational_session(OperationalSessionInput {
            id: Some("run:search-me".to_string()),
            title: "Searchable Mission Title".to_string(),
            body: "body".to_string(),
            metadata: None,
            project_hash: None,
            session_id: None,
        })
        .unwrap();
        db.upsert_operational_session(OperationalSessionInput {
            id: Some("run:other".to_string()),
            title: "Unrelated".to_string(),
            body: "other body".to_string(),
            metadata: None,
            project_hash: None,
            session_id: None,
        })
        .unwrap();

        let results = db
            .search_operational_sessions("Searchable Mission", 10)
            .unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].id, "run:search-me");
    }

    #[test]
    fn prune_operational_sessions_removes_old_records() {
        let db = test_db();
        let old_ts = chrono::Utc::now().timestamp() - (31 * 86400);
        {
            let conn = db.conn.lock().unwrap();
            conn.execute(
                "INSERT INTO operational_sessions (id, title, body, metadata, project_hash, session_id, created_at, updated_at) VALUES (?1, ?2, ?3, NULL, NULL, NULL, ?4, ?4)",
                rusqlite::params!["run:old", "Old", "old body", old_ts],
            )
            .unwrap();
        }
        db.upsert_operational_session(OperationalSessionInput {
            id: Some("run:recent".to_string()),
            title: "Recent".to_string(),
            body: "recent".to_string(),
            metadata: None,
            project_hash: None,
            session_id: None,
        })
        .unwrap();

        let pruned = db.prune_operational_sessions(30).unwrap();
        assert_eq!(pruned, 1);
        assert!(db.get_operational_session("run:old").unwrap().is_none());
        assert!(db.get_operational_session("run:recent").unwrap().is_some());
    }

    #[test]
    fn intelligence_search_excludes_operational_records() {
        let db = test_db();
        db.upsert_intelligence_node(IntelligenceNodeInput {
            id: Some("fact-search".to_string()),
            kind: "fact".to_string(),
            status: None,
            title: "UniqueSearchTerm Fact Title".to_string(),
            body: "body".to_string(),
            metadata: None,
            project_hash: None,
            session_id: None,
            relations: None,
        })
        .unwrap();
        db.upsert_operational_session(OperationalSessionInput {
            id: Some("run:op-search".to_string()),
            title: "UniqueSearchTerm Operational Title".to_string(),
            body: "op body".to_string(),
            metadata: None,
            project_hash: None,
            session_id: None,
        })
        .unwrap();

        let result = db
            .search_intelligence_nodes("UniqueSearchTerm", None, 10)
            .unwrap();
        assert_eq!(result.results.len(), 1);
        assert_eq!(result.results[0].id, "fact-search");
    }

    #[test]
    fn launchpad_operational_sessions_are_searchable_by_workdir() {
        let db = test_db();
        let workdir = "/tmp/launchpad-test";
        db.upsert_operational_session(OperationalSessionInput {
            id: Some("launchpad:test-uuid".to_string()),
            title: "Test Launchpad Mission".to_string(),
            body: "Launchpad body".to_string(),
            metadata: Some(serde_json::json!({
                "source": "launchpad",
                "workdir": workdir,
                "mode": "new",
                "summary": "summary text"
            })),
            project_hash: None,
            session_id: Some("launchpad:test-uuid".to_string()),
        })
        .unwrap();

        // LaunchpadDialog::for_workdir searches operational_sessions by workdir;
        // verify the underlying search returns the record.
        let results = db.search_operational_sessions(workdir, 10).unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].title, "Test Launchpad Mission");
        let meta: serde_json::Value =
            serde_json::from_str(results[0].metadata.as_deref().unwrap()).unwrap();
        assert_eq!(
            meta.get("source").and_then(|v| v.as_str()),
            Some("launchpad")
        );
    }

    // ── CM10: knowledge-node schema ──────────────────────────────────

    #[test]
    fn upsert_rejects_unknown_kind() {
        let db = test_db();
        let err = db
            .upsert_intelligence_node(IntelligenceNodeInput {
                kind: "unknown".to_string(),
                ..sample_node_input("bad-kind")
            })
            .unwrap_err();
        assert!(
            err.to_string().contains("unknown node kind"),
            "expected kind rejection, got: {err}"
        );
    }

    #[test]
    fn upsert_rejects_unknown_status() {
        let db = test_db();
        let err = db
            .upsert_intelligence_node(IntelligenceNodeInput {
                status: Some("bogus".to_string()),
                ..sample_node_input("bad-status")
            })
            .unwrap_err();
        assert!(
            err.to_string().contains("unknown node status"),
            "expected status rejection, got: {err}"
        );
    }

    #[test]
    fn upsert_defaults_status_to_noted() {
        let db = test_db();
        let (record, _, _, _) = db
            .upsert_intelligence_node(sample_node_input("default-status"))
            .unwrap();
        assert_eq!(record.status, "noted");
    }

    #[test]
    fn upsert_rejects_superseded_without_supersedes_edge() {
        let db = test_db();
        let err = db
            .upsert_intelligence_node(IntelligenceNodeInput {
                status: Some("superseded".to_string()),
                ..sample_node_input("sup-no-edge")
            })
            .unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("superseded"), "expected superseded in: {msg}");
        assert!(msg.contains("supersedes"), "expected supersedes in: {msg}");
    }

    #[test]
    fn upsert_accepts_superseded_with_supersedes_edge() {
        let db = test_db();
        db.upsert_intelligence_node(sample_node_input("sup-a"))
            .unwrap();
        let (record, _, _, _) = db
            .upsert_intelligence_node(IntelligenceNodeInput {
                status: Some("superseded".to_string()),
                relations: Some(vec![IntelligenceRelationInput {
                    to_node_id: "sup-a".to_string(),
                    relation: "supersedes".to_string(),
                    weight: None,
                }]),
                ..sample_node_input("sup-b")
            })
            .unwrap();
        assert_eq!(record.status, "superseded");
    }

    #[test]
    fn upsert_rejects_unknown_edge_relation() {
        let db = test_db();
        db.upsert_intelligence_node(sample_node_input("rel-target"))
            .unwrap();
        let err = db
            .upsert_intelligence_node(IntelligenceNodeInput {
                relations: Some(vec![IntelligenceRelationInput {
                    to_node_id: "rel-target".to_string(),
                    relation: "related_to".to_string(),
                    weight: None,
                }]),
                ..sample_node_input("rel-src")
            })
            .unwrap_err();
        assert!(
            err.to_string().contains("unknown edge relation"),
            "expected edge rejection, got: {err}"
        );
    }

    #[test]
    fn duplicate_detection_shared_citations() {
        let db = test_db();
        db.upsert_intelligence_node(IntelligenceNodeInput {
            body: "see src/foo.rs:42 for the implementation".to_string(),
            ..sample_node_input("cite-a")
        })
        .unwrap();
        let (_, _, duplicates, _) = db
            .upsert_intelligence_node(IntelligenceNodeInput {
                body: "also see src/foo.rs:42".to_string(),
                ..sample_node_input("cite-b")
            })
            .unwrap();
        assert!(
            duplicates.iter().any(|n| n.id == "cite-a"),
            "expected cite-a among duplicates, got: {:?}",
            duplicates.iter().map(|n| &n.id).collect::<Vec<_>>()
        );
    }

    #[test]
    fn duplicate_detection_lexical_similarity() {
        let db = test_db();
        db.upsert_intelligence_node(IntelligenceNodeInput {
            title: "Database connection cache".to_string(),
            ..sample_node_input("lex-a")
        })
        .unwrap();
        let (_, _, duplicates, _) = db
            .upsert_intelligence_node(IntelligenceNodeInput {
                title: "Database connection cache strategy".to_string(),
                ..sample_node_input("lex-b")
            })
            .unwrap();
        assert!(
            duplicates.iter().any(|n| n.id == "lex-a"),
            "expected lex-a among duplicates, got: {:?}",
            duplicates.iter().map(|n| &n.id).collect::<Vec<_>>()
        );
    }

    #[test]
    fn undeclared_reference_warning() {
        let db = test_db();
        let node_a_id = "aaaaaaaa-1111-2222-3333-444444444444";
        db.upsert_intelligence_node(IntelligenceNodeInput {
            id: Some(node_a_id.to_string()),
            ..sample_node_input("undecl-a")
        })
        .unwrap();
        // `undecl-c` exists so `undecl-b` also exercises a declared edge
        // alongside its undeclared reference.
        db.upsert_intelligence_node(sample_node_input("undecl-c"))
            .unwrap();
        let (_, _, _, undeclared) = db
            .upsert_intelligence_node(IntelligenceNodeInput {
                body: format!("this builds on {node_a_id}"),
                relations: Some(vec![IntelligenceRelationInput {
                    to_node_id: "undecl-c".to_string(),
                    relation: "extends".to_string(),
                    weight: None,
                }]),
                ..sample_node_input("undecl-b")
            })
            .unwrap();
        assert!(
            undeclared.iter().any(|id| id == node_a_id),
            "expected undeclared ref to {node_a_id}, got: {undeclared:?}"
        );
    }

    #[test]
    fn no_warning_for_node_without_edges() {
        let db = test_db();
        let (_, _, _, undeclared) = db
            .upsert_intelligence_node(IntelligenceNodeInput {
                body: "a fresh finding with no links".to_string(),
                ..sample_node_input("orphan")
            })
            .unwrap();
        assert!(
            undeclared.is_empty(),
            "expected no warning, got: {undeclared:?}"
        );
    }

    #[test]
    fn undeclared_reference_warns_even_without_declared_edges() {
        let db = test_db();
        let referenced = "bbbbbbbb-1111-2222-3333-444444444444";
        db.upsert_intelligence_node(IntelligenceNodeInput {
            id: Some(referenced.to_string()),
            ..sample_node_input("referenced")
        })
        .unwrap();

        let (_, _, _, undeclared) = db
            .upsert_intelligence_node(IntelligenceNodeInput {
                body: format!("this builds on {referenced}"),
                relations: None,
                ..sample_node_input("unlinked")
            })
            .unwrap();

        assert_eq!(undeclared, vec![referenced.to_string()]);
    }

    #[test]
    fn project_kind_exempt_from_kind_validation() {
        let db = test_db();
        let (record, _, _, _) = db
            .upsert_intelligence_node(IntelligenceNodeInput {
                kind: "project".to_string(),
                ..sample_node_input("proj-exempt")
            })
            .unwrap();
        assert_eq!(record.kind, "project");
    }

    #[test]
    fn status_preserved_on_update_when_not_provided() {
        let db = test_db();
        db.upsert_intelligence_node(IntelligenceNodeInput {
            status: Some("verified".to_string()),
            ..sample_node_input("keep-status")
        })
        .unwrap();
        let (record, created, _, _) = db
            .upsert_intelligence_node(IntelligenceNodeInput {
                title: "Updated title".to_string(),
                ..sample_node_input("keep-status")
            })
            .unwrap();
        assert!(!created);
        assert_eq!(record.status, "verified");
    }
}
