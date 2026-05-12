use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use anyhow::{anyhow, bail, Context, Result};
use serde_json::Value;
use tokio::process::Command;

use crate::application::notification_service::NotificationService;
use crate::db::Database;
use crate::domain::workflow::{
    WorkflowEdge, WorkflowNode, WorkflowNodeKind, WorkflowNodeRun, WorkflowRunStatus, WorkflowSpec,
    WorkflowSpecStatus, WorkflowStatus,
};

const DEFAULT_MAX_ITERATIONS_PER_NODE: usize = 10;

#[derive(Clone)]
pub struct WorkflowEngine {
    db: Arc<Database>,
    notification_service: Arc<dyn NotificationService>,
}

enum SpecExecutionOutcome {
    Completed,
    Paused,
    Failed(String),
}

struct NodeExecution {
    status: WorkflowRunStatus,
    output: Value,
    summary: String,
}

impl WorkflowEngine {
    pub fn new(db: Arc<Database>, notification_service: Arc<dyn NotificationService>) -> Self {
        Self {
            db,
            notification_service,
        }
    }

    pub fn start_background(self: Arc<Self>, workflow_id: String) {
        tokio::spawn(async move {
            if let Err(error) = self.run_workflow(workflow_id.clone()).await {
                tracing::error!("Workflow '{}' failed to run: {error:#}", workflow_id);
                let _ = self.fail_workflow(&workflow_id, &error.to_string());
            }
        });
    }

    pub fn request_pause(&self, workflow_id: &str) -> Result<bool> {
        let Some(workflow) = self.db.get_workflow(workflow_id)? else {
            return Ok(false);
        };

        match workflow.status {
            WorkflowStatus::Running => {
                self.db
                    .update_workflow_status(workflow_id, WorkflowStatus::Paused, None, None)
            }
            WorkflowStatus::Paused => Ok(true),
            _ => Ok(false),
        }
    }

    pub async fn run_workflow(&self, workflow_id: String) -> Result<()> {
        let Some(workflow) = self.db.get_workflow(&workflow_id)? else {
            bail!("Workflow '{}' not found.", workflow_id);
        };

        self.db.update_workflow_status(
            &workflow_id,
            WorkflowStatus::Running,
            Some(chrono::Utc::now()),
            None,
        )?;

        let specs = self.db.list_workflow_specs(&workflow_id)?;
        for spec in specs {
            if self.is_paused(&workflow_id)? {
                return Ok(());
            }
            if matches!(
                spec.status,
                WorkflowSpecStatus::Completed | WorkflowSpecStatus::Skipped
            ) {
                continue;
            }

            match self.run_spec(&workflow, &spec).await? {
                SpecExecutionOutcome::Completed => continue,
                SpecExecutionOutcome::Paused => return Ok(()),
                SpecExecutionOutcome::Failed(summary) => {
                    self.fail_workflow(&workflow_id, &summary)?;
                    return Ok(());
                }
            }
        }

        self.db.update_workflow_status(
            &workflow_id,
            WorkflowStatus::Completed,
            None,
            Some(chrono::Utc::now()),
        )?;
        self.notification_service
            .notify_task_completed(&workflow_id, true, Some(0));
        Ok(())
    }

