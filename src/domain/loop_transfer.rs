//! Loop export/import: a loop's design — name, description, nodes, edges,
//! and ensembles — serialized to and from one portable JSON document, so a
//! loop can leave one machine as a file and be recreated on another (spec
//! f0be8c66's successor: "a loop is worth sharing, not just narrating").
//!
//! Deliberately excludes ids, workdir, specs, and run/status state (a loop
//! design carrying someone else's backlog or run history would be a
//! surprise on arrival), and excludes `platform`/`model` by default (a
//! shared design pinned to a harness/model the recipient may not have is
//! either broken or silently spends their quota).
//!
//! This module is pure — it never touches the database. `daemon::handler`'s
//! `loop_export`/`loop_import` MCP tools (and their `canopy loop
//! export`/`import` CLI counterparts) fetch/persist the surrounding data and
//! call into here for the document shape and structural validation, so
//! import is built on the same node/edge/ensemble shapes `loop_add_node`/
//! `loop_add_edge`/`loop_add_ensemble` produce rather than a second,
//! divergent write path.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::domain::loops::{
    Ensemble, EnsembleDetails, EnsembleMember, Loop, LoopEdge, LoopEdgeCondition, LoopNode,
    LoopNodeKind,
};
use crate::domain::validation::validate_ensembles_in_graph;

/// The only `format_version` this build understands. An unrecognized or
/// missing version is a refusal, never a best-effort parse (decision 6).
pub const LOOP_EXPORT_FORMAT_VERSION: i64 = 1;

/// Ensemble member count bounds — mirrors `daemon::handler`'s
/// `ENSEMBLE_MIN_MEMBERS`/`ENSEMBLE_MAX_MEMBERS` (`loop_add_ensemble`'s own
/// authoring-time bounds). Duplicated rather than shared across the
/// domain/daemon boundary: the two numbers are part of the ensemble
/// contract itself, not an implementation detail either side owns alone.
const ENSEMBLE_MIN_MEMBERS: usize = 2;
const ENSEMBLE_MAX_MEMBERS: usize = 8;

/// A loop's design, portable across machines/installations. Key order is
/// deliberate (struct field order) — see `docs/loops.md` for the documented,
/// hand-writable contract this type is the source of truth for.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LoopExportDocument {
    pub format_version: i64,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub nodes: Vec<LoopExportNode>,
    pub edges: Vec<LoopExportEdge>,
    #[serde(default)]
    pub ensembles: Vec<LoopExportEnsemble>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LoopExportNode {
    pub name: String,
    pub kind: LoopNodeKind,
    pub position: i64,
    pub config: Value,
}

