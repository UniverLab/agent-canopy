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

    /// CM14: a quorum fanned out to another ensemble's members validates when
    /// the row holds the target's quorum id — chaining needs no intermediate
    /// node.
    #[test]
    fn chained_exit_into_another_ensemble_passes() {
        let (mut details, mut nodes, mut edges) = valid_fixture();
        // Second ensemble: gate -> {r1} -> join2 -> arbiter. Single member is
        // fine for a cascade unit.
        let target = Ensemble {
            id: "ens2".to_string(),
            spec_id: Some("spec".to_string()),
            loop_id: None,
            name: "Reviewers".to_string(),
            prompt_template: "{{spec_content}}".to_string(),
            join_node_id: "join2".to_string(),
            entry_from_node: "gate".to_string(),
            entry_condition: LoopEdgeCondition::Always,
            min_pass: 1,
            straggler_timeout_minutes: None,
            timeout_minutes: 30,
            on_pass_to: "arbiter".to_string(),
            on_fail_to: None,
            kind: crate::domain::loops::EnsembleKind::Cascade,
            round_robin_index: None,
            created_at: Utc::now(),
        };
        let target_members = vec![EnsembleMember {
            ensemble_id: "ens2".to_string(),
            node_id: "r1".to_string(),
            position: 0,
            platform: "claude".to_string(),
            model: None,
            prompt_override: None,
        }];
        // ens1's quorum now routes into ens2 instead of the arbiter.
        details[0].ensemble.on_pass_to = "join2".to_string();
        details.push(EnsembleDetails {
            ensemble: target,
            members: target_members,
        });
        nodes.extend([node("gate"), node("r1"), node("join2")]);
        edges.retain(|e| !(e.from_node == "join1" && e.to_node == "arbiter"));
        edges.extend([
            edge("join1", "r1", LoopEdgeCondition::Pass),
            edge("gate", "r1", LoopEdgeCondition::Always),
            edge("r1", "join2", LoopEdgeCondition::Always),
            edge("join2", "arbiter", LoopEdgeCondition::Pass),
        ]);
        assert!(validate_ensembles_in_graph(&details, &nodes, &edges).is_ok());
    }

    /// CM14: a chained exit missing one member's edge is rejected.
    #[test]
    fn chained_exit_missing_a_member_edge_is_rejected() {
        let (mut details, mut nodes, mut edges) = valid_fixture();
        let target = Ensemble {
            id: "ens2".to_string(),
            spec_id: Some("spec".to_string()),
            loop_id: None,
            name: "Reviewers".to_string(),
            prompt_template: "{{spec_content}}".to_string(),
            join_node_id: "join2".to_string(),
            entry_from_node: "gate".to_string(),
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
        let target_members = vec![
            EnsembleMember {
                ensemble_id: "ens2".to_string(),
                node_id: "r1".to_string(),
                position: 0,
                platform: "claude".to_string(),
                model: None,
                prompt_override: None,
            },
            EnsembleMember {
                ensemble_id: "ens2".to_string(),
                node_id: "r2".to_string(),
                position: 1,
                platform: "codex".to_string(),
                model: None,
                prompt_override: None,
            },
        ];
        details[0].ensemble.on_pass_to = "join2".to_string();
        details.push(EnsembleDetails {
            ensemble: target,
            members: target_members,
        });
        nodes.extend([node("gate"), node("r1"), node("r2"), node("join2")]);
        edges.retain(|e| !(e.from_node == "join1" && e.to_node == "arbiter"));
        // Only r1 gets the fan-out edge — r2 is missing.
        edges.extend([
            edge("join1", "r1", LoopEdgeCondition::Pass),
            edge("gate", "r1", LoopEdgeCondition::Always),
            edge("gate", "r2", LoopEdgeCondition::Always),
            edge("r1", "join2", LoopEdgeCondition::Always),
            edge("r2", "join2", LoopEdgeCondition::Always),
            edge("join2", "arbiter", LoopEdgeCondition::Pass),
        ]);
        let err = validate_ensembles_in_graph(&details, &nodes, &edges).unwrap_err();
        assert!(err.contains("no pass edge"), "{err}");
    }

    /// CM14: an entry source reaching only some members is rejected — entering
    /// from it would be ambiguous at runtime.
    #[test]
    fn incomplete_entry_source_is_rejected() {
        let (details, nodes, mut edges) = valid_fixture();
        edges.push(edge("gate", "m1", LoopEdgeCondition::Always));
        let mut nodes = nodes;
        nodes.push(node("gate"));
        let err = validate_ensembles_in_graph(&details, &nodes, &edges).unwrap_err();
        assert!(err.contains("incomplete entry wiring"), "{err}");
    }

    /// CM14: every entry source reaching every member validates.
    #[test]
    fn complete_multi_source_entry_passes() {
        let (details, nodes, mut edges) = valid_fixture();
        edges.extend([
            edge("gate", "m1", LoopEdgeCondition::Always),
            edge("gate", "m2", LoopEdgeCondition::Always),
        ]);
        let mut nodes = nodes;
        nodes.push(node("gate"));
        assert!(validate_ensembles_in_graph(&details, &nodes, &edges).is_ok());
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
    fn accepts_agent_node_missing_fail_edge_as_terminal() {
        // A (agent) -> B (agent), only pass edge from A (fail is terminal)
        let pass = LoopEdgeCondition::Pass;
        let nodes = vec![agent_node("resilience"), agent_node("B")];
        let edges = vec![edge("resilience", "B", &pass)];
        let report = validate_loop_graph(&nodes, &edges).expect("should validate");
        assert!(
            report
                .terminals
                .iter()
                .any(|t| t.node_id == "resilience" && t.state == "fail"),
            "terminals should include resilience/fail: {:?}",
            report.terminals
        );
    }

    #[test]
    fn accepts_agent_node_missing_pass_edge_as_terminal() {
        let fail = LoopEdgeCondition::Fail;
        let nodes = vec![agent_node("resilience"), agent_node("B")];
        let edges = vec![edge("resilience", "B", &fail)];
        let report = validate_loop_graph(&nodes, &edges).expect("should validate");
        assert!(
            report
                .terminals
                .iter()
                .any(|t| t.node_id == "resilience" && t.state == "pass"),
            "terminals should include resilience/pass: {:?}",
            report.terminals
        );
    }

    /// Spec guideline: "A graph whose only 'violation' is a node with no `fail` edge
    /// validates, and the terminal appears in the reported list."
    #[test]
    fn terminal_reported_for_missing_fail_edge() {
        let pass = LoopEdgeCondition::Pass;
        let nodes = vec![agent_node("A"), agent_node("B")];
        let edges = vec![edge("A", "B", &pass)];
        let report = validate_loop_graph(&nodes, &edges).unwrap();
        assert!(report
            .terminals
            .iter()
            .any(|t| t.node_id == "A" && t.state == "fail"));
    }

    #[test]
    fn fail_dead_end_reported_for_non_terminal_node_missing_fail_edge() {
        // A (entry) --pass--> B, A --fail--> B ; B --pass--> C ; C terminal.
        // B continues on pass but has no fail edge -> one fail dead end (B).
        // C has no outgoing edges at all -> deliberate terminal, NOT reported.
        let pass = LoopEdgeCondition::Pass;
        let fail = LoopEdgeCondition::Fail;
        let nodes = vec![agent_node("A"), agent_node("B"), agent_node("C")];
        let edges = vec![
            edge("A", "B", &pass),
            edge("A", "B", &fail),
            edge("B", "C", &pass),
        ];
        let report = validate_loop_graph(&nodes, &edges).expect("should validate");
        assert_eq!(
            report.fail_dead_ends.len(),
            1,
            "exactly one fail dead end: {:?}",
            report.fail_dead_ends
        );
        assert_eq!(report.fail_dead_ends[0].node_id, "B");
        assert!(!report.fail_dead_ends[0].has_break_path);
    }

    #[test]
    fn declared_terminal_missing_fail_edge_is_not_a_fail_dead_end() {
        // A (entry) --pass--> B ; B has no outgoing edges: a deliberate
        // terminal on BOTH pass and fail. B must never be a fail dead end.
        let pass = LoopEdgeCondition::Pass;
        let nodes = vec![agent_node("A"), agent_node("B")];
        let edges = vec![edge("A", "B", &pass)];
        let report = validate_loop_graph(&nodes, &edges).expect("should validate");
        assert!(
            report.fail_dead_ends.iter().all(|d| d.node_id != "B"),
            "B is a declared terminal, not a fail dead end: {:?}",
            report.fail_dead_ends
        );
    }

    #[test]
    fn no_fail_dead_ends_when_every_node_has_a_failure_path() {
        // A --always--> B ; A routes every status, B is a pure terminal.
        let always = LoopEdgeCondition::Always;
        let nodes = vec![agent_node("A"), agent_node("B")];
        let edges = vec![edge("A", "B", &always)];
        let report = validate_loop_graph(&nodes, &edges).expect("should validate");
        assert!(
            report.fail_dead_ends.is_empty(),
            "no fail dead ends expected: {:?}",
            report.fail_dead_ends
        );
    }

    #[test]
    fn fail_dead_end_notes_break_edge_when_present() {
        // B has pass + break but no fail/always: break covers infra failures
        // but a real fail verdict still dead-ends. has_break_path must be true.
        let pass = LoopEdgeCondition::Pass;
        let brk = LoopEdgeCondition::Break;
        let fail = LoopEdgeCondition::Fail;
        let nodes = vec![agent_node("A"), agent_node("B"), agent_node("C")];
        let edges = vec![
            edge("A", "B", &pass),
            edge("A", "B", &fail),
            edge("B", "C", &pass),
            edge("B", "C", &brk),
        ];
        let report = validate_loop_graph(&nodes, &edges).expect("should validate");
        let b = report
            .fail_dead_ends
            .iter()
            .find(|d| d.node_id == "B")
            .expect("B is a fail dead end");
        assert!(b.has_break_path, "B has a break edge");
    }

    /// Spec guideline: "The real shape of `cascade-v2-sonnet-fixes` — a resilience
    /// node ending on `fail`, and a last node ending on `pass` — validates as a fixture."
    #[test]
    fn accepts_cascade_shape_with_two_terminals() {
        // Entry -> Resilience -[pass]-> Next -[pass]-> Last (leaf)
        // Resilience has no fail edge (terminal), Last has no outgoing (both terminals)
        let pass = LoopEdgeCondition::Pass;
        let nodes = vec![
            agent_node("Entry"),
            agent_node("Resilience"),
            agent_node("Next"),
            agent_node("Last"),
        ];
        let edges = vec![
            edge("Entry", "Resilience", &pass),
            edge("Resilience", "Next", &pass),
            edge("Next", "Last", &pass),
        ];
        let report = validate_loop_graph(&nodes, &edges).expect("should validate");
        // Resilience ends on fail
        assert!(report
            .terminals
            .iter()
            .any(|t| t.node_id == "Resilience" && t.state == "fail"));
        // Last ends on both pass and fail (leaf node)
        assert!(report
            .terminals
            .iter()
            .any(|t| t.node_id == "Last" && t.state == "pass"));
        assert!(report
            .terminals
            .iter()
            .any(|t| t.node_id == "Last" && t.state == "fail"));
    }

    /// Spec guideline: "A graph with two terminals reports both in a single call."
    #[test]
    fn reports_multiple_terminals_in_single_call() {
        // A -[pass]-> B, A -[fail]-> C, B and C are leaves (both terminals)
        let pass = LoopEdgeCondition::Pass;
        let fail = LoopEdgeCondition::Fail;
        let nodes = vec![agent_node("A"), agent_node("B"), agent_node("C")];
        let edges = vec![edge("A", "B", &pass), edge("A", "C", &fail)];
        let report = validate_loop_graph(&nodes, &edges).unwrap();
        // B and C are leaves so they each have both pass and fail terminals
        assert!(report
            .terminals
            .iter()
            .any(|t| t.node_id == "B" && t.state == "pass"));
        assert!(report
            .terminals
            .iter()
            .any(|t| t.node_id == "B" && t.state == "fail"));
        assert!(report
            .terminals
            .iter()
            .any(|t| t.node_id == "C" && t.state == "pass"));
        assert!(report
            .terminals
            .iter()
            .any(|t| t.node_id == "C" && t.state == "fail"));
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
    fn rejects_router_edge_for_undeclared_route() {
        let route = LoopEdgeCondition::Route("unexpected".to_string());
        let labels = vec!["approve".to_string()];
        let nodes = vec![router_node("router", &labels), agent_node("next")];
        let edges = vec![edge("router", "next", &route)];
        let err = validate_loop_graph(&nodes, &edges).unwrap_err();
        assert!(err.contains("undeclared route"), "err: {err}");
        assert!(err.contains("unexpected"), "err: {err}");
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

    /// CM2: `Break` alone (without `Pass`) — pass is a terminal, not an error.
    #[test]
    fn accepts_agent_node_with_only_break_edge_pass_is_terminal() {
        let brk = LoopEdgeCondition::Break;
        let nodes = vec![agent_node("A"), agent_node("B")];
        let edges = vec![edge("A", "B", &brk)];
        let report = validate_loop_graph(&nodes, &edges).expect("should validate");
        // A has break (covers fail) but no pass — pass is terminal
        assert!(report
            .terminals
            .iter()
            .any(|t| t.node_id == "A" && t.state == "pass"));
    }

    #[test]
    fn ensemble_min_pass_valid_boundary_values() {
        // 1 is valid for any count
        assert!(validate_ensemble_min_pass("E", 1, 3).is_ok());
        // member_count itself is valid
        assert!(validate_ensemble_min_pass("E", 3, 3).is_ok());
    }

    #[test]
    fn ensemble_min_pass_zero_is_rejected() {
        let err = validate_ensemble_min_pass("MyEns", 0, 3).unwrap_err();
        assert!(err.contains("MyEns"));
        assert!(err.contains('0'));
        assert!(err.contains('3'));
    }

    #[test]
    fn ensemble_min_pass_above_count_is_rejected() {
        let err = validate_ensemble_min_pass("MyEns", 4, 3).unwrap_err();
        assert!(err.contains("MyEns"));
        assert!(err.contains('4'));
        assert!(err.contains('3'));
    }
}