    async fn run_spec(
        &self,
        workflow: &crate::domain::workflow::Workflow,
        spec: &WorkflowSpec,
    ) -> Result<SpecExecutionOutcome> {
        let Some(details) = self.db.get_workflow_details(&workflow.id)? else {
            bail!("Workflow '{}' disappeared during execution.", workflow.id);
        };
        let spec_details = details
            .specs
            .into_iter()
            .find(|item| item.spec.id == spec.id)
            .ok_or_else(|| anyhow!("Workflow spec '{}' not found.", spec.id))?;

        let nodes_by_id = spec_details
            .nodes
            .iter()
            .map(|node| (node.id.as_str(), node))
            .collect::<HashMap<_, _>>();
        let existing_runs = self.db.list_workflow_runs_for_spec(&spec.id)?;
        let (mut current_node_id, mut previous_output, mut iterations) =
            resolve_spec_start(&spec_details, spec, &existing_runs)?;

        self.db.update_workflow_spec_status(
            &spec.id,
            WorkflowSpecStatus::Running,
            Some(chrono::Utc::now()),
            None,
        )?;

        loop {
            if self.is_paused(&workflow.id)? {
                return Ok(SpecExecutionOutcome::Paused);
            }

            let iteration = iterations.entry(current_node_id.clone()).or_insert(0);
            *iteration += 1;
            if *iteration > DEFAULT_MAX_ITERATIONS_PER_NODE {
                let summary = format!(
                    "Spec '{}' exceeded max iterations for node '{}'.",
                    spec.name, current_node_id
                );
                self.db.update_workflow_spec_status(
                    &spec.id,
                    WorkflowSpecStatus::Failed,
                    None,
                    Some(chrono::Utc::now()),
                )?;
                return Ok(SpecExecutionOutcome::Failed(summary));
            }

            let node = nodes_by_id
                .get(current_node_id.as_str())
                .ok_or_else(|| anyhow!("Workflow node '{}' not found.", current_node_id))?;
            let execution = self
                .execute_node(workflow, spec, node, previous_output.as_ref())
                .await?;
            self.db.insert_workflow_run(&WorkflowNodeRun {
                id: uuid::Uuid::new_v4().to_string(),
                workflow_id: workflow.id.clone(),
                spec_id: spec.id.clone(),
                node_id: node.id.clone(),
                status: execution.status,
                input: previous_output.clone(),
                output: Some(execution.output.clone()),
                started_at: chrono::Utc::now(),
                completed_at: Some(chrono::Utc::now()),
                iteration: *iteration as i64,
            })?;

            if should_advance_to_next_spec(node, execution.status) {
                self.db.update_workflow_spec_status(
                    &spec.id,
                    WorkflowSpecStatus::Completed,
                    None,
                    Some(chrono::Utc::now()),
                )?;
                return Ok(SpecExecutionOutcome::Completed);
            }

            let next_node_id = select_next_node(&spec_details.edges, &node.id, execution.status)?
                .map(str::to_owned);

            match next_node_id {
                Some(next_node_id) => {
                    previous_output = Some(execution.output);
                    current_node_id = next_node_id;
                }
                None if execution.status == WorkflowRunStatus::Pass => {
                    self.db.update_workflow_spec_status(
                        &spec.id,
                        WorkflowSpecStatus::Completed,
                        None,
                        Some(chrono::Utc::now()),
                    )?;
                    return Ok(SpecExecutionOutcome::Completed);
                }
                None => {
                    self.db.update_workflow_spec_status(
                        &spec.id,
                        WorkflowSpecStatus::Failed,
                        None,
                        Some(chrono::Utc::now()),
                    )?;
                    return Ok(SpecExecutionOutcome::Failed(execution.summary));
                }
            }
        }
    }

    async fn execute_node(
        &self,
        workflow: &crate::domain::workflow::Workflow,
        spec: &WorkflowSpec,
        node: &WorkflowNode,
        previous_output: Option<&Value>,
    ) -> Result<NodeExecution> {
        match node.kind {
            WorkflowNodeKind::Check => execute_check_node(workflow, spec, node).await,
            WorkflowNodeKind::Gate => execute_gate_node(node, previous_output),
            WorkflowNodeKind::Agent => Ok(NodeExecution {
                status: WorkflowRunStatus::Fail,
                output: serde_json::json!({
                    "summary": "Agent nodes are not wired yet."
                }),
                summary: format!(
                    "Workflow node '{}' is an agent node and is not wired in this checkpoint.",
                    node.name
                ),
            }),
        }
    }

    fn is_paused(&self, workflow_id: &str) -> Result<bool> {
        Ok(self
            .db
            .get_workflow(workflow_id)?
            .is_some_and(|workflow| workflow.status == WorkflowStatus::Paused))
    }

    fn fail_workflow(&self, workflow_id: &str, summary: &str) -> Result<()> {
        self.db.update_workflow_status(
            workflow_id,
            WorkflowStatus::Failed,
            None,
            Some(chrono::Utc::now()),
        )?;
        self.notification_service
            .notify_task_failed(workflow_id, 1, summary);
        Ok(())
    }
}