/// References nodes by `name`, never by id (decision 2) — what makes the
/// file reviewable, hand-editable, and diffable.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LoopExportEdge {
    pub from_node: String,
    pub to_node: String,
    pub condition: LoopEdgeCondition,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LoopExportEnsembleMember {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub platform: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt_override: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LoopExportEnsemble {
    pub name: String,
    pub prompt_template: String,
    pub entry_from_node: String,
    pub entry_condition: LoopEdgeCondition,
    pub on_pass_to: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub on_fail_to: Option<String>,
    pub min_pass: i64,
    pub timeout_minutes: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub straggler_timeout_minutes: Option<i64>,
    pub members: Vec<LoopExportEnsembleMember>,
}

/// Build a [`LoopExportDocument`] from a loop's already-fetched graph.
///
/// `graph_nodes`/`graph_edges` are the loop's *entire* top-level graph — the
/// same rows `list_loop_nodes_for_loop`/`list_loop_edges_for_loop` return,
/// including ensemble member/join nodes and their wiring edges. This
/// function is what tells the two apart: every node/edge owned by an
/// ensemble (a member node, the join node, and the edges wiring them) is
/// excluded from `nodes`/`edges` and represented instead as one
/// [`LoopExportEnsemble`] entry, so an ensemble survives the round trip as
/// an ensemble, not as expanded member nodes.
pub fn build_export_document(
    lp: &Loop,
    graph_nodes: &[LoopNode],
    graph_edges: &[LoopEdge],
    ensembles: &[EnsembleDetails],
    with_models: bool,
) -> Result<LoopExportDocument, String> {
    let owned_ids: std::collections::HashSet<&str> = ensembles
        .iter()
        .flat_map(|details| {
            details
                .members
                .iter()
                .map(|member| member.node_id.as_str())
                .chain(std::iter::once(details.ensemble.join_node_id.as_str()))
        })
        .collect();

    let mut plain_nodes: Vec<&LoopNode> = graph_nodes
        .iter()
        .filter(|node| !owned_ids.contains(node.id.as_str()))
        .collect();
    plain_nodes.sort_by_key(|node| node.position);

    // Decision 2's enforced consequence: two nodes sharing a name would
    // make the exported edges/ensembles ambiguous about which one they
    // mean, so export refuses outright rather than guessing.
    let mut name_counts: HashMap<&str, usize> = HashMap::new();
    for node in &plain_nodes {
        *name_counts.entry(node.name.as_str()).or_insert(0) += 1;
    }
    let mut duplicate_names: Vec<&str> = name_counts
        .into_iter()
        .filter(|(_, count)| *count > 1)
        .map(|(name, _)| name)
        .collect();
    if !duplicate_names.is_empty() {
        duplicate_names.sort_unstable();
        return Err(format!(
            "Loop '{}' has duplicate node name(s): {}. Export requires unique node names, since the exported file references nodes by name — rename before exporting.",
            lp.name,
            duplicate_names.join(", ")
        ));
    }

    let id_to_name: HashMap<&str, &str> = plain_nodes
        .iter()
        .map(|node| (node.id.as_str(), node.name.as_str()))
        .collect();
    let resolve_name = |node_id: &str| -> Result<String, String> {
        id_to_name
            .get(node_id)
            .map(|name| (*name).to_string())
            .ok_or_else(|| {
                format!(
                    "Loop '{}' references node '{node_id}' that is not part of its own graph.",
                    lp.name
                )
            })
    };

    let export_nodes = plain_nodes
        .iter()
        .map(|node| LoopExportNode {
            name: node.name.clone(),
            kind: node.kind,
            position: node.position,
            config: export_node_config(node.kind, &node.config, with_models),
        })
        .collect();

    let mut export_edges = Vec::new();
    for edge in graph_edges {
        if owned_ids.contains(edge.from_node.as_str()) || owned_ids.contains(edge.to_node.as_str())
        {
            continue;
        }
        export_edges.push(LoopExportEdge {
            from_node: resolve_name(&edge.from_node)?,
            to_node: resolve_name(&edge.to_node)?,
            condition: edge.condition.clone(),
        });
    }
    export_edges.sort_by_key(edge_sort_key);

    let mut sorted_ensembles: Vec<&EnsembleDetails> = ensembles.iter().collect();
    sorted_ensembles.sort_by_key(|details| {
        graph_nodes
            .iter()
            .find(|node| node.id == details.ensemble.join_node_id)
            .map(|node| node.position)
            .unwrap_or(i64::MAX)
    });

    let mut export_ensembles = Vec::with_capacity(sorted_ensembles.len());
    for details in sorted_ensembles {
        let ensemble = &details.ensemble;
        let on_fail_to = ensemble
            .on_fail_to
            .as_deref()
            .map(resolve_name)
            .transpose()?;

        let members = details
            .members
            .iter()
            .map(|member| LoopExportEnsembleMember {
                platform: with_models.then(|| member.platform.clone()),
                model: if with_models {
                    member.model.clone()
                } else {
                    None
                },
                prompt_override: member.prompt_override.clone(),
            })
            .collect();

        export_ensembles.push(LoopExportEnsemble {
            name: ensemble.name.clone(),
            prompt_template: ensemble.prompt_template.clone(),
            entry_from_node: resolve_name(&ensemble.entry_from_node)?,
            entry_condition: ensemble.entry_condition.clone(),
            on_pass_to: resolve_name(&ensemble.on_pass_to)?,
            on_fail_to,
            min_pass: ensemble.min_pass,
            timeout_minutes: ensemble.timeout_minutes,
            straggler_timeout_minutes: ensemble.straggler_timeout_minutes,
            members,
        });
    }

    Ok(LoopExportDocument {
        format_version: LOOP_EXPORT_FORMAT_VERSION,
        name: lp.name.clone(),
        description: lp.description.clone(),
        nodes: export_nodes,
        edges: export_edges,
        ensembles: export_ensembles,
    })
}

/// An agent node's config with `platform`/`model` stripped, unless
/// `with_models` — decision 3. Every other kind's config, and every other
/// key on an agent node's config, passes through untouched.
fn export_node_config(kind: LoopNodeKind, config: &Value, with_models: bool) -> Value {
    if with_models || kind != LoopNodeKind::Agent {
        return config.clone();
    }
    let mut map = config.as_object().cloned().unwrap_or_default();
    map.remove("platform");
    map.remove("model");
    Value::Object(map)
}

fn edge_sort_key(edge: &LoopExportEdge) -> String {
    format!(
        "{}\u{0}{}\u{0}{}\u{0}{}",
        edge.from_node,
        edge.to_node,
        edge.condition.as_str(),
        edge.condition.route_label().unwrap_or("")
    )
}

/// Parse and validate a `format_version` before attempting the full
/// deserialization, so a missing/unsupported version is refused with its
/// own clear message rather than falling through to a generic serde error
/// (decision 6).
pub fn parse_export_document_value(value: &Value) -> Result<LoopExportDocument, String> {
    match value.get("format_version").and_then(Value::as_i64) {
        Some(version) if version == LOOP_EXPORT_FORMAT_VERSION => {}
        Some(other) => {
            return Err(format!(
                "Loop export document has format_version {other}, but this build only supports {LOOP_EXPORT_FORMAT_VERSION}."
            ))
        }
        None => {
            return Err(
                "Loop export document is missing format_version; refusing to guess. Expected format_version: 1."
                    .to_string(),
            )
        }
    }
    serde_json::from_value(value.clone()).map_err(|e| format!("Invalid loop export document: {e}."))
}

/// [`parse_export_document_value`] from raw JSON text — the CLI's entry
/// point for a file's (or stdin's) contents.
pub fn parse_export_document_str(raw: &str) -> Result<LoopExportDocument, String> {
    let value: Value = serde_json::from_str(raw)
        .map_err(|e| format!("Invalid loop export file: not valid JSON ({e})."))?;
    parse_export_document_value(&value)
}

/// One ensemble unit built by [`build_import_plan`] — the join node, every
/// member node, the `ensembles`/`ensemble_members` rows — mirroring the
/// shape `daemon::handler::build_ensemble_unit` assembles for
/// `loop_add_ensemble`, so the two authoring paths can never drift.
#[derive(Debug, Clone)]
pub struct LoopImportEnsemblePlan {
    pub ensemble: Ensemble,
    pub members: Vec<EnsembleMember>,
    pub member_nodes: Vec<LoopNode>,
    pub join_node: LoopNode,
}

/// Every fresh-id graph piece [`build_import_plan`] assembles for one
/// `loop_import` call — ready to persist as-is (decision 5: import is
/// all-or-nothing, so the caller persists every field here in one
/// transaction or none at all).
#[derive(Debug, Clone)]
pub struct LoopImportPlan {
    pub nodes: Vec<LoopNode>,
    pub edges: Vec<LoopEdge>,
    pub ensembles: Vec<LoopImportEnsemblePlan>,
}

/// Build a validated [`LoopImportPlan`] for `loop_id` from a parsed
/// [`LoopExportDocument`] — every node/edge/ensemble gets a fresh id, names
/// resolve to those ids, and every rule `loop_add_node`/`loop_add_edge`/
/// `loop_add_ensemble` would enforce at authoring time is enforced here too
/// (decision 5), with one deliberate carve-out: an agent node's missing
/// `platform`/`cli` is *not* rejected — decision 3 means a shared design may
/// legitimately arrive without one, and the caller (`loop_import`) reports
/// exactly which nodes still need it rather than refusing the whole import.
///
/// Returns `Err` (with nothing to persist) on: an unsupported/missing
/// `format_version`, a duplicate node name, an edge or ensemble field naming
/// a node absent from `document.nodes`, an ensemble with 2-8 members
/// violated, or `min_pass`/timeout fields out of range.
pub fn build_import_plan(
    document: &LoopExportDocument,
    loop_id: &str,
) -> Result<LoopImportPlan, String> {
    if document.format_version != LOOP_EXPORT_FORMAT_VERSION {
        return Err(format!(
            "Loop export document has format_version {}, but this build only supports {}.",
            document.format_version, LOOP_EXPORT_FORMAT_VERSION
        ));
    }

    let mut name_counts: HashMap<&str, usize> = HashMap::new();
    for node in &document.nodes {
        *name_counts.entry(node.name.as_str()).or_insert(0) += 1;
    }
    let mut duplicate_names: Vec<&str> = name_counts
        .into_iter()
        .filter(|(_, count)| *count > 1)
        .map(|(name, _)| name)
        .collect();
    if !duplicate_names.is_empty() {
        duplicate_names.sort_unstable();
        return Err(format!(
            "Loop export document has duplicate node name(s): {}.",
            duplicate_names.join(", ")
        ));
    }

    let now = chrono::Utc::now();
    let mut name_to_id: HashMap<&str, String> = HashMap::new();
    let mut nodes = Vec::with_capacity(document.nodes.len());
    for doc_node in &document.nodes {
        if doc_node.kind == LoopNodeKind::Join {
            return Err(format!(
                "Node '{}' has kind 'join', which is engine-managed and can only be created via an ensemble — it can never be authored directly.",
                doc_node.name
            ));
        }
        let id = uuid::Uuid::new_v4().to_string();
        name_to_id.insert(doc_node.name.as_str(), id.clone());
        nodes.push(LoopNode {
            id,
            spec_id: None,
            loop_id: Some(loop_id.to_string()),
            name: doc_node.name.clone(),
            kind: doc_node.kind,
            config: doc_node.config.clone(),
            position: doc_node.position,
            created_at: now,
        });
    }

    let resolve = |name: &str| -> Result<String, String> {
        name_to_id
            .get(name)
            .cloned()
            .ok_or_else(|| format!("references unknown node '{name}'"))
    };

    let mut edges = Vec::with_capacity(document.edges.len());
    for doc_edge in &document.edges {
        let from_id = resolve(&doc_edge.from_node).map_err(|e| {
            format!(
                "Edge '{}' -> '{}' {e}.",
                doc_edge.from_node, doc_edge.to_node
            )
        })?;
        let to_id = resolve(&doc_edge.to_node).map_err(|e| {
            format!(
                "Edge '{}' -> '{}' {e}.",
                doc_edge.from_node, doc_edge.to_node
            )
        })?;
        edges.push(LoopEdge {
            id: uuid::Uuid::new_v4().to_string(),
            spec_id: None,
            loop_id: Some(loop_id.to_string()),
            from_node: from_id,
            to_node: to_id,
            condition: doc_edge.condition.clone(),
        });
    }

    // Ensemble-owned nodes continue the position sequence after every plain
    // node — the same "append after existing nodes" convention
    // `loop_add_ensemble`'s `start_position` uses.
    let mut next_position = nodes
        .iter()
        .map(|node| node.position)
        .max()
        .map(|max| max + 1)
        .unwrap_or(1);

    let mut ensembles = Vec::with_capacity(document.ensembles.len());
    for doc_ensemble in &document.ensembles {
        if doc_ensemble.members.len() < ENSEMBLE_MIN_MEMBERS
            || doc_ensemble.members.len() > ENSEMBLE_MAX_MEMBERS
        {
            return Err(format!(
                "Ensemble '{}' must have {ENSEMBLE_MIN_MEMBERS}-{ENSEMBLE_MAX_MEMBERS} members, got {}.",
                doc_ensemble.name,
                doc_ensemble.members.len()
            ));
        }
        if doc_ensemble.min_pass < 1 || doc_ensemble.min_pass > doc_ensemble.members.len() as i64 {
            return Err(format!(
                "Ensemble '{}' has an invalid min_pass ({}) for {} members.",
                doc_ensemble.name,
                doc_ensemble.min_pass,
                doc_ensemble.members.len()
            ));
        }
        if doc_ensemble.timeout_minutes < 0 {
            return Err(format!(
                "Ensemble '{}' has a negative timeout_minutes.",
                doc_ensemble.name
            ));
        }
        if let Some(straggler) = doc_ensemble.straggler_timeout_minutes {
            if straggler < 0 {
                return Err(format!(
                    "Ensemble '{}' has a negative straggler_timeout_minutes.",
                    doc_ensemble.name
                ));
            }
        }

        let entry_from_node = resolve(&doc_ensemble.entry_from_node)
            .map_err(|e| format!("Ensemble '{}' entry_from_node {e}.", doc_ensemble.name))?;
        let on_pass_to = resolve(&doc_ensemble.on_pass_to)
            .map_err(|e| format!("Ensemble '{}' on_pass_to {e}.", doc_ensemble.name))?;
        let on_fail_to = doc_ensemble
            .on_fail_to
            .as_deref()
            .map(|name| {
                resolve(name)
                    .map_err(|e| format!("Ensemble '{}' on_fail_to {e}.", doc_ensemble.name))
            })
            .transpose()?;

        let ensemble_id = uuid::Uuid::new_v4().to_string();
        let join_node_id = uuid::Uuid::new_v4().to_string();
        let mut member_nodes = Vec::with_capacity(doc_ensemble.members.len());
        let mut members = Vec::with_capacity(doc_ensemble.members.len());

        for (index, member) in doc_ensemble.members.iter().enumerate() {
            let node_id = uuid::Uuid::new_v4().to_string();
            let effective_prompt = member
                .prompt_override
                .as_deref()
                .unwrap_or(doc_ensemble.prompt_template.as_str());
            member_nodes.push(LoopNode {
                id: node_id.clone(),
                spec_id: None,
                loop_id: Some(loop_id.to_string()),
                name: format!("{} [{}]", doc_ensemble.name, index + 1),
                kind: LoopNodeKind::Agent,
                // A member's platform may legitimately be absent (see this
                // function's doc comment) — an empty string here is exactly
                // what `agent_nodes_missing_platform` looks for downstream.
                config: serde_json::json!({
                    "platform": member.platform.clone().unwrap_or_default(),
                    "model": member.model,
                    "prompt_template": effective_prompt,
                    "timeout_minutes": doc_ensemble.timeout_minutes,
                }),
                position: next_position,
                created_at: now,
            });
            edges.push(LoopEdge {
                id: uuid::Uuid::new_v4().to_string(),
                spec_id: None,
                loop_id: Some(loop_id.to_string()),
                from_node: entry_from_node.clone(),
                to_node: node_id.clone(),
                condition: doc_ensemble.entry_condition.clone(),
            });
            edges.push(LoopEdge {
                id: uuid::Uuid::new_v4().to_string(),
                spec_id: None,
                loop_id: Some(loop_id.to_string()),
                from_node: node_id.clone(),
                to_node: join_node_id.clone(),
                condition: LoopEdgeCondition::Always,
            });
            members.push(EnsembleMember {
                ensemble_id: ensemble_id.clone(),
                node_id,
                position: index as i64,
                platform: member.platform.clone().unwrap_or_default(),
                model: member.model.clone(),
                prompt_override: member.prompt_override.clone(),
            });
            next_position += 1;
        }

        let join_node = LoopNode {
            id: join_node_id.clone(),
            spec_id: None,
            loop_id: Some(loop_id.to_string()),
            name: format!("{} (quorum)", doc_ensemble.name),
            kind: LoopNodeKind::Join,
            config: serde_json::json!({ "ensemble_id": ensemble_id }),
            position: next_position,
            created_at: now,
        };
        next_position += 1;

        edges.push(LoopEdge {
            id: uuid::Uuid::new_v4().to_string(),
            spec_id: None,
            loop_id: Some(loop_id.to_string()),
            from_node: join_node_id.clone(),
            to_node: on_pass_to.clone(),
            condition: LoopEdgeCondition::Pass,
        });
        if let Some(on_fail_to) = &on_fail_to {
            edges.push(LoopEdge {
                id: uuid::Uuid::new_v4().to_string(),
                spec_id: None,
                loop_id: Some(loop_id.to_string()),
                from_node: join_node_id.clone(),
                to_node: on_fail_to.clone(),
                condition: LoopEdgeCondition::Fail,
            });
        }

        ensembles.push(LoopImportEnsemblePlan {
            ensemble: Ensemble {
                id: ensemble_id,
                spec_id: None,
                loop_id: Some(loop_id.to_string()),
                name: doc_ensemble.name.clone(),
                prompt_template: doc_ensemble.prompt_template.clone(),
                join_node_id,
                entry_from_node,
                entry_condition: doc_ensemble.entry_condition.clone(),
                min_pass: doc_ensemble.min_pass,
                straggler_timeout_minutes: doc_ensemble.straggler_timeout_minutes,
                timeout_minutes: doc_ensemble.timeout_minutes,
                on_pass_to,
                on_fail_to,
                created_at: now,
            },
            members,
            member_nodes,
            join_node,
        });
    }

    // Defense in depth: the same structural check `loop_run` runs on every
    // ensemble reachable from a live graph (see `validate_ensembles_in_graph`)
    // confirms the plan's wiring is internally consistent before anything is
    // persisted — nothing above should ever be able to trip it, but a
    // hand-edited file is exactly the kind of input this guards against.
    let mut all_nodes = nodes.clone();
    let mut ensemble_details = Vec::with_capacity(ensembles.len());
    for plan in &ensembles {
        all_nodes.push(plan.join_node.clone());
        all_nodes.extend(plan.member_nodes.iter().cloned());
        ensemble_details.push(EnsembleDetails {
            ensemble: plan.ensemble.clone(),
            members: plan.members.clone(),
        });
    }
    validate_ensembles_in_graph(&ensemble_details, &all_nodes, &edges)?;

    Ok(LoopImportPlan {
        nodes,
        edges,
        ensembles,
    })
}

/// Names of every agent node (plain or ensemble member) left without a
/// `platform`/`cli` after import — decision 3's counterpart: since export
/// strips them by default, `loop_import`'s response calls out exactly which
/// nodes need one filled in before the loop can run (requirement 4).
pub fn agent_nodes_missing_platform(plan: &LoopImportPlan) -> Vec<String> {
    let plain = plan
        .nodes
        .iter()
        .filter(|node| node.kind == LoopNodeKind::Agent)
        .filter(|node| !node_config_has_harness(&node.config));
    let members = plan
        .ensembles
        .iter()
        .flat_map(|plan| plan.member_nodes.iter())
        .filter(|node| !node_config_has_harness(&node.config));
    plain.chain(members).map(|node| node.name.clone()).collect()
}

/// Mirrors `daemon::handler::config_has_agent_harness` by design: an agent
/// node's config carries a non-empty `platform` or `cli`.
fn node_config_has_harness(config: &Value) -> bool {
    let has_non_empty_str = |field: &str| {
        config
            .get(field)
            .and_then(Value::as_str)
            .is_some_and(|s| !s.trim().is_empty())
    };
    has_non_empty_str("platform") || has_non_empty_str("cli")
}

/// Resolve a desired loop name against names already taken in the target
/// workdir: the name itself if free, else `"{name} (2)"`, `"{name} (3)"`,
/// etc. — decision 4's "import always creates a new loop" never overwrites,
/// so a taken name gets a suffix instead of a refusal.
pub fn resolve_unique_loop_name(existing_names: &[String], desired: &str) -> String {
    if !existing_names.iter().any(|name| name == desired) {
        return desired.to_string();
    }
    let mut suffix = 2;
    loop {
        let candidate = format!("{desired} ({suffix})");
        if !existing_names.iter().any(|name| name == &candidate) {
            return candidate;
        }
        suffix += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;

    fn make_loop(name: &str) -> Loop {
        Loop {
            archived: false,
            id: "loop-1".to_string(),
            name: name.to_string(),
            description: Some("A test loop".to_string()),
            workdir: "/tmp/project".to_string(),
            status: crate::domain::loops::LoopStatus::Draft,
            trigger: None,
            created_at: Utc::now(),
            started_at: None,
            completed_at: None,
            autorun_at: None,
            auto_continue_at: None,
            auto_continue_action: None,
            active_run_queue_id: None,
            on_completed: None,
        }
    }

    fn make_node(
        id: &str,
        name: &str,
        kind: LoopNodeKind,
        config: Value,
        position: i64,
    ) -> LoopNode {
        LoopNode {
            id: id.to_string(),
            spec_id: None,
            loop_id: Some("loop-1".to_string()),
            name: name.to_string(),
            kind,
            config,
            position,
            created_at: Utc::now(),
        }
    }

    fn make_edge(id: &str, from: &str, to: &str, condition: LoopEdgeCondition) -> LoopEdge {
        LoopEdge {
            id: id.to_string(),
            spec_id: None,
            loop_id: Some("loop-1".to_string()),
            from_node: from.to_string(),
            to_node: to.to_string(),
            condition,
        }
    }

    /// A simple 3-node chain (implementer -> gate -> committer), no
    /// ensembles: the base case every other test builds on.
    fn simple_graph() -> (Loop, Vec<LoopNode>, Vec<LoopEdge>) {
        let lp = make_loop("simple-loop");
        let nodes = vec![
            make_node(
                "n1",
                "implementer",
                LoopNodeKind::Agent,
                serde_json::json!({"platform": "claude", "model": "opus", "prompt_template": "implement it"}),
                1,
            ),
            make_node(
                "n2",
                "gate",
                LoopNodeKind::Check,
                serde_json::json!({"command": "cargo test"}),
                2,
            ),
            make_node(
                "n3",
                "committer",
                LoopNodeKind::Agent,
                serde_json::json!({"platform": "claude", "prompt_template": "commit it"}),
                3,
            ),
        ];
        let edges = vec![
            make_edge("e1", "n1", "n2", LoopEdgeCondition::Always),
            make_edge("e2", "n2", "n3", LoopEdgeCondition::Pass),
        ];
        (lp, nodes, edges)
    }

    #[test]
    fn export_strips_platform_and_model_by_default() {
        let (lp, nodes, edges) = simple_graph();
        let doc = build_export_document(&lp, &nodes, &edges, &[], false).unwrap();
        let implementer = doc.nodes.iter().find(|n| n.name == "implementer").unwrap();
        assert!(implementer.config.get("platform").is_none());
        assert!(implementer.config.get("model").is_none());
        assert_eq!(implementer.config["prompt_template"], "implement it");
    }

    #[test]
    fn export_with_models_keeps_platform_and_model() {
        let (lp, nodes, edges) = simple_graph();
        let doc = build_export_document(&lp, &nodes, &edges, &[], true).unwrap();
        let implementer = doc.nodes.iter().find(|n| n.name == "implementer").unwrap();
        assert_eq!(implementer.config["platform"], "claude");
        assert_eq!(implementer.config["model"], "opus");
    }

    #[test]
    fn export_rejects_duplicate_node_names() {
        let (lp, mut nodes, edges) = simple_graph();
        nodes[1].name = "implementer".to_string();
        let err = build_export_document(&lp, &nodes, &edges, &[], false).unwrap_err();
        assert!(err.contains("implementer"));
        assert!(err.contains("duplicate"));
    }

    #[test]
    fn format_version_first_key_in_serialized_json() {
        let (lp, nodes, edges) = simple_graph();
        let doc = build_export_document(&lp, &nodes, &edges, &[], false).unwrap();
        let raw = serde_json::to_string(&doc).unwrap();
        // struct field order drives serde_json's key order.
        assert!(raw.starts_with("{\"format_version\":1"));
    }

    #[test]
    fn import_rejects_missing_format_version() {
        let value = serde_json::json!({
            "name": "x", "nodes": [], "edges": [], "ensembles": []
        });
        let err = parse_export_document_value(&value).unwrap_err();
        assert!(err.contains("format_version"));
    }

    #[test]
    fn import_rejects_unsupported_format_version() {
        let value = serde_json::json!({
            "format_version": 2, "name": "x", "nodes": [], "edges": [], "ensembles": []
        });
        let err = parse_export_document_value(&value).unwrap_err();
        assert!(err.contains("2"));
    }

    #[test]
    fn import_plan_rejects_edge_naming_nonexistent_node() {
        let doc = LoopExportDocument {
            format_version: 1,
            name: "x".to_string(),
            description: None,
            nodes: vec![LoopExportNode {
                name: "only".to_string(),
                kind: LoopNodeKind::Check,
                position: 1,
                config: serde_json::json!({"command": "true"}),
            }],
            edges: vec![LoopExportEdge {
                from_node: "only".to_string(),
                to_node: "ghost".to_string(),
                condition: LoopEdgeCondition::Always,
            }],
            ensembles: vec![],
        };
        let err = build_import_plan(&doc, "new-loop").unwrap_err();
        assert!(err.contains("ghost"));
    }

    #[test]
    fn import_plan_rejects_join_kind_node() {
        let doc = LoopExportDocument {
            format_version: 1,
            name: "x".to_string(),
            description: None,
            nodes: vec![LoopExportNode {
                name: "sneaky".to_string(),
                kind: LoopNodeKind::Join,
                position: 1,
                config: serde_json::json!({}),
            }],
            edges: vec![],
            ensembles: vec![],
        };
        let err = build_import_plan(&doc, "new-loop").unwrap_err();
        assert!(err.contains("join"));
    }

    #[test]
    fn import_plan_assigns_fresh_ids_and_loop_id() {
        let (lp, nodes, edges) = simple_graph();
        let doc = build_export_document(&lp, &nodes, &edges, &[], true).unwrap();
        let plan = build_import_plan(&doc, "brand-new-loop-id").unwrap();
        assert_eq!(plan.nodes.len(), 3);
        for node in &plan.nodes {
            assert_eq!(node.loop_id.as_deref(), Some("brand-new-loop-id"));
            assert!(node.spec_id.is_none());
            assert!(!nodes.iter().any(|n| n.id == node.id), "id must be fresh");
        }
        assert_eq!(plan.edges.len(), 2);
    }

    #[test]
    fn agent_nodes_missing_platform_reports_stripped_nodes() {
        let (lp, nodes, edges) = simple_graph();
        let doc = build_export_document(&lp, &nodes, &edges, &[], false).unwrap();
        let plan = build_import_plan(&doc, "loop-2").unwrap();
        let missing = agent_nodes_missing_platform(&plan);
        assert_eq!(missing.len(), 2);
        assert!(missing.contains(&"implementer".to_string()));
        assert!(missing.contains(&"committer".to_string()));
    }

    #[test]
    fn agent_nodes_missing_platform_empty_with_models() {
        let (lp, nodes, edges) = simple_graph();
        let doc = build_export_document(&lp, &nodes, &edges, &[], true).unwrap();
        let plan = build_import_plan(&doc, "loop-2").unwrap();
        assert!(agent_nodes_missing_platform(&plan).is_empty());
    }

    #[test]
    fn resolve_unique_loop_name_returns_desired_when_free() {
        let existing = vec!["other".to_string()];
        assert_eq!(resolve_unique_loop_name(&existing, "my-loop"), "my-loop");
    }

    #[test]
    fn resolve_unique_loop_name_suffixes_on_collision() {
        let existing = vec!["my-loop".to_string(), "my-loop (2)".to_string()];
        assert_eq!(
            resolve_unique_loop_name(&existing, "my-loop"),
            "my-loop (3)"
        );
    }

    fn make_ensemble_details(
        ensemble_id: &str,
        join_id: &str,
        entry_from: &str,
        on_pass_to: &str,
        member_ids: &[&str],
    ) -> EnsembleDetails {
        let ensemble = Ensemble {
            id: ensemble_id.to_string(),
            spec_id: None,
            loop_id: Some("loop-1".to_string()),
            name: "Proposers".to_string(),
            prompt_template: "draft it".to_string(),
            join_node_id: join_id.to_string(),
            entry_from_node: entry_from.to_string(),
            entry_condition: LoopEdgeCondition::Always,
            min_pass: member_ids.len() as i64,
            straggler_timeout_minutes: None,
            timeout_minutes: 30,
            on_pass_to: on_pass_to.to_string(),
            on_fail_to: None,
            created_at: Utc::now(),
        };
        let members = member_ids
            .iter()
            .enumerate()
            .map(|(i, id)| EnsembleMember {
                ensemble_id: ensemble_id.to_string(),
                node_id: id.to_string(),
                position: i as i64,
                platform: "openrouter".to_string(),
                model: Some(format!("model-{i}")),
                prompt_override: None,
            })
            .collect();
        EnsembleDetails { ensemble, members }
    }

    /// The full ensemble round trip: build a graph with a kickoff node, a
    /// 2-member ensemble, and a downstream node; export it, import it under
    /// a new loop id, and export the result again. Requirement 5 (round
    /// trip with `--with-models` on both ends) and the acceptance
    /// criterion ("an ensemble survives the round trip as an ensemble, not
    /// as expanded member nodes") both pin on this.
    #[test]
    fn ensemble_round_trips_as_an_ensemble_not_expanded_nodes() {
        let lp = make_loop("ensemble-loop");
        let kickoff = make_node(
            "kickoff",
            "kickoff",
            LoopNodeKind::Check,
            serde_json::json!({"command": "true"}),
            1,
        );
        let downstream = make_node(
            "downstream",
            "downstream",
            LoopNodeKind::Agent,
            serde_json::json!({"platform": "claude", "prompt_template": "wrap up"}),
            10,
        );
        let member1 = make_node(
            "m1",
            "Proposers [1]",
            LoopNodeKind::Agent,
            serde_json::json!({"platform": "openrouter", "model": "model-0", "prompt_template": "draft it", "timeout_minutes": 30}),
            2,
        );
        let member2 = make_node(
            "m2",
            "Proposers [2]",
            LoopNodeKind::Agent,
            serde_json::json!({"platform": "openrouter", "model": "model-1", "prompt_template": "draft it", "timeout_minutes": 30}),
            3,
        );
        let join = make_node(
            "join1",
            "Proposers (quorum)",
            LoopNodeKind::Join,
            serde_json::json!({"ensemble_id": "ens1"}),
            4,
        );
        let nodes = vec![kickoff, downstream, member1, member2, join];
        let edges = vec![
            make_edge("e1", "kickoff", "m1", LoopEdgeCondition::Always),
            make_edge("e2", "kickoff", "m2", LoopEdgeCondition::Always),
            make_edge("e3", "m1", "join1", LoopEdgeCondition::Always),
            make_edge("e4", "m2", "join1", LoopEdgeCondition::Always),
            make_edge("e5", "join1", "downstream", LoopEdgeCondition::Pass),
        ];
        let ensembles = vec![make_ensemble_details(
            "ens1",
            "join1",
            "kickoff",
            "downstream",
            &["m1", "m2"],
        )];

        let first_export = build_export_document(&lp, &nodes, &edges, &ensembles, true).unwrap();

        // Ensemble survives as one ensemble entry; the plain node list
        // excludes the member/join nodes entirely.
        assert_eq!(first_export.ensembles.len(), 1);
        assert_eq!(first_export.nodes.len(), 2);
        assert!(!first_export
            .nodes
            .iter()
            .any(|n| n.name.contains("Proposers")));
        assert_eq!(first_export.ensembles[0].members.len(), 2);

        let plan = build_import_plan(&first_export, "loop-2").unwrap();
        assert_eq!(plan.ensembles.len(), 1);
        assert_eq!(plan.ensembles[0].member_nodes.len(), 2);

        // Re-export the imported plan under a Loop with the same name as
        // the original (import always renames on collision, but here we
        // simulate "no collision" so the round trip is name-for-name) and
        // assert the document is identical except for the name change we
        // deliberately introduce.
        let mut imported_all_nodes = plan.nodes.clone();
        let mut imported_ensemble_details = Vec::new();
        for ens in &plan.ensembles {
            imported_all_nodes.push(ens.join_node.clone());
            imported_all_nodes.extend(ens.member_nodes.iter().cloned());
            imported_ensemble_details.push(EnsembleDetails {
                ensemble: ens.ensemble.clone(),
                members: ens.members.clone(),
            });
        }
        let mut lp2 = make_loop("ensemble-loop");
        lp2.id = "loop-2".to_string();
        let second_export = build_export_document(
            &lp2,
            &imported_all_nodes,
            &plan.edges,
            &imported_ensemble_details,
            true,
        )
        .unwrap();

        assert_eq!(first_export.name, second_export.name);
        assert_eq!(first_export.description, second_export.description);
        assert_eq!(first_export.nodes, second_export.nodes);
        assert_eq!(first_export.edges, second_export.edges);
        assert_eq!(first_export.ensembles, second_export.ensembles);
    }

    /// Requirement 5's full statement: export -> import -> export again
    /// produces an identical document except for the name, for a plain
    /// (non-ensemble) graph too.
    #[test]
    fn plain_graph_round_trips_identically_except_name() {
        let (lp, nodes, edges) = simple_graph();
        let first_export = build_export_document(&lp, &nodes, &edges, &[], true).unwrap();

        let plan = build_import_plan(&first_export, "loop-2").unwrap();
        let mut lp2 = make_loop("renamed-on-import");
        lp2.id = "loop-2".to_string();
        let second_export =
            build_export_document(&lp2, &plan.nodes, &plan.edges, &[], true).unwrap();

        assert_ne!(first_export.name, second_export.name);
        assert_eq!(second_export.name, "renamed-on-import");
        assert_eq!(first_export.description, second_export.description);
        assert_eq!(first_export.nodes, second_export.nodes);
        assert_eq!(first_export.edges, second_export.edges);
        assert_eq!(first_export.ensembles, second_export.ensembles);
    }

    /// Pins `docs/loops.md`'s worked example to the actual format: if this
    /// test ever fails to parse/import, the doc's example has drifted from
    /// what the code accepts and needs updating alongside it.
    #[test]
    fn docs_worked_example_parses_and_imports_cleanly() {
        let raw = r#"{
          "format_version": 1,
          "name": "implement-and-review",
          "description": "Implement a spec, get two model opinions, then commit.",
          "nodes": [
            {
              "name": "implementer",
              "kind": "agent",
              "position": 1,
              "config": {
                "prompt_template": "Implement: {{spec_content}}",
                "timeout_minutes": 30
              }
            },
            {
              "name": "committer",
              "kind": "agent",
              "position": 4,
              "config": {
                "prompt_template": "Review the feedback and commit if satisfied.",
                "commit_rights": true,
                "timeout_minutes": 15
              }
            }
          ],
          "edges": [],
          "ensembles": [
            {
              "name": "reviewers",
              "prompt_template": "Review this diff for correctness: {{previous_feedback}}",
              "entry_from_node": "implementer",
              "entry_condition": "always",
              "on_pass_to": "committer",
              "min_pass": 2,
              "timeout_minutes": 20,
              "members": [
                {},
                {}
              ]
            }
          ]
        }"#;

        let document = parse_export_document_str(raw).expect("doc example must parse");
        let plan = build_import_plan(&document, "loop-from-docs").expect("doc example must import");
        assert_eq!(plan.nodes.len(), 2);
        assert_eq!(plan.ensembles.len(), 1);
        assert_eq!(plan.ensembles[0].member_nodes.len(), 2);
        let missing = agent_nodes_missing_platform(&plan);
        assert_eq!(
            missing.len(),
            4,
            "implementer, committer, and both reviewer members are missing a platform: {missing:?}"
        );
    }

    #[test]
    fn import_plan_rejects_ensemble_with_too_few_members() {
        let doc = LoopExportDocument {
            format_version: 1,
            name: "x".to_string(),
            description: None,
            nodes: vec![
                LoopExportNode {
                    name: "kickoff".to_string(),
                    kind: LoopNodeKind::Check,
                    position: 1,
                    config: serde_json::json!({"command": "true"}),
                },
                LoopExportNode {
                    name: "next".to_string(),
                    kind: LoopNodeKind::Check,
                    position: 2,
                    config: serde_json::json!({"command": "true"}),
                },
            ],
            edges: vec![],
            ensembles: vec![LoopExportEnsemble {
                name: "solo".to_string(),
                prompt_template: "go".to_string(),
                entry_from_node: "kickoff".to_string(),
                entry_condition: LoopEdgeCondition::Always,
                on_pass_to: "next".to_string(),
                on_fail_to: None,
                min_pass: 1,
                timeout_minutes: 30,
                straggler_timeout_minutes: None,
                members: vec![LoopExportEnsembleMember {
                    platform: Some("claude".to_string()),
                    model: None,
                    prompt_override: None,
                }],
            }],
        };
        let err = build_import_plan(&doc, "new-loop").unwrap_err();
        assert!(err.contains("2-8 members"));
    }
}
