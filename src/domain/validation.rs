//! Domain validation rules for identifiers, prompts, and paths.

use std::collections::{HashMap, HashSet};

use crate::domain::loops::{
    EnsembleDetails, EnsembleKind, LoopEdge, LoopEdgeCondition, LoopNode, LoopNodeKind,
};

pub const MAX_ID_LENGTH: usize = 64;
pub const MAX_PROMPT_LENGTH: usize = 50_000;
pub const MAX_PATH_LENGTH: usize = 4096;

/// Validate an identifier: non-empty, max length, alphanumeric + hyphens/underscores.
pub fn validate_id(id: &str) -> Result<(), String> {
    if id.is_empty() {
        return Err("ID cannot be empty".to_string());
    }
    if id.len() > MAX_ID_LENGTH {
        return Err(format!(
            "ID exceeds maximum length of {MAX_ID_LENGTH} characters"
        ));
    }
    if !id
        .chars()
        .all(|c| c.is_alphanumeric() || c == '-' || c == '_')
    {
        return Err(
            "ID must contain only alphanumeric characters, hyphens, and underscores".to_string(),
        );
    }
    Ok(())
}

/// Validate a prompt string: non-empty, max length.
pub fn validate_prompt(prompt: &str) -> Result<(), String> {
    if prompt.trim().is_empty() {
        return Err("Prompt cannot be empty".to_string());
    }
    if prompt.len() > MAX_PROMPT_LENGTH {
        return Err(format!(
            "Prompt exceeds maximum length of {MAX_PROMPT_LENGTH} characters"
        ));
    }
    Ok(())
}

/// Validate a path string: non-empty, max length, absolute.
pub fn validate_watch_path(path: &str) -> Result<(), String> {
    if path.trim().is_empty() {
        return Err("Path cannot be empty".to_string());
    }
    if path.len() > MAX_PATH_LENGTH {
        return Err(format!(
            "Path exceeds maximum length of {MAX_PATH_LENGTH} characters"
        ));
    }
    if !std::path::Path::new(path).is_absolute() {
        return Err("Path must be absolute".to_string());
    }
    Ok(())
}

/// Validate every ensemble (F1) found within one graph (a loop's top-level
/// graph, or a single spec's own graph — never both mixed together, since an
/// ensemble belongs to exactly one) as a unit: entry reachable, every member
/// wired to the join, and both exits wired to nodes that actually exist in
/// this same graph. Called at `loop_run` so a structurally broken ensemble
/// fails fast with an actionable message instead of surfacing as a runtime
/// "ambiguous outgoing edges" or "node not found" deep into a run.
///
/// In practice every one of these invariants is guaranteed by construction —
/// `loop_add_ensemble`/`loop_update_ensemble` are the only writers of
/// ensemble-owned nodes/edges — so this exists as defense in depth against a
/// future bug or direct DB edit, not because callers are expected to trip it
/// today.
pub fn validate_ensembles_in_graph(
    ensembles: &[EnsembleDetails],
    nodes: &[LoopNode],
    edges: &[LoopEdge],
) -> Result<(), String> {
    let node_exists = |id: &str| nodes.iter().any(|node| node.id == id);
    let has_edge = |from: &str, to: &str, condition: LoopEdgeCondition| {
        edges
            .iter()
            .any(|edge| edge.from_node == from && edge.to_node == to && edge.condition == condition)
    };

    for details in ensembles {
        let ensemble = &details.ensemble;
        let label = format!("Ensemble '{}' ('{}')", ensemble.id, ensemble.name);

        let min_members = match ensemble.kind {
            EnsembleKind::Parallel => 2,
            EnsembleKind::Cascade | EnsembleKind::RoundRobin => 1,
        };
        if details.members.len() < min_members {
            return Err(format!("{label} has fewer than {min_members} members."));
        }
        if ensemble.kind == EnsembleKind::Parallel
            && (ensemble.min_pass < 1 || ensemble.min_pass > details.members.len() as i64)
        {
            return Err(format!(
                "{label} has an invalid min_pass ({}) for {} members.",
                ensemble.min_pass,
                details.members.len()
            ));
        }
        if !node_exists(&ensemble.entry_from_node) {
            return Err(format!(
                "{label}'s entry node '{}' is not reachable in this graph.",
                ensemble.entry_from_node
            ));
        }
        if !node_exists(&ensemble.join_node_id) {
            return Err(format!(
                "{label}'s quorum node '{}' is missing from this graph.",
                ensemble.join_node_id
            ));
        }
        if !node_exists(&ensemble.on_pass_to) {
            return Err(format!(
                "{label}'s on_pass_to target '{}' is not wired into this graph.",
                ensemble.on_pass_to
            ));
        }
        if let Some(on_fail_to) = &ensemble.on_fail_to {
            if !node_exists(on_fail_to) {
                return Err(format!(
                    "{label}'s on_fail_to target '{on_fail_to}' is not wired into this graph."
                ));
            }
        }
        if !has_edge(
            &ensemble.join_node_id,
            &ensemble.on_pass_to,
            LoopEdgeCondition::Pass,
        ) {
            return Err(format!(
                "{label}'s quorum has no pass edge to its on_pass_to target."
            ));
        }
        for member in &details.members {
            if !node_exists(&member.node_id) {
                return Err(format!(
                    "{label}'s member node '{}' is missing from this graph.",
                    member.node_id
                ));
            }
            if !has_edge(
                &ensemble.entry_from_node,
                &member.node_id,
                ensemble.entry_condition.clone(),
            ) {
                return Err(format!(
                    "{label}'s member '{}' has no entry edge from '{}'.",
                    member.node_id, ensemble.entry_from_node
                ));
            }
            if !has_edge(
                &member.node_id,
                &ensemble.join_node_id,
                LoopEdgeCondition::Always,
            ) {
                return Err(format!(
                    "{label}'s member '{}' is not wired to the quorum — every member must route to the quorum.",
                    member.node_id
                ));
            }
        }
    }

    Ok(())
}

