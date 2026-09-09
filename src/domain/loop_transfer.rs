//! Loop export/import: a loop's design — name, description, nodes, edges,
//! and ensembles — serialized to and from one portable JSON document, so a
//! loop can leave one machine as a file and be recreated on another (spec
//! f0be8c66's successor: "a loop is worth sharing, not just narrating").
//!
//! Deliberately excludes ids, workdir, specs, and run/status state (a loop
//! design carrying someone else's backlog or run history would be a
//! surprise on arrival), and always includes `platform`/`model` (v2) so an
//! exported document round-trips its harness bindings.
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
    Ensemble, EnsembleDetails, EnsembleKind, EnsembleMember, Loop, LoopEdge, LoopEdgeCondition,
    LoopNode, LoopNodeKind,
};
use crate::domain::validation::{
    validate_ensembles_in_graph, validate_loop_graph, GraphEdgeView, GraphNodeView,
};

/// The latest `format_version` this build writes. `loop_import` accepts both
/// `1` (members with no binding) and `2` (bindings included). An
/// unrecognized or missing version is a refusal, never a best-effort parse
/// (decision 6).
pub const LOOP_EXPORT_FORMAT_VERSION: i64 = 2;

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
    /// CM2: optional pre-wired target for infrastructure failures, referenced
    /// by node name (not id) — `None` when the loop has no infra node.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub infra_node: Option<String>,
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
    #[serde(default)]
    pub platform: Option<String>,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub prompt_override: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LoopExportEnsemble {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
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
            config: export_node_config(node.kind, &node.config),
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

        let mut sorted_members: Vec<&EnsembleMember> = details.members.iter().collect();
        sorted_members.sort_by_key(|m| m.position);
        let members = sorted_members
            .iter()
            .map(|member| LoopExportEnsembleMember {
                platform: if member.platform.is_empty() {
                    None
                } else {
                    Some(member.platform.clone())
                },
                model: member.model.clone(),
                prompt_override: member.prompt_override.clone(),
            })
            .collect();

        export_ensembles.push(LoopExportEnsemble {
            name: ensemble.name.clone(),
            kind: if ensemble.kind == EnsembleKind::Parallel {
                None
            } else {
                Some(ensemble.kind.as_str().to_string())
            },
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

    let infra_node = lp.infra_node_id.as_deref().map(resolve_name).transpose()?;

    Ok(LoopExportDocument {
        format_version: LOOP_EXPORT_FORMAT_VERSION,
        name: lp.name.clone(),
        description: lp.description.clone(),
        nodes: export_nodes,
        edges: export_edges,
        ensembles: export_ensembles,
        infra_node,
    })
}

/// An agent node's config passes through untouched, except that a missing
/// `model` key is injected as `null` so the export states "platform
/// default" explicitly, field-for-field with `loop_get`. Every other kind's
/// config passes through verbatim.
fn export_node_config(kind: LoopNodeKind, config: &Value) -> Value {
    if kind != LoopNodeKind::Agent {
        return config.clone();
    }
    if let Some(map) = config.as_object() {
        if map.contains_key("model") {
            return config.clone();
        }
        let mut map = map.clone();
        map.insert("model".to_string(), Value::Null);
        return Value::Object(map);
    }
    config.clone()
}

fn extract_router_labels(config: &Value) -> Vec<String> {
    config
        .get("routes")
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(|entry| {
                    entry
                        .get("label")
                        .and_then(Value::as_str)
                        .map(|s| s.to_string())
                })
                .collect()
        })
        .unwrap_or_default()
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
        Some(1) | Some(2) => {}
        Some(other) => {
            return Err(format!(
                "Loop export document has format_version {other}, but this build only supports 1 and {LOOP_EXPORT_FORMAT_VERSION}."
            ))
        }
        None => {
            return Err(
                "Loop export document is missing format_version; refusing to guess. Expected format_version: 2."
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
    /// Terminal exits reported by structural validation, formatted for
    /// operators with the node name and id.
    pub terminals: Vec<String>,
    /// CM2: resolved infra node id (from the document's `infra_node` name),
    /// or `None` when the document has no infra node.
    pub infra_node_id: Option<String>,
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
    if document.format_version != 1 && document.format_version != LOOP_EXPORT_FORMAT_VERSION {
        return Err(format!(
            "Loop export document has format_version {}, but this build only supports 1 and {}.",
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

    for doc_node in &document.nodes {
        if let Some(prompt) = doc_node
            .config
            .get("prompt_template")
            .and_then(|v| v.as_str())
        {
            let mut rest = prompt;
            while let Some(start) = rest.find("{{output:") {
                let after_prefix = &rest[start + 9..];
                if let Some(end) = after_prefix.find("}}") {
                    let referenced_name = &after_prefix[..end];
                    if !name_to_id.contains_key(referenced_name) {
                        return Err(format!(
                            "Node '{}' references unknown node '{}' in {{{{output:{}}}}}.",
                            doc_node.name, referenced_name, referenced_name
                        ));
                    }
                    rest = &after_prefix[end + 2..];
                } else {
                    break;
                }
            }
        }
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
        let import_kind = doc_ensemble
            .kind
            .as_deref()
            .and_then(EnsembleKind::from_str)
            .unwrap_or(EnsembleKind::Parallel);
        let min_members = match import_kind {
            EnsembleKind::Parallel => ENSEMBLE_MIN_MEMBERS,
            EnsembleKind::Cascade | EnsembleKind::RoundRobin => 1,
        };
        if doc_ensemble.members.len() < min_members
            || doc_ensemble.members.len() > ENSEMBLE_MAX_MEMBERS
        {
            return Err(format!(
                "Ensemble '{}' must have {min_members}-{ENSEMBLE_MAX_MEMBERS} members, got {}.",
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

        let import_kind = doc_ensemble
            .kind
            .as_deref()
            .and_then(EnsembleKind::from_str)
            .unwrap_or(EnsembleKind::Parallel);

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
                kind: import_kind,
                round_robin_index: if import_kind == EnsembleKind::RoundRobin {
                    Some(0)
                } else {
                    None
                },
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

    // Graph-level structural validation (CB8): validate the complete expanded
    // graph as a whole. This catches unreachable nodes, multiple entry points,
    // missing outgoing coverage, and dangling edges — properties that only a
    // whole-graph check can see. For ensembles, the expanded graph includes
    // member/join nodes and wiring, so a document whose plain nodes appear
    // disconnected (kickoff/downstream with only ensemble bridging them) is
    // correctly considered connected.
    let terminals = {
        let router_labels: Vec<Vec<String>> = all_nodes
            .iter()
            .map(|n| {
                if n.kind == LoopNodeKind::Router {
                    extract_router_labels(&n.config)
                } else {
                    Vec::new()
                }
            })
            .collect();
        let node_views: Vec<GraphNodeView> = all_nodes
            .iter()
            .enumerate()
            .map(|(idx, n)| GraphNodeView {
                id: &n.id,
                kind: n.kind,
                route_labels: &router_labels[idx],
            })
            .collect();
        let edge_views: Vec<GraphEdgeView> = edges
            .iter()
            .map(|e| GraphEdgeView {
                from: &e.from_node,
                to: &e.to_node,
                condition: &e.condition,
            })
            .collect();
        match validate_loop_graph(&node_views, &edge_views) {
            Ok(report) => report
                .terminals
                .into_iter()
                .map(|terminal| {
                    let display = all_nodes
                        .iter()
                        .find(|node| node.id == terminal.node_id)
                        .map(|node| format!("{} ({})", node.name, node.id))
                        .unwrap_or(terminal.node_id);
                    format!("{} ends on '{}'", display, terminal.state)
                })
                .collect(),
            Err(e) => {
                // For import, the document names are more actionable than fresh
                // UUIDs, so enrich the message with names where we can resolve them.
                let mut enriched = e;
                // Try to map UUIDs back to names for friendlier messages: build id->name map.
                let id_to_name: std::collections::HashMap<&str, &str> = all_nodes
                    .iter()
                    .map(|n| (n.id.as_str(), n.name.as_str()))
                    .collect();
                for (id, name) in id_to_name {
                    if enriched.contains(id) {
                        enriched = enriched.replace(id, &format!("{name} ({id})"));
                    }
                }
                return Err(enriched);
            }
        }
    };

    let infra_node_id = document.infra_node.as_deref().map(resolve).transpose()?;

    Ok(LoopImportPlan {
        nodes,
        edges,
        ensembles,
        terminals,
        infra_node_id,
    })
}

/// Names of every agent node (plain or ensemble member) left without a
/// `platform`/`cli` after import — a v1 document (or a hand-written one)
/// may legitimately arrive without one, so `loop_import`'s response calls
/// out exactly which nodes need one filled in before the loop can run
/// (requirement 4).
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
            paused_by_reconciliation: false,
            infra_node_id: None,
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
            hooks: std::collections::BTreeMap::new(),
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
            make_edge("e2", "n2", "n3", LoopEdgeCondition::Always),
        ];
        (lp, nodes, edges)
    }

    #[test]
    fn export_v2_always_includes_agent_bindings_and_null_model() {
        let (lp, nodes, edges) = simple_graph();
        let doc = build_export_document(&lp, &nodes, &edges, &[]).unwrap();
        assert_eq!(doc.format_version, LOOP_EXPORT_FORMAT_VERSION);
        let implementer = doc.nodes.iter().find(|n| n.name == "implementer").unwrap();
        assert_eq!(implementer.config["platform"], "claude");
        assert_eq!(implementer.config["model"], "opus");
        assert_eq!(implementer.config["prompt_template"], "implement it");
        // The committer stores no model (platform default): export states
        // it explicitly as null rather than omitting the key.
        let committer = doc.nodes.iter().find(|n| n.name == "committer").unwrap();
        assert_eq!(committer.config["platform"], "claude");
        assert!(committer.config.get("model").is_some());
        assert!(committer.config["model"].is_null());
        // Non-binding keys pass through untouched.
        assert_eq!(committer.config["prompt_template"], "commit it");
    }

    #[test]
    fn export_v2_members_carry_bindings_in_position_order() {
        let lp = make_loop("member-order-loop");
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
            "Team [1]",
            LoopNodeKind::Agent,
            serde_json::json!({"platform": "copilot", "prompt_template": "draft it", "timeout_minutes": 30}),
            2,
        );
        let member2 = make_node(
            "m2",
            "Team [2]",
            LoopNodeKind::Agent,
            serde_json::json!({"platform": "opencode", "model": "opencode-go/qwen3.7-plus", "prompt_template": "draft it", "timeout_minutes": 30}),
            3,
        );
        let member3 = make_node(
            "m3",
            "Team [3]",
            LoopNodeKind::Agent,
            serde_json::json!({"platform": "opencode", "model": "opencode/muse-spark", "prompt_template": "custom", "timeout_minutes": 30}),
            4,
        );
        let join = make_node(
            "join1",
            "Team (quorum)",
            LoopNodeKind::Join,
            serde_json::json!({"ensemble_id": "ens1"}),
            5,
        );
        let nodes = vec![kickoff, downstream, member1, member2, member3, join];
        let edges = vec![
            make_edge("e1", "kickoff", "m1", LoopEdgeCondition::Always),
            make_edge("e2", "kickoff", "m2", LoopEdgeCondition::Always),
            make_edge("e3", "kickoff", "m3", LoopEdgeCondition::Always),
            make_edge("e4", "m1", "join1", LoopEdgeCondition::Always),
            make_edge("e5", "m2", "join1", LoopEdgeCondition::Always),
            make_edge("e6", "m3", "join1", LoopEdgeCondition::Always),
            make_edge("e7", "join1", "downstream", LoopEdgeCondition::Pass),
        ];
        // Members stored out of position order on purpose: export must
        // still emit them in position order.
        let mut ensemble_details =
            make_ensemble_details("ens1", "join1", "kickoff", "downstream", &["m1", "m2"]);
        ensemble_details.members.push(EnsembleMember {
            ensemble_id: "ens1".to_string(),
            node_id: "m3".to_string(),
            position: 2,
            platform: "opencode".to_string(),
            model: Some("opencode/muse-spark".to_string()),
            prompt_override: Some("custom angle".to_string()),
        });
        ensemble_details.members[0] = EnsembleMember {
            ensemble_id: "ens1".to_string(),
            node_id: "m1".to_string(),
            position: 0,
            platform: "copilot".to_string(),
            model: None,
            prompt_override: None,
        };
        ensemble_details.members[1] = EnsembleMember {
            ensemble_id: "ens1".to_string(),
            node_id: "m2".to_string(),
            position: 1,
            platform: "opencode".to_string(),
            model: Some("opencode-go/qwen3.7-plus".to_string()),
            prompt_override: None,
        };
        // Shuffle the stored order so the test pins the sort, not the input.
        ensemble_details.members.swap(0, 2);
        ensemble_details.ensemble.name = "Team".to_string();

        let doc = build_export_document(&lp, &nodes, &edges, &[ensemble_details]).unwrap();
        let team = doc.ensembles.iter().find(|e| e.name == "Team").unwrap();
        assert_eq!(team.members.len(), 3);
        assert_eq!(team.members[0].platform.as_deref(), Some("copilot"));
        assert_eq!(team.members[0].model, None);
        assert_eq!(team.members[0].prompt_override, None);
        assert_eq!(team.members[1].platform.as_deref(), Some("opencode"));
        assert_eq!(
            team.members[1].model.as_deref(),
            Some("opencode-go/qwen3.7-plus")
        );
        assert_eq!(team.members[2].platform.as_deref(), Some("opencode"));
        assert_eq!(
            team.members[2].model.as_deref(),
            Some("opencode/muse-spark")
        );
        assert_eq!(
            team.members[2].prompt_override.as_deref(),
            Some("custom angle")
        );
        // Optional fields serialize as explicit nulls, never omitted keys.
        let raw = serde_json::to_string(&doc).unwrap();
        assert!(raw.contains("\"model\":null"));
        assert!(raw.contains("\"prompt_override\":null"));
        assert!(raw.contains("\"platform\":\"copilot\""));
    }

    #[test]
    fn export_rejects_duplicate_node_names() {
        let (lp, mut nodes, edges) = simple_graph();
        nodes[1].name = "implementer".to_string();
        let err = build_export_document(&lp, &nodes, &edges, &[]).unwrap_err();
        assert!(err.contains("implementer"));
        assert!(err.contains("duplicate"));
    }

    #[test]
    fn format_version_first_key_in_serialized_json() {
        let (lp, nodes, edges) = simple_graph();
        let doc = build_export_document(&lp, &nodes, &edges, &[]).unwrap();
        let raw = serde_json::to_string(&doc).unwrap();
        // struct field order drives serde_json's key order.
        assert!(raw.starts_with("{\"format_version\":2"));
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
            "format_version": 99, "name": "x", "nodes": [], "edges": [], "ensembles": []
        });
        let err = parse_export_document_value(&value).unwrap_err();
        assert!(err.contains("99"));
    }

    #[test]
    fn import_accepts_format_version_1_and_2() {
        for version in [1, 2] {
            let value = serde_json::json!({
                "format_version": version, "name": "x", "nodes": [], "edges": [], "ensembles": []
            });
            let doc = parse_export_document_value(&value)
                .unwrap_or_else(|e| panic!("version {version} must parse: {e}"));
            assert_eq!(doc.format_version, version);
        }
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
            infra_node: None,
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
            infra_node: None,
        };
        let err = build_import_plan(&doc, "new-loop").unwrap_err();
        assert!(err.contains("join"));
    }

    #[test]
    fn import_plan_assigns_fresh_ids_and_loop_id() {
        let (lp, nodes, edges) = simple_graph();
        let doc = build_export_document(&lp, &nodes, &edges, &[]).unwrap();
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
    fn agent_nodes_missing_platform_reports_unbound_v1_nodes() {
        // A v1-style document carries no bindings: every agent node is
        // reported. Built by hand (not via export, which always binds).
        let doc = LoopExportDocument {
            format_version: 1,
            name: "x".to_string(),
            description: None,
            nodes: vec![
                LoopExportNode {
                    name: "implementer".to_string(),
                    kind: LoopNodeKind::Agent,
                    position: 1,
                    config: serde_json::json!({"prompt_template": "implement it"}),
                },
                LoopExportNode {
                    name: "gate".to_string(),
                    kind: LoopNodeKind::Check,
                    position: 2,
                    config: serde_json::json!({"command": "cargo test"}),
                },
                LoopExportNode {
                    name: "committer".to_string(),
                    kind: LoopNodeKind::Agent,
                    position: 3,
                    config: serde_json::json!({"prompt_template": "commit it"}),
                },
            ],
            edges: vec![
                LoopExportEdge {
                    from_node: "implementer".to_string(),
                    to_node: "gate".to_string(),
                    condition: LoopEdgeCondition::Always,
                },
                LoopExportEdge {
                    from_node: "gate".to_string(),
                    to_node: "committer".to_string(),
                    condition: LoopEdgeCondition::Always,
                },
            ],
            ensembles: vec![],
            infra_node: None,
        };
        let plan = build_import_plan(&doc, "loop-2").unwrap();
        let missing = agent_nodes_missing_platform(&plan);
        assert_eq!(missing.len(), 2);
        assert!(missing.contains(&"implementer".to_string()));
        assert!(missing.contains(&"committer".to_string()));
    }

    #[test]
    fn agent_nodes_missing_platform_empty_for_v2_export() {
        let (lp, nodes, edges) = simple_graph();
        let doc = build_export_document(&lp, &nodes, &edges, &[]).unwrap();
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
            kind: EnsembleKind::Parallel,
            round_robin_index: None,
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
    /// trip, with bindings included on both ends) and the acceptance
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

        let first_export = build_export_document(&lp, &nodes, &edges, &ensembles).unwrap();

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
        let first_export = build_export_document(&lp, &nodes, &edges, &[]).unwrap();

        let plan = build_import_plan(&first_export, "loop-2").unwrap();
        let mut lp2 = make_loop("renamed-on-import");
        lp2.id = "loop-2".to_string();
        let second_export = build_export_document(&lp2, &plan.nodes, &plan.edges, &[]).unwrap();

        assert_ne!(first_export.name, second_export.name);
        assert_eq!(second_export.name, "renamed-on-import");
        assert_eq!(first_export.description, second_export.description);
        assert_eq!(first_export.nodes, second_export.nodes);
        assert_eq!(first_export.edges, second_export.edges);
        assert_eq!(first_export.ensembles, second_export.ensembles);
    }

    /// Export → import → export of a loop holding one multi-member ensemble
    /// with distinct platform/model per member plus a solo agent node:
    /// every harness binding must survive identically.
    #[test]
    fn v2_export_import_export_preserves_bindings() {
        let lp = make_loop("bindings-loop");
        let solo = make_node(
            "solo",
            "solo",
            LoopNodeKind::Agent,
            serde_json::json!({"platform": "opencode", "model": "opencode/muse-spark", "prompt_template": "go solo", "timeout_minutes": 15}),
            1,
        );
        let downstream = make_node(
            "downstream",
            "downstream",
            LoopNodeKind::Check,
            serde_json::json!({"command": "true"}),
            10,
        );
        let member1 = make_node(
            "m1",
            "Crew [1]",
            LoopNodeKind::Agent,
            serde_json::json!({"platform": "copilot", "prompt_template": "draft it", "timeout_minutes": 30}),
            2,
        );
        let member2 = make_node(
            "m2",
            "Crew [2]",
            LoopNodeKind::Agent,
            serde_json::json!({"platform": "opencode", "model": "opencode-go/qwen3.7-plus", "prompt_template": "draft it", "timeout_minutes": 30}),
            3,
        );
        let join = make_node(
            "join1",
            "Crew (quorum)",
            LoopNodeKind::Join,
            serde_json::json!({"ensemble_id": "ens1"}),
            4,
        );
        let nodes = vec![solo, downstream, member1, member2, join];
        let edges = vec![
            make_edge("e1", "solo", "m1", LoopEdgeCondition::Always),
            make_edge("e2", "solo", "m2", LoopEdgeCondition::Always),
            make_edge("e3", "m1", "join1", LoopEdgeCondition::Always),
            make_edge("e4", "m2", "join1", LoopEdgeCondition::Always),
            make_edge("e5", "join1", "downstream", LoopEdgeCondition::Pass),
        ];
        let mut details =
            make_ensemble_details("ens1", "join1", "solo", "downstream", &["m1", "m2"]);
        details.ensemble.name = "Crew".to_string();
        details.members[0].platform = "copilot".to_string();
        details.members[0].model = None;
        details.members[0].prompt_override = Some("first angle".to_string());
        details.members[1].platform = "opencode".to_string();
        details.members[1].model = Some("opencode-go/qwen3.7-plus".to_string());

        let first = build_export_document(&lp, &nodes, &edges, &[details]).unwrap();
        let plan = build_import_plan(&first, "loop-2").unwrap();
        assert!(agent_nodes_missing_platform(&plan).is_empty());

        let mut imported_all_nodes = plan.nodes.clone();
        let mut imported_details = Vec::new();
        for ens in &plan.ensembles {
            imported_all_nodes.push(ens.join_node.clone());
            imported_all_nodes.extend(ens.member_nodes.iter().cloned());
            imported_details.push(EnsembleDetails {
                ensemble: ens.ensemble.clone(),
                members: ens.members.clone(),
            });
        }
        let mut lp2 = make_loop("bindings-loop");
        lp2.id = "loop-2".to_string();
        let second =
            build_export_document(&lp2, &imported_all_nodes, &plan.edges, &imported_details)
                .unwrap();

        // Harness bindings identical across the round trip, node and member.
        let first_solo = first.nodes.iter().find(|n| n.name == "solo").unwrap();
        let second_solo = second.nodes.iter().find(|n| n.name == "solo").unwrap();
        assert_eq!(
            first_solo.config["platform"],
            second_solo.config["platform"]
        );
        assert_eq!(first_solo.config["model"], second_solo.config["model"]);
        assert_eq!(first.ensembles, second.ensembles);
        assert_eq!(first.nodes, second.nodes);
    }

    /// Import must not fail when the document names a platform the importing
    /// machine has not configured: the node is created as written, and
    /// reporting an unusable pair belongs to `loop_preflight`.
    #[test]
    fn import_v2_with_unconfigured_platform_succeeds() {
        let doc = LoopExportDocument {
            format_version: 2,
            name: "x".to_string(),
            description: None,
            nodes: vec![
                LoopExportNode {
                    name: "odd".to_string(),
                    kind: LoopNodeKind::Agent,
                    position: 1,
                    config: serde_json::json!({
                        "platform": "never-configured-xyz",
                        "model": "some-model-abc",
                        "prompt_template": "go",
                    }),
                },
                LoopExportNode {
                    name: "next".to_string(),
                    kind: LoopNodeKind::Check,
                    position: 2,
                    config: serde_json::json!({"command": "true"}),
                },
            ],
            edges: vec![LoopExportEdge {
                from_node: "odd".to_string(),
                to_node: "next".to_string(),
                condition: LoopEdgeCondition::Always,
            }],
            ensembles: vec![],
            infra_node: None,
        };
        let plan = build_import_plan(&doc, "new-loop")
            .expect("unconfigured platform must import, not error");
        let odd = plan.nodes.iter().find(|n| n.name == "odd").unwrap();
        assert_eq!(odd.config["platform"], "never-configured-xyz");
        assert_eq!(odd.config["model"], "some-model-abc");
    }

    /// A `format_version: 2` document must stay readable after later fields
    /// are added: unknown fields are ignored rather than rejected, at the
    /// document, node, and member levels.
    #[test]
    fn unknown_fields_are_ignored() {
        let value = serde_json::json!({
            "format_version": 2,
            "name": "future-loop",
            "future_field": 123,
            "nodes": [
                {"name": "a", "kind": "check", "position": 1, "config": {"command": "true"}, "future_node_field": "x"},
                {"name": "b", "kind": "check", "position": 2, "config": {"command": "true"}},
            ],
            "edges": [
                {"from_node": "a", "to_node": "b", "condition": "always"}
            ],
            "ensembles": [
                {
                    "name": "team",
                    "prompt_template": "go",
                    "entry_from_node": "a",
                    "entry_condition": "always",
                    "on_pass_to": "b",
                    "min_pass": 2,
                    "timeout_minutes": 30,
                    "members": [
                        {"platform": "claude", "model": null, "prompt_override": null, "future_member_field": [1, 2]},
                        {"platform": "claude", "model": "opus", "prompt_override": null}
                    ]
                }
            ]
        });
        let document = parse_export_document_value(&value)
            .expect("unknown future fields must not break parsing");
        let plan = build_import_plan(&document, "new-loop")
            .expect("unknown future fields must not break import");
        assert_eq!(plan.nodes.len(), 2);
        assert_eq!(plan.ensembles.len(), 1);
        assert_eq!(plan.ensembles[0].members[1].model.as_deref(), Some("opus"));
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
                kind: None,
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
            infra_node: None,
        };
        let err = build_import_plan(&doc, "new-loop").unwrap_err();
        assert!(err.contains("2-8 members"));
    }

    #[test]
    fn import_plan_rejects_unreachable_node() {
        // A -> B, C self-loop (single entry A, C unreachable)
        let doc = LoopExportDocument {
            format_version: 1,
            name: "x".to_string(),
            description: None,
            nodes: vec![
                LoopExportNode {
                    name: "A".to_string(),
                    kind: LoopNodeKind::Agent,
                    position: 1,
                    config: serde_json::json!({"platform": "claude", "prompt_template": "a"}),
                },
                LoopExportNode {
                    name: "B".to_string(),
                    kind: LoopNodeKind::Agent,
                    position: 2,
                    config: serde_json::json!({"platform": "claude", "prompt_template": "b"}),
                },
                LoopExportNode {
                    name: "C".to_string(),
                    kind: LoopNodeKind::Agent,
                    position: 3,
                    config: serde_json::json!({"platform": "claude", "prompt_template": "c"}),
                },
            ],
            edges: vec![
                LoopExportEdge {
                    from_node: "A".to_string(),
                    to_node: "B".to_string(),
                    condition: LoopEdgeCondition::Always,
                },
                LoopExportEdge {
                    from_node: "C".to_string(),
                    to_node: "C".to_string(),
                    condition: LoopEdgeCondition::Always,
                },
            ],
            ensembles: vec![],
            infra_node: None,
        };
        let err = build_import_plan(&doc, "new-loop").unwrap_err();
        assert!(err.contains('C'), "err should name C: {err}");
        assert!(
            err.to_lowercase().contains("unreachable"),
            "err should mention unreachable: {err}"
        );
    }

    #[test]
    fn import_plan_rejects_multiple_entry_points() {
        // A and B both with no incoming => multiple entries
        let doc = LoopExportDocument {
            format_version: 1,
            name: "x".to_string(),
            description: None,
            nodes: vec![
                LoopExportNode {
                    name: "A".to_string(),
                    kind: LoopNodeKind::Agent,
                    position: 1,
                    config: serde_json::json!({"platform": "claude", "prompt_template": "a"}),
                },
                LoopExportNode {
                    name: "B".to_string(),
                    kind: LoopNodeKind::Agent,
                    position: 2,
                    config: serde_json::json!({"platform": "claude", "prompt_template": "b"}),
                },
                LoopExportNode {
                    name: "C".to_string(),
                    kind: LoopNodeKind::Agent,
                    position: 3,
                    config: serde_json::json!({"platform": "claude", "prompt_template": "c"}),
                },
            ],
            edges: vec![LoopExportEdge {
                from_node: "A".to_string(),
                to_node: "C".to_string(),
                condition: LoopEdgeCondition::Always,
            }],
            ensembles: vec![],
            infra_node: None,
        };
        let err = build_import_plan(&doc, "new-loop").unwrap_err();
        assert!(err.to_lowercase().contains("entry"), "err: {err}");
        assert!(err.contains('A'), "err should name A: {err}");
        assert!(err.contains('B'), "err should name B: {err}");
    }

    #[test]
    fn import_plan_accepts_agent_missing_fail_edge_as_terminal() {
        // Resilience node with only pass edge -> fail is a terminal exit,
        // not an error (same verdict as loop_preflight).
        let doc = LoopExportDocument {
            format_version: 1,
            name: "x".to_string(),
            description: None,
            nodes: vec![
                LoopExportNode {
                    name: "resilience".to_string(),
                    kind: LoopNodeKind::Agent,
                    position: 1,
                    config: serde_json::json!({"platform": "claude", "prompt_template": "go"}),
                },
                LoopExportNode {
                    name: "next".to_string(),
                    kind: LoopNodeKind::Agent,
                    position: 2,
                    config: serde_json::json!({"platform": "claude", "prompt_template": "next"}),
                },
            ],
            edges: vec![LoopExportEdge {
                from_node: "resilience".to_string(),
                to_node: "next".to_string(),
                condition: LoopEdgeCondition::Pass,
            }],
            ensembles: vec![],
            infra_node: None,
        };
        let plan =
            build_import_plan(&doc, "new-loop").expect("missing fail is a terminal, not an error");
        assert_eq!(plan.nodes.len(), 2);
        assert_eq!(plan.terminals.len(), 3);
        assert!(plan
            .terminals
            .iter()
            .any(|terminal| terminal.starts_with("resilience (")
                && terminal.ends_with(") ends on 'fail'")));
    }

    #[test]
    fn build_import_plan_rejects_unknown_node_reference() {
        let doc = LoopExportDocument {
            format_version: LOOP_EXPORT_FORMAT_VERSION,
            name: "test".to_string(),
            description: None,
            nodes: vec![LoopExportNode {
                name: "Worker".to_string(),
                kind: LoopNodeKind::Agent,
                config: serde_json::json!({
                    "platform": "test",
                    "prompt_template": "See {{output:Nonexistent}}"
                }),
                position: 1,
            }],
            edges: vec![],
            ensembles: vec![],
            infra_node: None,
        };
        let err = build_import_plan(&doc, "new-loop").unwrap_err();
        assert!(
            err.contains("references unknown node 'Nonexistent'"),
            "err: {err}"
        );
    }

    #[test]
    fn build_import_plan_accepts_valid_node_reference() {
        let doc = LoopExportDocument {
            format_version: LOOP_EXPORT_FORMAT_VERSION,
            name: "test".to_string(),
            description: None,
            nodes: vec![
                LoopExportNode {
                    name: "Architect".to_string(),
                    kind: LoopNodeKind::Agent,
                    config: serde_json::json!({"platform": "test"}),
                    position: 1,
                },
                LoopExportNode {
                    name: "Implementer".to_string(),
                    kind: LoopNodeKind::Agent,
                    config: serde_json::json!({
                        "platform": "test",
                        "prompt_template": "Follow {{output:Architect}}"
                    }),
                    position: 2,
                },
                LoopExportNode {
                    name: "Resilience".to_string(),
                    kind: LoopNodeKind::Agent,
                    config: serde_json::json!({"platform": "test"}),
                    position: 3,
                },
            ],
            edges: vec![
                LoopExportEdge {
                    from_node: "Architect".to_string(),
                    to_node: "Implementer".to_string(),
                    condition: LoopEdgeCondition::Pass,
                },
                LoopExportEdge {
                    from_node: "Architect".to_string(),
                    to_node: "Resilience".to_string(),
                    condition: LoopEdgeCondition::Fail,
                },
                LoopExportEdge {
                    from_node: "Implementer".to_string(),
                    to_node: "Resilience".to_string(),
                    condition: LoopEdgeCondition::Pass,
                },
                LoopExportEdge {
                    from_node: "Implementer".to_string(),
                    to_node: "Resilience".to_string(),
                    condition: LoopEdgeCondition::Fail,
                },
            ],
            ensembles: vec![],
            infra_node: None,
        };
        let result = build_import_plan(&doc, "new-loop");
        assert!(
            result.is_ok(),
            "valid {{output:NodeName}} reference must be accepted: {:?}",
            result.err()
        );
    }
}
