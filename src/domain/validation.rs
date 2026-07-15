//! Domain validation rules for identifiers, prompts, and paths.

use crate::domain::loops::{EnsembleDetails, LoopEdge, LoopEdgeCondition, LoopNode};

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

        if details.members.len() < 2 {
            return Err(format!("{label} has fewer than 2 members."));
        }
        if ensemble.min_pass < 1 || ensemble.min_pass > details.members.len() as i64 {
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
                "{label}'s join node '{}' is missing from this graph.",
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
                "{label}'s join has no pass edge to its on_pass_to target."
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
                ensemble.entry_condition,
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
                    "{label}'s member '{}' is not wired to the join — every member must route to the join.",
                    member.node_id
                ));
            }
        }
    }

    Ok(())
}

#[cfg(test)]
#[path = "validation_tests.rs"]
mod tests;