async fn execute_check_node(
    workflow: &crate::domain::workflow::Workflow,
    spec: &WorkflowSpec,
    node: &WorkflowNode,
) -> Result<NodeExecution> {
    let command = node
        .config
        .get("command")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| anyhow!("Check node '{}' is missing a command.", node.name))?;
    let success_condition = node
        .config
        .get("success_condition")
        .and_then(Value::as_str)
        .unwrap_or("exit_code_0");
    let timeout_seconds = node
        .config
        .get("timeout_seconds")
        .and_then(Value::as_u64)
        .unwrap_or(120);

    let mut process = shell_command(command);
    process.current_dir(&workflow.workdir);
    let output = tokio::time::timeout(
        std::time::Duration::from_secs(timeout_seconds),
        process.output(),
    )
    .await
    .with_context(|| format!("Check node '{}' timed out.", node.name))??;

    let exit_code = output.status.code().unwrap_or(-1);
    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    let combined = if stderr.is_empty() {
        stdout.clone()
    } else if stdout.is_empty() {
        stderr.clone()
    } else {
        format!("{stdout}\n{stderr}")
    };
    let passed = evaluate_success_condition(success_condition, exit_code, &combined)?;

    Ok(NodeExecution {
        status: if passed {
            WorkflowRunStatus::Pass
        } else {
            WorkflowRunStatus::Fail
        },
        output: serde_json::json!({
            "kind": "check",
            "workflow_id": workflow.id,
            "spec_id": spec.id,
            "node_id": node.id,
            "command": command,
            "success_condition": success_condition,
            "exit_code": exit_code,
            "stdout": stdout,
            "stderr": stderr,
            "passed": passed,
        }),
        summary: format!(
            "Check node '{}' {}.",
            node.name,
            if passed { "passed" } else { "failed" }
        ),
    })
}

fn execute_gate_node(
    node: &WorkflowNode,
    previous_output: Option<&Value>,
) -> Result<NodeExecution> {
    let previous_output = previous_output
        .ok_or_else(|| anyhow!("Gate node '{}' requires previous node output.", node.name))?;
    let evaluate = node
        .config
        .get("evaluate")
        .and_then(Value::as_str)
        .unwrap_or("output_contains");
    let expected = node
        .config
        .get("value")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let haystack = serde_json::to_string(previous_output)?;

    let passed = match evaluate {
        "output_contains" => haystack.contains(expected),
        other => bail!("Unsupported gate evaluate mode '{}'.", other),
    };

    Ok(NodeExecution {
        status: if passed {
            WorkflowRunStatus::Pass
        } else {
            WorkflowRunStatus::Fail
        },
        output: serde_json::json!({
            "kind": "gate",
            "node_id": node.id,
            "evaluate": evaluate,
            "value": expected,
            "passed": passed,
        }),
        summary: format!(
            "Gate node '{}' {}.",
            node.name,
            if passed { "passed" } else { "failed" }
        ),
    })
}

fn evaluate_success_condition(condition: &str, exit_code: i32, output: &str) -> Result<bool> {
    if condition == "exit_code_0" {
        return Ok(exit_code == 0);
    }

    if let Some(expected) = condition.strip_prefix("exit_code_0_and_output_contains:") {
        return Ok(exit_code == 0 && output.contains(expected.trim().trim_matches('"')));
    }

    if let Some(expected) = condition.strip_prefix("output_not_contains:") {
        return Ok(!output.contains(expected.trim().trim_matches('"')));
    }

    bail!("Unsupported success condition '{}'.", condition)
}

fn find_entry_node(spec: &crate::domain::workflow::WorkflowSpecDetails) -> Result<String> {
    let incoming = spec
        .edges
        .iter()
        .map(|edge| edge.to_node.as_str())
        .collect::<HashSet<_>>();
    let entry_nodes = spec
        .nodes
        .iter()
        .filter(|node| !incoming.contains(node.id.as_str()))
        .collect::<Vec<_>>();

    match entry_nodes.as_slice() {
        [entry] => Ok(entry.id.clone()),
        [] => bail!("Spec '{}' has no entry node.", spec.spec.name),
        _ => bail!("Spec '{}' has multiple entry nodes.", spec.spec.name),
    }
}