/// A node as seen by graph-level validation — identified by an opaque
/// string (a name in an import document, an id in a live graph).
pub struct GraphNodeView<'a> {
    pub id: &'a str,
    pub kind: LoopNodeKind,
    /// Declared route labels for router nodes; empty for every other kind.
    pub route_labels: &'a [String],
}

/// An edge as seen by graph-level validation.
pub struct GraphEdgeView<'a> {
    pub from: &'a str,
    pub to: &'a str,
    pub condition: &'a LoopEdgeCondition,
}

/// Validate structural properties of a complete loop graph. Returns `Err`
/// with a message naming the concrete node(s) or edge(s) involved.
///
/// Checks performed:
/// 1. Every edge references nodes that exist in the graph.
/// 2. Exactly one entry point (node with no incoming edges). Zero or 2+ is an error.
/// 3. Every node is reachable from the entry point via outgoing edges.
/// 4. Every agent/check/gate node has outgoing coverage for both `pass` and `fail`
///    (an `always` edge covers both). Every router node has an outgoing edge for
///    each of its declared routes. Join nodes are skipped (engine-managed).
pub fn validate_loop_graph(
    nodes: &[GraphNodeView<'_>],
    edges: &[GraphEdgeView<'_>],
) -> Result<(), String> {
    if nodes.is_empty() {
        return Ok(());
    }

    let node_ids: HashSet<&str> = nodes.iter().map(|n| n.id).collect();

    // 1. Edge target validation — every edge must reference existing nodes.
    for edge in edges {
        if !node_ids.contains(edge.from) {
            return Err(format!(
                "Edge '{}' -> '{}' references unknown node '{}'.",
                edge.from, edge.to, edge.from
            ));
        }
        if !node_ids.contains(edge.to) {
            return Err(format!(
                "Edge '{}' -> '{}' references unknown node '{}'.",
                edge.from, edge.to, edge.to
            ));
        }
    }

    // 2. Entry point check — exactly one node with no incoming edges.
    let incoming: HashSet<&str> = edges.iter().map(|e| e.to).collect();
    let entry_nodes: Vec<&GraphNodeView<'_>> =
        nodes.iter().filter(|n| !incoming.contains(n.id)).collect();

    let entry_id = match entry_nodes.as_slice() {
        [single] => single.id,
        [] => {
            return Err(
                "Graph has no entry point: every node has an incoming edge. Expected exactly one node with no incoming edges.".to_string()
            );
        }
        many => {
            let mut names: Vec<&str> = many.iter().map(|n| n.id).collect();
            names.sort_unstable();
            return Err(format!(
                "Graph has multiple entry points (nodes with no incoming edges): {}. Expected exactly one entry point.",
                names.join(", ")
            ));
        }
    };

    // 3. Reachability — every node reachable from entry via outgoing edges.
    let mut adjacency: HashMap<&str, Vec<&str>> = HashMap::new();
    for edge in edges {
        adjacency.entry(edge.from).or_default().push(edge.to);
    }
    let mut visited: HashSet<&str> = HashSet::new();
    let mut stack = vec![entry_id];
    visited.insert(entry_id);
    while let Some(current) = stack.pop() {
        if let Some(neighbors) = adjacency.get(current) {
            for neighbor in neighbors {
                if visited.insert(neighbor) {
                    stack.push(neighbor);
                }
            }
        }
    }
    let unreachable: Vec<&str> = nodes
        .iter()
        .filter(|n| !visited.contains(n.id))
        .map(|n| n.id)
        .collect();
    if !unreachable.is_empty() {
        let mut sorted = unreachable;
        sorted.sort_unstable();
        if sorted.len() == 1 {
            return Err(format!(
                "Node '{}' is unreachable from entry point '{}'.",
                sorted[0], entry_id
            ));
        }
        return Err(format!(
            "Nodes unreachable from entry point '{}': {}.",
            entry_id,
            sorted.join(", ")
        ));
    }

    // 4. Outgoing coverage.
    let mut outgoing: HashMap<&str, Vec<&LoopEdgeCondition>> = HashMap::new();
    for edge in edges {
        outgoing.entry(edge.from).or_default().push(edge.condition);
    }

    for node in nodes {
        if node.kind == LoopNodeKind::Join {
            continue;
        }
        let outgoing_for_node = outgoing.get(node.id);
        // Leaf nodes (no outgoing edges at all) terminate the spec — the
        // engine treats a missing outgoing as spec completion (pass) or
        // spec failure (fail) without requiring an explicit edge. Requiring
        // coverage there would make every linear chain invalid and break the
        // "existing valid graph still imports" non-functional requirement.
        // A router with no outgoing at all is also the valid "not yet wired"
        // state (see validate_router_route_coverage's early return).
        if outgoing_for_node.is_none() {
            continue;
        }
        if node.kind == LoopNodeKind::Router {
            let conds = outgoing_for_node.expect("just checked Some");
            // If this router has no route edges at all, it's not yet wired.
            let has_any_route = conds.iter().any(|c| c.route_label().is_some());
            if !has_any_route {
                continue;
            }
            for label in node.route_labels {
                let has_route = conds
                    .iter()
                    .any(|c| c.route_label() == Some(label.as_str()));
                if !has_route {
                    return Err(format!(
                        "Router node '{}' has no outgoing edge for route '{}'.",
                        node.id, label
                    ));
                }
            }
            continue;
        }
        // Agent / Check / Gate — must have both pass and fail coverage when
        // they have any outgoing at all (missing fail is the demonstrator
        // bug: the resilience node had no fail edge).
        if matches!(
            node.kind,
            LoopNodeKind::Agent | LoopNodeKind::Check | LoopNodeKind::Gate
        ) {
            let conds = outgoing_for_node.expect("just checked Some");
            let has_pass = conds
                .iter()
                .any(|c| **c == LoopEdgeCondition::Pass || **c == LoopEdgeCondition::Always);
            let has_fail = conds.iter().any(|c| {
                **c == LoopEdgeCondition::Fail
                    || **c == LoopEdgeCondition::Always
                    || **c == LoopEdgeCondition::Break
            });
            if !has_pass {
                return Err(format!(
                    "Node '{}' has no outgoing edge for state 'pass' (expected a 'pass' or 'always' edge).",
                    node.id
                ));
            }
            if !has_fail {
                return Err(format!(
                    "Node '{}' has no outgoing edge for state 'fail' (expected a 'fail' or 'always' edge).",
                    node.id
                ));
            }
        }
    }

    Ok(())
}

#[cfg(test)]
#[path = "validation_tests.rs"]
mod tests;
