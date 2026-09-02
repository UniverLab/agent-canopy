use super::*;

// ── validate_id ───────────────────────────────────────────────

#[test]
fn test_validate_id_valid() {
    assert!(validate_id("my-background_agent").is_ok());
    assert!(validate_id("task_123").is_ok());
    assert!(validate_id("a").is_ok());
    assert!(validate_id("ABC-def_456").is_ok());
}

#[test]
fn test_validate_id_empty() {
    assert!(validate_id("").is_err());
}

#[test]
fn test_validate_id_too_long() {
    let long_id = "a".repeat(MAX_ID_LENGTH + 1);
    assert!(validate_id(&long_id).is_err());
    let exact_id = "a".repeat(MAX_ID_LENGTH);
    assert!(validate_id(&exact_id).is_ok());
}

#[test]
fn test_validate_id_invalid_chars() {
    assert!(validate_id("has space").is_err());
    assert!(validate_id("has.dot").is_err());
    assert!(validate_id("has/slash").is_err());
    assert!(validate_id("has@at").is_err());
    assert!(validate_id("has\nnewline").is_err());
}

// ── validate_prompt ───────────────────────────────────────────

#[test]
fn test_validate_prompt_valid() {
    assert!(validate_prompt("Run the tests").is_ok());
    assert!(validate_prompt("a").is_ok());
}

#[test]
fn test_validate_prompt_empty() {
    assert!(validate_prompt("").is_err());
    assert!(validate_prompt("   ").is_err());
    assert!(validate_prompt("\t\n").is_err());
}

#[test]
fn test_validate_prompt_too_long() {
    let long = "x".repeat(MAX_PROMPT_LENGTH + 1);
    assert!(validate_prompt(&long).is_err());
    let exact = "x".repeat(MAX_PROMPT_LENGTH);
    assert!(validate_prompt(&exact).is_ok());
}

// ── validate_watch_path ───────────────────────────────────────

#[test]
fn test_validate_watch_path_valid() {
    assert!(validate_watch_path("/tmp/project").is_ok());
    assert!(validate_watch_path("/home/user/src").is_ok());
}

#[test]
fn test_validate_watch_path_empty() {
    assert!(validate_watch_path("").is_err());
    assert!(validate_watch_path("   ").is_err());
}

#[test]
fn test_validate_watch_path_relative() {
    assert!(validate_watch_path("relative/path").is_err());
    assert!(validate_watch_path("./here").is_err());
}

#[test]
fn test_validate_watch_path_too_long() {
    let long = format!("/{}", "a".repeat(MAX_PATH_LENGTH));
    assert!(validate_watch_path(&long).is_err());
}

#[test]
fn test_validate_watch_path_root() {
    assert!(validate_watch_path("/").is_ok());
}

#[test]
fn test_validate_watch_path_with_special_chars() {
    assert!(validate_watch_path("/tmp/my-file_123.txt").is_ok());
}

#[test]
fn test_validate_watch_path_with_spaces_is_ok() {
    // Absolute filesystem paths may legitimately contain spaces (e.g. a
    // workdir under "/Users/Jane Doe/project") — only emptiness, length, and
    // absoluteness are validated.
    assert!(validate_watch_path("/path with spaces").is_ok());
}

#[test]
fn test_validate_id_exact_length() {
    let exact = "a".repeat(MAX_ID_LENGTH);
    assert!(validate_id(&exact).is_ok());
}

#[test]
fn test_validate_prompt_exact_length() {
    let exact = "x".repeat(MAX_PROMPT_LENGTH);
    assert!(validate_prompt(&exact).is_ok());
}

// ── validate_ensembles_in_graph ─────────────────────────────────

mod ensemble_graph {
    use super::*;
    use crate::domain::loops::{Ensemble, EnsembleMember};
    use chrono::Utc;

    fn node(id: &str) -> LoopNode {
        LoopNode {
            id: id.to_string(),
            spec_id: Some("spec".to_string()),
            loop_id: None,
            name: id.to_string(),
            kind: crate::domain::loops::LoopNodeKind::Agent,
            config: serde_json::json!({}),
            position: 0,
            created_at: Utc::now(),
        }
    }

    fn edge(from: &str, to: &str, condition: LoopEdgeCondition) -> LoopEdge {
        LoopEdge {
            id: format!("{from}->{to}"),
            spec_id: Some("spec".to_string()),
            loop_id: None,
            from_node: from.to_string(),
            to_node: to.to_string(),
            condition,
        }
    }