fn select_next_node<'a>(
    edges: &'a [WorkflowEdge],
    from_node: &str,
    status: WorkflowRunStatus,
) -> Result<Option<&'a str>> {
    let matching = edges
        .iter()
        .filter(|edge| edge.from_node == from_node)
        .filter(|edge| match status {
            WorkflowRunStatus::Pass => {
                edge.condition == crate::domain::workflow::WorkflowEdgeCondition::Pass
                    || edge.condition == crate::domain::workflow::WorkflowEdgeCondition::Always
            }
            WorkflowRunStatus::Fail => {
                edge.condition == crate::domain::workflow::WorkflowEdgeCondition::Fail
                    || edge.condition == crate::domain::workflow::WorkflowEdgeCondition::Always
            }
            WorkflowRunStatus::Running => false,
        })
        .collect::<Vec<_>>();

    match matching.as_slice() {
        [] => Ok(None),
        [edge] => Ok(Some(edge.to_node.as_str())),
        _ => bail!("Node '{}' has ambiguous outgoing edges.", from_node),
    }
}

fn should_advance_to_next_spec(node: &WorkflowNode, status: WorkflowRunStatus) -> bool {
    let route_key = match status {
        WorkflowRunStatus::Pass => "pass_route",
        WorkflowRunStatus::Fail => "fail_route",
        WorkflowRunStatus::Running => return false,
    };

    node.kind == WorkflowNodeKind::Gate
        && node
            .config
            .get(route_key)
            .and_then(Value::as_str)
            .is_some_and(|route| route == "next_spec")
}

fn resolve_spec_start(
    spec_details: &crate::domain::workflow::WorkflowSpecDetails,
    spec: &WorkflowSpec,
    existing_runs: &[WorkflowNodeRun],
) -> Result<(String, Option<Value>, HashMap<String, usize>)> {
    let mut iterations = HashMap::<String, usize>::new();
    for run in existing_runs {
        *iterations.entry(run.node_id.clone()).or_insert(0) += 1;
    }

    if spec.status == WorkflowSpecStatus::Running {
        if let Some(last_run) = existing_runs.last() {
            return Ok((last_run.node_id.clone(), last_run.input.clone(), iterations));
        }
    }

    Ok((find_entry_node(spec_details)?, None, iterations))
}

#[cfg(unix)]
fn shell_command(command: &str) -> Command {
    let mut process = Command::new("sh");
    process.arg("-lc").arg(command);
    process
}

