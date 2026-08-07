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
}