    /// A well-formed ensemble: kickoff -> {m1, m2} -> join -> arbiter (pass).
    fn valid_fixture() -> (Vec<EnsembleDetails>, Vec<LoopNode>, Vec<LoopEdge>) {
        let ensemble = Ensemble {
            id: "ens1".to_string(),
            spec_id: Some("spec".to_string()),
            loop_id: None,
            name: "Proposers".to_string(),
            prompt_template: "{{spec_content}}".to_string(),
            join_node_id: "join1".to_string(),
            entry_from_node: "kickoff".to_string(),
            entry_condition: LoopEdgeCondition::Always,
            min_pass: 2,
            straggler_timeout_minutes: None,
            timeout_minutes: 30,
            on_pass_to: "arbiter".to_string(),
            on_fail_to: None,
            kind: crate::domain::loops::EnsembleKind::Parallel,
            round_robin_index: None,
            created_at: Utc::now(),
        };
        let members = vec![
            EnsembleMember {
                ensemble_id: "ens1".to_string(),
                node_id: "m1".to_string(),
                position: 0,
                platform: "claude".to_string(),
                model: None,
                prompt_override: None,
            },
            EnsembleMember {
                ensemble_id: "ens1".to_string(),
                node_id: "m2".to_string(),
                position: 1,
                platform: "codex".to_string(),
                model: None,
                prompt_override: None,
            },
        ];
        let details = vec![EnsembleDetails { ensemble, members }];
        let nodes = vec![
            node("kickoff"),
            node("m1"),
            node("m2"),
            node("join1"),
            node("arbiter"),
        ];
        let edges = vec![
            edge("kickoff", "m1", LoopEdgeCondition::Always),
            edge("kickoff", "m2", LoopEdgeCondition::Always),
            edge("m1", "join1", LoopEdgeCondition::Always),
            edge("m2", "join1", LoopEdgeCondition::Always),
            edge("join1", "arbiter", LoopEdgeCondition::Pass),
        ];
        (details, nodes, edges)
    }

    #[test]
    fn well_formed_ensemble_passes() {
        let (details, nodes, edges) = valid_fixture();
        assert!(validate_ensembles_in_graph(&details, &nodes, &edges).is_ok());
    }

    #[test]
    fn missing_member_to_join_edge_is_rejected() {
        let (details, nodes, mut edges) = valid_fixture();
        edges.retain(|e| !(e.from_node == "m2" && e.to_node == "join1"));
        let err = validate_ensembles_in_graph(&details, &nodes, &edges).unwrap_err();
        assert!(err.contains("not wired to the quorum"));
    }

    #[test]
    fn missing_entry_edge_is_rejected() {
        let (details, nodes, mut edges) = valid_fixture();
        edges.retain(|e| !(e.from_node == "kickoff" && e.to_node == "m1"));
        let err = validate_ensembles_in_graph(&details, &nodes, &edges).unwrap_err();
        assert!(err.contains("no entry edge"));
    }

    #[test]
    fn missing_on_pass_to_node_is_rejected() {
        let (details, mut nodes, edges) = valid_fixture();
        nodes.retain(|n| n.id != "arbiter");
        let err = validate_ensembles_in_graph(&details, &nodes, &edges).unwrap_err();
        assert!(err.contains("on_pass_to target"));
    }

    #[test]
    fn min_pass_above_member_count_is_rejected() {
        let (mut details, nodes, edges) = valid_fixture();
        details[0].ensemble.min_pass = 5;
        let err = validate_ensembles_in_graph(&details, &nodes, &edges).unwrap_err();
        assert!(err.contains("invalid min_pass"));
    }

    fn single_member_fixture(
        kind: crate::domain::loops::EnsembleKind,
    ) -> (Vec<EnsembleDetails>, Vec<LoopNode>, Vec<LoopEdge>) {
        let ensemble = Ensemble {
            id: "ens1".to_string(),
            spec_id: Some("spec".to_string()),
            loop_id: None,
            name: "Solo".to_string(),
            prompt_template: "{{spec_content}}".to_string(),
            join_node_id: "join1".to_string(),
            entry_from_node: "kickoff".to_string(),
            entry_condition: LoopEdgeCondition::Always,
            min_pass: 1,
            straggler_timeout_minutes: None,
            timeout_minutes: 30,
            on_pass_to: "sink".to_string(),
            on_fail_to: None,
            kind,
            round_robin_index: None,
            created_at: Utc::now(),
        };
        let members = vec![EnsembleMember {
            ensemble_id: "ens1".to_string(),
            node_id: "m1".to_string(),
            position: 0,
            platform: "claude".to_string(),
            model: None,
            prompt_override: None,
        }];
        let details = vec![EnsembleDetails { ensemble, members }];
        let nodes = vec![node("kickoff"), node("m1"), node("join1"), node("sink")];
        let edges = vec![
            edge("kickoff", "m1", LoopEdgeCondition::Always),
            edge("m1", "join1", LoopEdgeCondition::Always),
            edge("join1", "sink", LoopEdgeCondition::Pass),
        ];
        (details, nodes, edges)
    }