#[cfg(windows)]
fn shell_command(command: &str) -> Command {
    let mut process = Command::new("cmd");
    process.arg("/C").arg(command);
    process
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::application::notification_service::DefaultNotificationService;
    use tempfile::{tempdir, TempDir};

    fn workflow_fixture() -> Result<(TempDir, Arc<Database>, WorkflowEngine, String, String)> {
        let dir = tempdir()?;
        let db = Arc::new(Database::new(&dir.path().join("test.db"))?);
        let workflow = crate::domain::workflow::Workflow {
            id: "wf-test".to_string(),
            name: "Workflow".to_string(),
            description: None,
            workdir: dir.path().to_string_lossy().to_string(),
            status: WorkflowStatus::Draft,
            created_at: chrono::Utc::now(),
            started_at: None,
            completed_at: None,
        };
        let spec = crate::domain::workflow::WorkflowSpec {
            id: "spec-test".to_string(),
            workflow_id: workflow.id.clone(),
            name: "Spec".to_string(),
            description: Some(
                "Functional Requirements:\n- A\n\nNon-Functional Requirements:\n- B\n\nObjective:\n- C\n\nConstraints:\n- D\n\nGuidelines:\n- E\n\nIn Scope:\n- F\n\nOut of Scope:\n- G".to_string(),
            ),
            position: 1,
            parallelizable: false,
            status: WorkflowSpecStatus::Pending,
            started_at: None,
            completed_at: None,
        };

        db.insert_workflow(&workflow)?;
        db.insert_workflow_spec(&spec)?;

        Ok((
            dir,
            Arc::clone(&db),
            WorkflowEngine::new(db, Arc::new(DefaultNotificationService)),
            workflow.id,
            spec.id,
        ))
    }

    #[tokio::test]
    async fn workflow_engine_completes_check_and_gate_spec() {
        let (_dir, db, engine, workflow_id, spec_id) = workflow_fixture().unwrap();
        let check = WorkflowNode {
            id: "node-check".to_string(),
            spec_id: spec_id.clone(),
            name: "check".to_string(),
            kind: WorkflowNodeKind::Check,
            config: serde_json::json!({
                "command": "printf APPROVED",
                "success_condition": "exit_code_0"
            }),
            position: 1,
            created_at: chrono::Utc::now(),
        };
        let gate = WorkflowNode {
            id: "node-gate".to_string(),
            spec_id: spec_id.clone(),
            name: "gate".to_string(),
            kind: WorkflowNodeKind::Gate,
            config: serde_json::json!({
                "evaluate": "output_contains",
                "value": "APPROVED",
                "pass_route": "next_spec"
            }),
            position: 2,
            created_at: chrono::Utc::now(),
        };

        db.insert_workflow_node(&check).unwrap();
        db.insert_workflow_node(&gate).unwrap();
        db.insert_workflow_edge(&WorkflowEdge {
            id: "edge-pass".to_string(),
            spec_id: spec_id.clone(),
            from_node: check.id.clone(),
            to_node: gate.id.clone(),
            condition: crate::domain::workflow::WorkflowEdgeCondition::Pass,
        })
        .unwrap();

        engine.run_workflow(workflow_id.clone()).await.unwrap();

        let workflow = db.get_workflow(&workflow_id).unwrap().unwrap();
        let spec = db.get_workflow_spec(&spec_id).unwrap().unwrap();
        let runs = db.list_workflow_runs_for_spec(&spec_id).unwrap();

        assert_eq!(workflow.status, WorkflowStatus::Completed);
        assert_eq!(spec.status, WorkflowSpecStatus::Completed);
        assert_eq!(runs.len(), 2);
    }

    #[tokio::test]
    async fn workflow_engine_fails_spec_when_check_fails_without_route() {
        let (_dir, db, engine, workflow_id, spec_id) = workflow_fixture().unwrap();
        db.insert_workflow_node(&WorkflowNode {
            id: "node-check".to_string(),
            spec_id: spec_id.clone(),
            name: "check".to_string(),
            kind: WorkflowNodeKind::Check,
            config: serde_json::json!({
                "command": "exit 1",
                "success_condition": "exit_code_0"
            }),
            position: 1,
            created_at: chrono::Utc::now(),
        })
        .unwrap();

        engine.run_workflow(workflow_id.clone()).await.unwrap();

        let workflow = db.get_workflow(&workflow_id).unwrap().unwrap();
        let spec = db.get_workflow_spec(&spec_id).unwrap().unwrap();

        assert_eq!(workflow.status, WorkflowStatus::Failed);
        assert_eq!(spec.status, WorkflowSpecStatus::Failed);
    }

    #[test]
    fn resolve_spec_start_retries_last_running_node() {
        let spec = WorkflowSpec {
            id: "spec".to_string(),
            workflow_id: "wf".to_string(),
            name: "Spec".to_string(),
            description: None,
            position: 1,
            parallelizable: false,
            status: WorkflowSpecStatus::Running,
            started_at: None,
            completed_at: None,
        };
        let details = crate::domain::workflow::WorkflowSpecDetails {
            spec: spec.clone(),
            nodes: vec![WorkflowNode {
                id: "node-1".to_string(),
                spec_id: spec.id.clone(),
                name: "Node".to_string(),
                kind: WorkflowNodeKind::Check,
                config: serde_json::json!({"command": "true"}),
                position: 1,
                created_at: chrono::Utc::now(),
            }],
            edges: vec![],
        };
        let runs = vec![WorkflowNodeRun {
            id: "run".to_string(),
            workflow_id: "wf".to_string(),
            spec_id: spec.id.clone(),
            node_id: "node-1".to_string(),
            status: WorkflowRunStatus::Fail,
            input: Some(serde_json::json!({"previous": "context"})),
            output: None,
            started_at: chrono::Utc::now(),
            completed_at: Some(chrono::Utc::now()),
            iteration: 1,
        }];

        let (node_id, previous_output, iterations) =
            resolve_spec_start(&details, &spec, &runs).unwrap();

        assert_eq!(node_id, "node-1");
        assert_eq!(iterations.get("node-1"), Some(&1));
        assert_eq!(
            previous_output.and_then(|value| value.get("previous").cloned()),
            Some(serde_json::json!("context"))
        );
    }
}