    #[test]
    fn cascade_ensemble_with_one_member_passes_graph_validation() {
        let (details, nodes, edges) =
            single_member_fixture(crate::domain::loops::EnsembleKind::Cascade);
        assert!(validate_ensembles_in_graph(&details, &nodes, &edges).is_ok());
    }

    #[test]
    fn round_robin_ensemble_with_one_member_passes_graph_validation() {
        let (details, nodes, edges) =
            single_member_fixture(crate::domain::loops::EnsembleKind::RoundRobin);
        assert!(validate_ensembles_in_graph(&details, &nodes, &edges).is_ok());
    }

    #[test]
    fn parallel_ensemble_with_one_member_still_rejected() {
        let (details, nodes, edges) =
            single_member_fixture(crate::domain::loops::EnsembleKind::Parallel);
        let err = validate_ensembles_in_graph(&details, &nodes, &edges).unwrap_err();
        assert!(err.contains("fewer than 2 members"));
    }
}

// ── validate_loop_graph (CB8) ───────────────────────────────────────

mod graph_validation {
    use super::*;
    use crate::domain::loops::LoopNodeKind;

    fn agent_node(id: &str) -> GraphNodeView<'_> {
        static EMPTY: &[String] = &[];
        GraphNodeView {
            id,
            kind: LoopNodeKind::Agent,
            route_labels: EMPTY,
        }
    }

    fn check_node(id: &str) -> GraphNodeView<'_> {
        static EMPTY: &[String] = &[];
        GraphNodeView {
            id,
            kind: LoopNodeKind::Check,
            route_labels: EMPTY,
        }
    }

    fn router_node<'a>(id: &'a str, labels: &'a [String]) -> GraphNodeView<'a> {
        GraphNodeView {
            id,
            kind: LoopNodeKind::Router,
            route_labels: labels,
        }
    }

    fn join_node(id: &str) -> GraphNodeView<'_> {
        static EMPTY: &[String] = &[];
        GraphNodeView {
            id,
            kind: LoopNodeKind::Join,
            route_labels: EMPTY,
        }
    }

    fn edge<'a>(from: &'a str, to: &'a str, condition: &'a LoopEdgeCondition) -> GraphEdgeView<'a> {
        GraphEdgeView {
            from,
            to,
            condition,
        }
    }

    #[test]
    fn rejects_graph_with_no_entry_point() {
        // Two nodes, cycle: A -> B -> A, every node has incoming.
        let always = LoopEdgeCondition::Always;
        let nodes = vec![agent_node("A"), agent_node("B")];
        let edges = vec![edge("A", "B", &always), edge("B", "A", &always)];
        let err = validate_loop_graph(&nodes, &edges).unwrap_err();
        assert!(
            err.to_lowercase().contains("no entry") || err.to_lowercase().contains("entry point"),
            "err: {err}"
        );
    }

    #[test]
    fn rejects_graph_with_multiple_entry_points() {
        // Three nodes: A (no incoming), B (no incoming), C (incoming from A)
        let always = LoopEdgeCondition::Always;
        let nodes = vec![agent_node("A"), agent_node("B"), agent_node("C")];
        let edges = vec![edge("A", "C", &always)];
        let err = validate_loop_graph(&nodes, &edges).unwrap_err();
        assert!(
            err.contains("multiple entry") || err.to_lowercase().contains("entry point"),
            "err: {err}"
        );
        assert!(err.contains('A'), "err should name A: {err}");
        assert!(err.contains('B'), "err should name B: {err}");
    }

    #[test]
    fn rejects_unreachable_node() {
        let always = LoopEdgeCondition::Always;
        // To get single-entry unreachable: A (entry) -> B, C self-loop so C has incoming but not reachable from A.
        let nodes2 = vec![agent_node("A"), agent_node("B"), agent_node("C")];
        let edges2 = vec![edge("A", "B", &always), edge("C", "C", &always)];
        let err2 = validate_loop_graph(&nodes2, &edges2).unwrap_err();
        assert!(err2.contains('C'), "err should name C: {err2}");
        assert!(
            err2.to_lowercase().contains("unreachable"),
            "err should mention unreachable: {err2}"
        );
    }

    #[test]
    fn rejects_agent_node_missing_fail_edge() {
        // A (agent) -> B (agent), only pass edge from A (fail missing)
        let pass = LoopEdgeCondition::Pass;
        let nodes = vec![agent_node("resilience"), agent_node("B")];
        let edges = vec![edge("resilience", "B", &pass)];
        let err = validate_loop_graph(&nodes, &edges).unwrap_err();
        assert!(err.contains("resilience"), "err should name node: {err}");
        assert!(
            err.to_lowercase().contains("fail"),
            "err should mention fail: {err}"
        );
    }

    #[test]
    fn rejects_agent_node_missing_pass_edge() {
        let fail = LoopEdgeCondition::Fail;
        let nodes = vec![agent_node("resilience"), agent_node("B")];
        let edges = vec![edge("resilience", "B", &fail)];
        let err = validate_loop_graph(&nodes, &edges).unwrap_err();
        assert!(err.contains("resilience"), "err: {err}");
        assert!(err.to_lowercase().contains("pass"), "err: {err}");
    }

    #[test]
    fn accepts_agent_node_with_always_edge() {
        // A -[always]-> B (always covers both pass and fail)
        let always = LoopEdgeCondition::Always;
        let nodes = vec![agent_node("A"), agent_node("B")];
        let edges = vec![edge("A", "B", &always)];
        assert!(validate_loop_graph(&nodes, &edges).is_ok());
    }

    #[test]
    fn rejects_router_missing_route_edge() {
        let always = LoopEdgeCondition::Always;
        let approve = LoopEdgeCondition::Route("approve".to_string());
        let labels = vec!["approve".to_string(), "reject".to_string()];
        let nodes = vec![
            router_node("router", &labels),
            agent_node("next"),
            agent_node("other"),
        ];
        let edges = vec![
            edge("router", "next", &approve),
            edge("next", "other", &always),
        ];
        let err = validate_loop_graph(&nodes, &edges).unwrap_err();
        assert!(err.contains("router"), "err: {err}");
        assert!(
            err.contains("reject"),
            "err should name missing route: {err}"
        );
    }

    #[test]
    fn rejects_edge_to_nonexistent_node() {
        let always = LoopEdgeCondition::Always;
        let nodes = vec![agent_node("A")];
        let edges = vec![edge("A", "ghost", &always)];
        let err = validate_loop_graph(&nodes, &edges).unwrap_err();
        assert!(err.contains("ghost"), "err: {err}");
    }

    #[test]
    fn accepts_valid_simple_graph() {
        // A (agent) -[always]-> B (check) -[always]-> C (agent leaf, exempt)
        let always = LoopEdgeCondition::Always;
        let nodes = vec![agent_node("A"), check_node("B"), agent_node("C")];
        let edges = vec![edge("A", "B", &always), edge("B", "C", &always)];
        assert!(validate_loop_graph(&nodes, &edges).is_ok());
    }

    #[test]
    fn empty_graph_is_valid() {
        let nodes: Vec<GraphNodeView> = vec![];
        let edges: Vec<GraphEdgeView> = vec![];
        assert!(validate_loop_graph(&nodes, &edges).is_ok());
    }

    #[test]
    fn join_nodes_are_skipped_for_outgoing_check() {
        // Join node with only a pass edge should not trigger missing-fail
        let always = LoopEdgeCondition::Always;
        let pass = LoopEdgeCondition::Pass;
        let nodes = vec![agent_node("A"), join_node("join"), agent_node("B")];
        let edges = vec![edge("A", "join", &always), edge("join", "B", &pass)];
        assert!(validate_loop_graph(&nodes, &edges).is_ok());
    }

    #[test]
    fn join_nodes_skipped_even_with_no_outgoing() {
        let always = LoopEdgeCondition::Always;
        let nodes = vec![agent_node("A"), join_node("join")];
        let edges = vec![edge("A", "join", &always)];
        assert!(validate_loop_graph(&nodes, &edges).is_ok());
    }

    /// CM2: `Break` edge counts toward fail coverage — a node with `Pass` +
    /// `Break` edges is valid (Break covers fail).
    #[test]
    fn accepts_agent_node_with_pass_and_break_edges() {
        let pass = LoopEdgeCondition::Pass;
        let brk = LoopEdgeCondition::Break;
        let nodes = vec![agent_node("A"), agent_node("B"), agent_node("C")];
        let edges = vec![edge("A", "B", &pass), edge("A", "C", &brk)];
        assert!(validate_loop_graph(&nodes, &edges).is_ok());
    }

    /// CM2: `Break` alone (without `Pass`) still fails validation — pass
    /// coverage is separate from fail coverage.
    #[test]
    fn rejects_agent_node_with_only_break_edge() {
        let brk = LoopEdgeCondition::Break;
        let nodes = vec![agent_node("A"), agent_node("B")];
        let edges = vec![edge("A", "B", &brk)];
        let err = validate_loop_graph(&nodes, &edges).unwrap_err();
        assert!(
            err.to_lowercase().contains("pass"),
            "err should mention missing pass: {err}"
        );
    }
}
