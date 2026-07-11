use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use anyhow::{anyhow, bail, Context, Result};
use serde_json::Value;
use tokio::process::Command;

use crate::application::notification_service::NotificationService;
use crate::db::Database;
use crate::domain::loops::{
    LoopEdge, LoopNode, LoopNodeKind, LoopNodeRun, LoopRunStatus, LoopSpec, LoopSpecStatus,
    LoopStatus,
};
use crate::domain::models::Cli;

const DEFAULT_MAX_ITERATIONS_PER_NODE: usize = 10;

#[derive(Clone)]
pub struct LoopEngine {
    db: Arc<Database>,
    notification_service: Arc<dyn NotificationService>,
}

enum SpecExecutionOutcome {
    Completed,
    Paused,
    Failed(String),
}

struct NodeExecution {
    status: LoopRunStatus,
    output: Value,
    summary: String,
}

impl LoopEngine {
    pub fn new(db: Arc<Database>, notification_service: Arc<dyn NotificationService>) -> Self {
        Self {
            db,
            notification_service,
        }
    }

    pub fn start_background(self: Arc<Self>, loop_id: String) {
        Arc::clone(&self).start_background_run(loop_id, None, None);
    }

    /// Same as [`Self::start_background`], but optionally drives the loop's
    /// pending pool specs (see [`Self::run_loop`]) and/or overrides the
    /// workdir for this run only.
    pub fn start_background_run(
        self: Arc<Self>,
        loop_id: String,
        pool_id: Option<String>,
        workdir_override: Option<String>,
    ) {
        tokio::spawn(async move {
            if let Err(error) = self
                .run_loop(loop_id.clone(), pool_id, workdir_override)
                .await
            {
                tracing::error!("Loop '{}' failed to run: {error:#}", loop_id);
                let _ = self.fail_loop(&loop_id, &error.to_string());
            }
        });
    }

    pub fn request_pause(&self, loop_id: &str) -> Result<bool> {
        let Some(lp) = self.db.get_loop(loop_id)? else {
            return Ok(false);
        };

        match lp.status {
            LoopStatus::Running => {
                self.db
                    .update_loop_status(loop_id, LoopStatus::Paused, None, None)
            }
            LoopStatus::Paused => Ok(true),
            _ => Ok(false),
        }
    }

    /// Run `loop_id`'s specs through its graph (R2).
    ///
    /// With `pool_id`: runs the pool's pending members, in the pool's queue
    /// order, instead of the loop's own bound specs. Pool membership never
    /// mutates the specs themselves — they stay standalone (`loop_id: None`)
    /// so the same pool can be run by different loops over time.
    ///
    /// A pool run is *live* (R6): the "next pending" spec is re-queried from
    /// the pool at every spec boundary via
    /// [`Database::pool_next_pending_spec_id`], never off a list captured at
    /// launch. That's what lets `pool_add_spec`/`pool_reorder` calls made
    /// while the run is in flight actually change what runs next — the run
    /// ends only when a pick finds no pending member left. A bound run (no
    /// `pool_id`) keeps the pre-pool behavior below: its spec list is fixed
    /// at launch.
    ///
    /// `workdir_override`, when set, wins over `loop.workdir` for this run
    /// only — the loop's own `workdir` is left untouched.
    ///
    /// Without `pool_id`: identical to the pre-pool behavior (bound specs,
    /// `loop.workdir`).
    pub async fn run_loop(
        &self,
        loop_id: String,
        pool_id: Option<String>,
        workdir_override: Option<String>,
    ) -> Result<()> {
        let Some(lp) = self.db.get_loop(&loop_id)? else {
            bail!("Loop '{}' not found.", loop_id);
        };

        self.db.update_loop_status(
            &loop_id,
            LoopStatus::Running,
            Some(chrono::Utc::now()),
            None,
        )?;

        // The run's `workdir` param wins over `loop.workdir` — a pool run can
        // point the same loop's graph at a different checkout without
        // mutating the loop itself.
        let workdir = workdir_override.unwrap_or_else(|| lp.workdir.clone());

        match &pool_id {
            Some(pool_id) => loop {
                if self.is_paused(&loop_id)? {
                    return Ok(());
                }
                // Live pick: fresh query, not a frozen list. Only ever
                // returns a spec whose status is `pending` (defense in
                // depth — even if the pool's stored order were ever
                // corrupted to place a running/completed member where a
                // pending one belongs, this filter still won't pick it).
                let Some(spec_id) = self.db.pool_next_pending_spec_id(pool_id)? else {
                    break;
                };
                let Some(spec) = self.db.get_loop_spec(&spec_id)? else {
                    continue;
                };

                match self.run_spec(&lp, &spec, &workdir).await? {
                    SpecExecutionOutcome::Completed => continue,
                    SpecExecutionOutcome::Paused => return Ok(()),
                    SpecExecutionOutcome::Failed(summary) => {
                        self.fail_loop(&loop_id, &summary)?;
                        return Ok(());
                    }
                }
            },
            None => {
                for spec in self.db.list_loop_specs(&loop_id)? {
                    if self.is_paused(&loop_id)? {
                        return Ok(());
                    }
                    if matches!(
                        spec.status,
                        LoopSpecStatus::Completed | LoopSpecStatus::Skipped
                    ) {
                        continue;
                    }

                    match self.run_spec(&lp, &spec, &workdir).await? {
                        SpecExecutionOutcome::Completed => continue,
                        SpecExecutionOutcome::Paused => return Ok(()),
                        SpecExecutionOutcome::Failed(summary) => {
                            self.fail_loop(&loop_id, &summary)?;
                            return Ok(());
                        }
                    }
                }
            }
        }

        self.db.update_loop_status(
            &loop_id,
            LoopStatus::Completed,
            None,
            Some(chrono::Utc::now()),
        )?;
        self.notification_service
            .notify_task_completed(&loop_id, true, Some(0));
        Ok(())
    }

    async fn run_spec(
        &self,
        lp: &crate::domain::loops::Loop,
        spec: &LoopSpec,
        workdir: &str,
    ) -> Result<SpecExecutionOutcome> {
        let spec_details = self
            .db
            .get_loop_spec_details(&spec.id)?
            .ok_or_else(|| anyhow!("Loop spec '{}' not found.", spec.id))?;
        let graph_nodes = self.db.list_loop_nodes_for_loop(&lp.id)?;
        let graph_edges = self.db.list_loop_edges_for_loop(&lp.id)?;

        // A spec with its own graph always uses it (full backwards
        // compatibility). Only a spec with no nodes of its own falls back to
        // the loop-level graph, so the same graph can drive every spec in
        // the loop without repeating it per spec.
        let (nodes, edges): (&[LoopNode], &[LoopEdge]) = if !spec_details.nodes.is_empty() {
            (&spec_details.nodes, &spec_details.edges)
        } else if !graph_nodes.is_empty() {
            (&graph_nodes, &graph_edges)
        } else {
            let summary = format!(
                "Spec '{}' has no nodes of its own and loop '{}' has no loop-level graph to fall back to.",
                spec.name, lp.name
            );
            self.db.update_loop_spec_status(
                &spec.id,
                LoopSpecStatus::Failed,
                Some(chrono::Utc::now()),
                Some(chrono::Utc::now()),
            )?;
            return Ok(SpecExecutionOutcome::Failed(summary));
        };

        let nodes_by_id = nodes
            .iter()
            .map(|node| (node.id.as_str(), node))
            .collect::<HashMap<_, _>>();
        let existing_runs = self.db.list_loop_runs_for_spec(&spec.id)?;
        let (mut current_node_id, mut previous_output, mut iterations) =
            resolve_spec_start(nodes, edges, spec, &existing_runs)?;

        // Capture the workdir's git HEAD once, at the moment the spec starts
        // running — not on every node. A resumed spec (interrupted mid-run
        // by e.g. a daemon restart, then continued) reuses the value it
        // already persisted instead of re-capturing, so `{{spec_start_head}}`
        // always means "HEAD when this spec began", never "HEAD right now".
        let spec_start_head = if spec.status == LoopSpecStatus::Running {
            spec_details.spec.spec_start_head.clone()
        } else {
            let head = capture_workdir_head(workdir).await;
            self.db
                .set_loop_spec_start_head(&spec.id, head.as_deref())?;
            head
        };

        self.db.update_loop_spec_status(
            &spec.id,
            LoopSpecStatus::Running,
            Some(chrono::Utc::now()),
            None,
        )?;

        loop {
            if self.is_paused(&lp.id)? {
                return Ok(SpecExecutionOutcome::Paused);
            }

            let iteration = iterations.entry(current_node_id.clone()).or_insert(0);
            *iteration += 1;
            if *iteration > DEFAULT_MAX_ITERATIONS_PER_NODE {
                let summary = format!(
                    "Spec '{}' exceeded max iterations for node '{}'.",
                    spec.name, current_node_id
                );
                self.db.update_loop_spec_status(
                    &spec.id,
                    LoopSpecStatus::Failed,
                    None,
                    Some(chrono::Utc::now()),
                )?;
                return Ok(SpecExecutionOutcome::Failed(summary));
            }

            let node = nodes_by_id
                .get(current_node_id.as_str())
                .ok_or_else(|| anyhow!("Loop node '{}' not found.", current_node_id))?;
            let run_id = uuid::Uuid::new_v4().to_string();
            self.db.insert_loop_run(&LoopNodeRun {
                id: run_id.clone(),
                loop_id: lp.id.clone(),
                spec_id: spec.id.clone(),
                node_id: node.id.clone(),
                status: LoopRunStatus::Running,
                input: previous_output.clone(),
                output: None,
                started_at: chrono::Utc::now(),
                completed_at: None,
                iteration: *iteration as i64,
            })?;
            let execution = self
                .execute_node(
                    lp,
                    spec,
                    node,
                    previous_output.as_ref(),
                    spec_start_head.as_deref(),
                    &run_id,
                    workdir,
                )
                .await?;
            let run = self
                .db
                .get_loop_run(&run_id)?
                .ok_or_else(|| anyhow!("Loop run '{}' not found after execution.", run_id))?;
            let final_execution = if run.status == LoopRunStatus::Running {
                self.db.update_loop_run_result(
                    &run_id,
                    execution.status,
                    Some(&execution.output),
                    Some(chrono::Utc::now()),
                )?;
                execution
            } else {
                NodeExecution {
                    status: run.status,
                    output: run.output.unwrap_or_else(|| serde_json::json!({})),
                    summary: execution.summary,
                }
            };

            if self.is_paused(&lp.id)? {
                return Ok(SpecExecutionOutcome::Paused);
            }

            if should_advance_to_next_spec(node, final_execution.status) {
                self.db.update_loop_spec_status(
                    &spec.id,
                    LoopSpecStatus::Completed,
                    None,
                    Some(chrono::Utc::now()),
                )?;
                return Ok(SpecExecutionOutcome::Completed);
            }

            let next_node_id =
                select_next_node(edges, &node.id, final_execution.status)?.map(str::to_owned);

            match next_node_id {
                Some(next_node_id) => {
                    previous_output = Some(final_execution.output);
                    current_node_id = next_node_id;
                }
                None if final_execution.status == LoopRunStatus::Pass => {
                    self.db.update_loop_spec_status(
                        &spec.id,
                        LoopSpecStatus::Completed,
                        None,
                        Some(chrono::Utc::now()),
                    )?;
                    return Ok(SpecExecutionOutcome::Completed);
                }
                None => {
                    self.db.update_loop_spec_status(
                        &spec.id,
                        LoopSpecStatus::Failed,
                        None,
                        Some(chrono::Utc::now()),
                    )?;
                    return Ok(SpecExecutionOutcome::Failed(final_execution.summary));
                }
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    async fn execute_node(
        &self,
        lp: &crate::domain::loops::Loop,
        spec: &LoopSpec,
        node: &LoopNode,
        previous_output: Option<&Value>,
        spec_start_head: Option<&str>,
        run_id: &str,
        workdir: &str,
    ) -> Result<NodeExecution> {
        match node.kind {
            LoopNodeKind::Check => {
                execute_check_node(lp, spec, node, spec_start_head, workdir).await
            }
            LoopNodeKind::Gate => execute_gate_node(node, previous_output),
            LoopNodeKind::Agent => {
                execute_agent_node(&self.db, lp, spec, node, previous_output, run_id, workdir).await
            }
        }
    }

    fn is_paused(&self, loop_id: &str) -> Result<bool> {
        Ok(self
            .db
            .get_loop(loop_id)?
            .is_some_and(|lp| lp.status == LoopStatus::Paused))
    }

    fn fail_loop(&self, loop_id: &str, summary: &str) -> Result<()> {
        self.db
            .update_loop_status(loop_id, LoopStatus::Failed, None, Some(chrono::Utc::now()))?;
        self.notification_service
            .notify_task_failed(loop_id, 1, summary);
        Ok(())
    }
}

async fn execute_check_node(
    lp: &crate::domain::loops::Loop,
    spec: &LoopSpec,
    node: &LoopNode,
    spec_start_head: Option<&str>,
    workdir: &str,
) -> Result<NodeExecution> {
    let raw_command = node
        .config
        .get("command")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| anyhow!("Check node '{}' is missing a command.", node.name))?;
    let command = raw_command.replace("{{spec_start_head}}", spec_start_head.unwrap_or(""));
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

    let mut process = shell_command(&command);
    process.current_dir(workdir);
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
            LoopRunStatus::Pass
        } else {
            LoopRunStatus::Fail
        },
        output: serde_json::json!({
            "kind": "check",
            "loop_id": lp.id,
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

async fn execute_agent_node(
    db: &Arc<Database>,
    lp: &crate::domain::loops::Loop,
    spec: &LoopSpec,
    node: &LoopNode,
    previous_output: Option<&Value>,
    run_id: &str,
    workdir: &str,
) -> Result<NodeExecution> {
    let cli_name = node
        .config
        .get("platform")
        .or_else(|| node.config.get("cli"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| anyhow!("Agent node '{}' is missing a platform/cli.", node.name))?;
    let cli = Cli::resolve(Some(cli_name)).map_err(anyhow::Error::msg)?;
    let prompt_template = node
        .config
        .get("prompt_template")
        .and_then(Value::as_str)
        .unwrap_or("{{spec_content}}\n\n{{previous_feedback}}");
    let prompt = render_agent_prompt(lp, spec, node, prompt_template, previous_output, workdir);
    let model = node.config.get("model").and_then(Value::as_str);
    let timeout_minutes = node
        .config
        .get("timeout_minutes")
        .and_then(Value::as_u64)
        .unwrap_or(30);

    let mut command = cli
        .strategy()
        .build_command(&prompt, model, Some(workdir))
        .with_context(|| format!("Agent node '{}' failed to start.", node.name))?;
    let output = tokio::time::timeout(
        std::time::Duration::from_secs(timeout_minutes * 60),
        command.output(),
    )
    .await
    .with_context(|| format!("Agent node '{}' timed out.", node.name))??;

    if let Some(run) = db.get_loop_run(run_id)? {
        if run.status != LoopRunStatus::Running {
            return Ok(NodeExecution {
                status: run.status,
                output: run.output.unwrap_or_else(|| serde_json::json!({})),
                summary: format!("Agent node '{}' reported its own result.", node.name),
            });
        }
    }

    let exit_code = output.status.code().unwrap_or(-1);
    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    Ok(NodeExecution {
        status: if output.status.success() {
            LoopRunStatus::Pass
        } else {
            LoopRunStatus::Fail
        },
        output: serde_json::json!({
            "kind": "agent",
            "node_id": node.id,
            "cli": cli.as_str(),
            "model": model,
            "exit_code": exit_code,
            "stdout": stdout,
            "stderr": stderr,
        }),
        summary: format!("Agent node '{}' exited with code {}.", node.name, exit_code),
    })
}

fn execute_gate_node(node: &LoopNode, previous_output: Option<&Value>) -> Result<NodeExecution> {
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
            LoopRunStatus::Pass
        } else {
            LoopRunStatus::Fail
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

fn find_entry_node(nodes: &[LoopNode], edges: &[LoopEdge], spec_name: &str) -> Result<String> {
    let incoming = edges
        .iter()
        .map(|edge| edge.to_node.as_str())
        .collect::<HashSet<_>>();
    let entry_nodes = nodes
        .iter()
        .filter(|node| !incoming.contains(node.id.as_str()))
        .collect::<Vec<_>>();

    match entry_nodes.as_slice() {
        [entry] => Ok(entry.id.clone()),
        // Every node has an incoming edge: the graph is a retry cycle (e.g.
        // implement <-> review). There is no source node, so fall back to the
        // designated start — the node with the lowest position.
        [] => nodes
            .iter()
            .min_by_key(|node| node.position)
            .map(|node| node.id.clone())
            .ok_or_else(|| anyhow!("Spec '{}' has no nodes.", spec_name)),
        _ => bail!("Spec '{}' has multiple entry nodes.", spec_name),
    }
}

fn select_next_node<'a>(
    edges: &'a [LoopEdge],
    from_node: &str,
    status: LoopRunStatus,
) -> Result<Option<&'a str>> {
    let matching = edges
        .iter()
        .filter(|edge| edge.from_node == from_node)
        .filter(|edge| match status {
            LoopRunStatus::Pass => {
                edge.condition == crate::domain::loops::LoopEdgeCondition::Pass
                    || edge.condition == crate::domain::loops::LoopEdgeCondition::Always
            }
            LoopRunStatus::Fail => {
                edge.condition == crate::domain::loops::LoopEdgeCondition::Fail
                    || edge.condition == crate::domain::loops::LoopEdgeCondition::Always
            }
            LoopRunStatus::Running => false,
        })
        .collect::<Vec<_>>();

    match matching.as_slice() {
        [] => Ok(None),
        [edge] => Ok(Some(edge.to_node.as_str())),
        _ => {
            let distinct_targets = matching
                .iter()
                .map(|edge| edge.to_node.as_str())
                .collect::<HashSet<_>>();
            match distinct_targets.into_iter().collect::<Vec<_>>().as_slice() {
                [to_node] => Ok(Some(*to_node)),
                _ => bail!("Node '{}' has ambiguous outgoing edges.", from_node),
            }
        }
    }
}

fn should_advance_to_next_spec(node: &LoopNode, status: LoopRunStatus) -> bool {
    let route_key = match status {
        LoopRunStatus::Pass => "pass_route",
        LoopRunStatus::Fail => "fail_route",
        LoopRunStatus::Running => return false,
    };

    node.kind == LoopNodeKind::Gate
        && node
            .config
            .get(route_key)
            .and_then(Value::as_str)
            .is_some_and(|route| route == "next_spec")
}

fn render_agent_prompt(
    lp: &crate::domain::loops::Loop,
    spec: &LoopSpec,
    node: &LoopNode,
    prompt_template: &str,
    previous_output: Option<&Value>,
    workdir: &str,
) -> String {
    let previous_feedback = previous_output
        .map(|value| serde_json::to_string_pretty(value).unwrap_or_default())
        .unwrap_or_else(|| "(none)".to_string());
    let spec_content = spec.description.as_deref().unwrap_or(&spec.name);
    let prompt = prompt_template
        .replace("{{loop_name}}", &lp.name)
        .replace("{{workdir}}", workdir)
        .replace("{{spec_id}}", &spec.id)
        .replace("{{spec_name}}", &spec.name)
        .replace("{{spec_content}}", spec_content)
        .replace("{{node_id}}", &node.id)
        .replace("{{previous_feedback}}", &previous_feedback);

    format!(
        "# [LOOP CONTEXT]\n<loop>\n  <name>{}</name>\n  <spec>{}</spec>\n  <node>{}</node>\n  <workdir>{}</workdir>\n</loop>\n\n# [SPEC]\n{}\n\n# [PREVIOUS FEEDBACK]\n{}\n\n# [REPORTING]\nWhen you finish this node, call loop_complete_node with node_id=\"{}\", status=\"pass\"|\"fail\", a concise summary, and your output.\nIf you are blocked and need human intervention, call loop_report_blocker with node_id=\"{}\" and the blocker description.\n",
        lp.name,
        spec.name,
        node.name,
        workdir,
        prompt,
        previous_feedback,
        node.id,
        node.id
    )
}

/// The workdir's current `git rev-parse HEAD`, or `None` if it isn't a git
/// repo (or the command otherwise fails). Never errors the caller — a check
/// node that references `{{spec_start_head}}` in a non-git workdir just sees
/// an empty string and decides for itself, per `execute_check_node`.
async fn capture_workdir_head(workdir: &str) -> Option<String> {
    let output = Command::new("git")
        .arg("rev-parse")
        .arg("HEAD")
        .current_dir(workdir)
        .output()
        .await
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let head = String::from_utf8_lossy(&output.stdout).trim().to_string();
    (!head.is_empty()).then_some(head)
}

fn resolve_spec_start(
    nodes: &[LoopNode],
    edges: &[LoopEdge],
    spec: &LoopSpec,
    existing_runs: &[LoopNodeRun],
) -> Result<(String, Option<Value>, HashMap<String, usize>)> {
    if spec.status == LoopSpecStatus::Running {
        if let Some(last_run) = existing_runs.last() {
            let mut iterations = HashMap::<String, usize>::new();
            for run in existing_runs {
                *iterations.entry(run.node_id.clone()).or_insert(0) += 1;
            }
            return Ok((last_run.node_id.clone(), last_run.input.clone(), iterations));
        }
    }

    Ok((
        find_entry_node(nodes, edges, &spec.name)?,
        None,
        HashMap::new(),
    ))
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

    fn loop_fixture() -> Result<(TempDir, Arc<Database>, LoopEngine, String, String)> {
        let dir = tempdir()?;
        let db = Arc::new(Database::new(&dir.path().join("test.db"))?);
        let lp = crate::domain::loops::Loop {
            id: "wf-test".to_string(),
            name: "Loop".to_string(),
            description: None,
            workdir: dir.path().to_string_lossy().to_string(),
            status: LoopStatus::Draft,
            trigger: None,
            created_at: chrono::Utc::now(),
            started_at: None,
            completed_at: None,
            autorun_at: None,
        };
        let spec = crate::domain::loops::LoopSpec {
            id: "spec-test".to_string(),
            loop_id: Some(lp.id.clone()),
            name: "Spec".to_string(),
            description: Some(
                "Functional Requirements:\n- A\n\nNon-Functional Requirements:\n- B\n\nObjective:\n- C\n\nConstraints:\n- D\n\nGuidelines:\n- E\n\nIn Scope:\n- F\n\nOut of Scope:\n- G".to_string(),
            ),
            position: 1,
            parallelizable: false,
            status: LoopSpecStatus::Pending,
            started_at: None,
            completed_at: None,
            spec_start_head: None,
            workdir: None,
        };

        db.insert_loop(&lp)?;
        db.insert_loop_spec(&spec)?;

        Ok((
            dir,
            Arc::clone(&db),
            LoopEngine::new(db, Arc::new(DefaultNotificationService)),
            lp.id,
            spec.id,
        ))
    }

    fn init_git_repo(path: &std::path::Path) {
        let run = |args: &[&str]| {
            let status = std::process::Command::new("git")
                .args(args)
                .current_dir(path)
                .env("GIT_AUTHOR_NAME", "Test")
                .env("GIT_AUTHOR_EMAIL", "test@example.com")
                .env("GIT_COMMITTER_NAME", "Test")
                .env("GIT_COMMITTER_EMAIL", "test@example.com")
                .status()
                .expect("git command failed to run");
            assert!(status.success(), "git {:?} failed", args);
        };
        run(&["init", "-q"]);
        std::fs::write(path.join("README.md"), "test").unwrap();
        run(&["add", "."]);
        run(&["commit", "-q", "-m", "init"]);
    }

    fn git_head(path: &std::path::Path) -> String {
        let output = std::process::Command::new("git")
            .args(["rev-parse", "HEAD"])
            .current_dir(path)
            .output()
            .expect("git rev-parse failed to run");
        assert!(output.status.success(), "git rev-parse HEAD failed");
        String::from_utf8_lossy(&output.stdout).trim().to_string()
    }

    #[tokio::test]
    async fn loop_engine_captures_spec_start_head_for_git_workdir() {
        let (dir, db, engine, loop_id, spec_id) = loop_fixture().unwrap();
        init_git_repo(dir.path());
        let expected_head = git_head(dir.path());

        db.insert_loop_node(&LoopNode {
            id: "node-check".to_string(),
            spec_id: Some(spec_id.clone()),
            loop_id: None,
            name: "check".to_string(),
            kind: LoopNodeKind::Check,
            config: serde_json::json!({
                "command": "printf APPROVED",
                "success_condition": "exit_code_0"
            }),
            position: 1,
            created_at: chrono::Utc::now(),
        })
        .unwrap();

        engine.run_loop(loop_id, None, None).await.unwrap();

        let spec = db.get_loop_spec(&spec_id).unwrap().unwrap();
        assert_eq!(
            spec.spec_start_head.as_deref(),
            Some(expected_head.as_str())
        );
    }

    #[tokio::test]
    async fn loop_engine_substitutes_spec_start_head_in_check_command() {
        let (dir, db, engine, loop_id, spec_id) = loop_fixture().unwrap();
        init_git_repo(dir.path());

        db.insert_loop_node(&LoopNode {
            id: "node-check".to_string(),
            spec_id: Some(spec_id.clone()),
            loop_id: None,
            name: "check".to_string(),
            kind: LoopNodeKind::Check,
            config: serde_json::json!({
                "command": "test \"$(git rev-parse HEAD)\" = \"{{spec_start_head}}\"",
                "success_condition": "exit_code_0"
            }),
            position: 1,
            created_at: chrono::Utc::now(),
        })
        .unwrap();

        engine.run_loop(loop_id.clone(), None, None).await.unwrap();

        let lp = db.get_loop(&loop_id).unwrap().unwrap();
        let spec = db.get_loop_spec(&spec_id).unwrap().unwrap();
        assert_eq!(lp.status, LoopStatus::Completed);
        assert_eq!(spec.status, LoopSpecStatus::Completed);
    }

    #[tokio::test]
    async fn loop_engine_substitutes_empty_spec_start_head_for_non_git_workdir() {
        let (_dir, db, engine, loop_id, spec_id) = loop_fixture().unwrap();

        db.insert_loop_node(&LoopNode {
            id: "node-check".to_string(),
            spec_id: Some(spec_id.clone()),
            loop_id: None,
            name: "check".to_string(),
            kind: LoopNodeKind::Check,
            config: serde_json::json!({
                "command": "test -z \"{{spec_start_head}}\"",
                "success_condition": "exit_code_0"
            }),
            position: 1,
            created_at: chrono::Utc::now(),
        })
        .unwrap();

        engine.run_loop(loop_id.clone(), None, None).await.unwrap();

        let lp = db.get_loop(&loop_id).unwrap().unwrap();
        let spec = db.get_loop_spec(&spec_id).unwrap().unwrap();
        assert_eq!(lp.status, LoopStatus::Completed);
        assert_eq!(spec.status, LoopSpecStatus::Completed);
        assert_eq!(spec.spec_start_head, None);
    }

    #[tokio::test]
    async fn loop_engine_completes_check_and_gate_spec() {
        let (_dir, db, engine, loop_id, spec_id) = loop_fixture().unwrap();
        let check = LoopNode {
            id: "node-check".to_string(),
            spec_id: Some(spec_id.clone()),
            loop_id: None,
            name: "check".to_string(),
            kind: LoopNodeKind::Check,
            config: serde_json::json!({
                "command": "printf APPROVED",
                "success_condition": "exit_code_0"
            }),
            position: 1,
            created_at: chrono::Utc::now(),
        };
        let gate = LoopNode {
            id: "node-gate".to_string(),
            spec_id: Some(spec_id.clone()),
            loop_id: None,
            name: "gate".to_string(),
            kind: LoopNodeKind::Gate,
            config: serde_json::json!({
                "evaluate": "output_contains",
                "value": "APPROVED",
                "pass_route": "next_spec"
            }),
            position: 2,
            created_at: chrono::Utc::now(),
        };

        db.insert_loop_node(&check).unwrap();
        db.insert_loop_node(&gate).unwrap();
        db.insert_loop_edge(&LoopEdge {
            id: "edge-pass".to_string(),
            spec_id: Some(spec_id.clone()),
            loop_id: None,
            from_node: check.id.clone(),
            to_node: gate.id.clone(),
            condition: crate::domain::loops::LoopEdgeCondition::Pass,
        })
        .unwrap();

        engine.run_loop(loop_id.clone(), None, None).await.unwrap();

        let lp = db.get_loop(&loop_id).unwrap().unwrap();
        let spec = db.get_loop_spec(&spec_id).unwrap().unwrap();
        let runs = db.list_loop_runs_for_spec(&spec_id).unwrap();

        assert_eq!(lp.status, LoopStatus::Completed);
        assert_eq!(spec.status, LoopSpecStatus::Completed);
        assert_eq!(runs.len(), 2);
    }

    #[tokio::test]
    async fn loop_engine_fails_spec_when_check_fails_without_route() {
        let (_dir, db, engine, loop_id, spec_id) = loop_fixture().unwrap();
        db.insert_loop_node(&LoopNode {
            id: "node-check".to_string(),
            spec_id: Some(spec_id.clone()),
            loop_id: None,
            name: "check".to_string(),
            kind: LoopNodeKind::Check,
            config: serde_json::json!({
                "command": "exit 1",
                "success_condition": "exit_code_0"
            }),
            position: 1,
            created_at: chrono::Utc::now(),
        })
        .unwrap();

        engine.run_loop(loop_id.clone(), None, None).await.unwrap();

        let lp = db.get_loop(&loop_id).unwrap().unwrap();
        let spec = db.get_loop_spec(&spec_id).unwrap().unwrap();

        assert_eq!(lp.status, LoopStatus::Failed);
        assert_eq!(spec.status, LoopSpecStatus::Failed);
    }

    #[test]
    fn resolve_spec_start_retries_last_running_node() {
        let spec = LoopSpec {
            id: "spec".to_string(),
            loop_id: Some("wf".to_string()),
            name: "Spec".to_string(),
            description: None,
            position: 1,
            parallelizable: false,
            status: LoopSpecStatus::Running,
            started_at: None,
            completed_at: None,
            spec_start_head: None,
            workdir: None,
        };
        let details = crate::domain::loops::LoopSpecDetails {
            spec: spec.clone(),
            nodes: vec![LoopNode {
                id: "node-1".to_string(),
                spec_id: Some(spec.id.clone()),
                loop_id: None,
                name: "Node".to_string(),
                kind: LoopNodeKind::Check,
                config: serde_json::json!({"command": "true"}),
                position: 1,
                created_at: chrono::Utc::now(),
            }],
            edges: vec![],
        };
        let runs = vec![LoopNodeRun {
            id: "run".to_string(),
            loop_id: "wf".to_string(),
            spec_id: spec.id.clone(),
            node_id: "node-1".to_string(),
            status: LoopRunStatus::Fail,
            input: Some(serde_json::json!({"previous": "context"})),
            output: None,
            started_at: chrono::Utc::now(),
            completed_at: Some(chrono::Utc::now()),
            iteration: 1,
        }];

        let (node_id, previous_output, iterations) =
            resolve_spec_start(&details.nodes, &details.edges, &spec, &runs).unwrap();

        assert_eq!(node_id, "node-1");
        assert_eq!(iterations.get("node-1"), Some(&1));
        assert_eq!(
            previous_output.and_then(|value| value.get("previous").cloned()),
            Some(serde_json::json!("context"))
        );
    }

    #[test]
    fn resolve_spec_start_resets_iterations_for_fresh_spec() {
        let spec = LoopSpec {
            id: "spec".to_string(),
            loop_id: Some("wf".to_string()),
            name: "Spec".to_string(),
            description: None,
            position: 1,
            parallelizable: false,
            status: LoopSpecStatus::Pending,
            started_at: None,
            completed_at: None,
            spec_start_head: None,
            workdir: None,
        };
        let details = crate::domain::loops::LoopSpecDetails {
            spec: spec.clone(),
            nodes: vec![LoopNode {
                id: "node-1".to_string(),
                spec_id: Some(spec.id.clone()),
                loop_id: None,
                name: "Node".to_string(),
                kind: LoopNodeKind::Check,
                config: serde_json::json!({"command": "true"}),
                position: 1,
                created_at: chrono::Utc::now(),
            }],
            edges: vec![],
        };
        // Historical runs from a previous attempt at this spec: 10 failed
        // iterations that exhausted the budget last time around.
        let runs: Vec<LoopNodeRun> = (0..10)
            .map(|i| LoopNodeRun {
                id: format!("run-{i}"),
                loop_id: "wf".to_string(),
                spec_id: spec.id.clone(),
                node_id: "node-1".to_string(),
                status: LoopRunStatus::Fail,
                input: None,
                output: None,
                started_at: chrono::Utc::now(),
                completed_at: Some(chrono::Utc::now()),
                iteration: i + 1,
            })
            .collect();

        let (node_id, previous_output, iterations) =
            resolve_spec_start(&details.nodes, &details.edges, &spec, &runs).unwrap();

        assert_eq!(node_id, "node-1");
        assert!(previous_output.is_none());
        assert!(iterations.is_empty());
    }

    #[test]
    fn find_entry_node_picks_lowest_position_in_retry_cycle() {
        // implement (pos 1) <-> review (pos 2): every node has an incoming
        // edge, so there is no source node. The entry must be the designated
        // start (lowest position), not an error.
        let spec = LoopSpec {
            id: "spec".to_string(),
            loop_id: Some("wf".to_string()),
            name: "Spec".to_string(),
            description: None,
            position: 1,
            parallelizable: false,
            status: LoopSpecStatus::Pending,
            started_at: None,
            completed_at: None,
            spec_start_head: None,
            workdir: None,
        };
        let node = |id: &str, position: i64| LoopNode {
            id: id.to_string(),
            spec_id: Some(spec.id.clone()),
            loop_id: None,
            name: id.to_string(),
            kind: LoopNodeKind::Agent,
            config: serde_json::json!({}),
            position,
            created_at: chrono::Utc::now(),
        };
        let edge = |id: &str, from: &str, to: &str, condition| LoopEdge {
            id: id.to_string(),
            spec_id: Some(spec.id.clone()),
            loop_id: None,
            from_node: from.to_string(),
            to_node: to.to_string(),
            condition,
        };
        let details = crate::domain::loops::LoopSpecDetails {
            spec: spec.clone(),
            // Insert review before implement so the result cannot depend on
            // node ordering — only on position.
            nodes: vec![node("review", 2), node("implement", 1)],
            edges: vec![
                edge(
                    "e1",
                    "implement",
                    "review",
                    crate::domain::loops::LoopEdgeCondition::Always,
                ),
                edge(
                    "e2",
                    "review",
                    "implement",
                    crate::domain::loops::LoopEdgeCondition::Fail,
                ),
            ],
        };

        assert_eq!(
            find_entry_node(&details.nodes, &details.edges, &details.spec.name).unwrap(),
            "implement"
        );
    }

    #[test]
    fn render_agent_prompt_includes_reporting_contract() {
        let lp = crate::domain::loops::Loop {
            id: "wf".to_string(),
            name: "Loop".to_string(),
            description: None,
            workdir: "/tmp/project".to_string(),
            status: LoopStatus::Draft,
            trigger: None,
            created_at: chrono::Utc::now(),
            started_at: None,
            completed_at: None,
            autorun_at: None,
        };
        let spec = LoopSpec {
            id: "spec".to_string(),
            loop_id: Some("wf".to_string()),
            name: "Spec".to_string(),
            description: Some("Do the thing".to_string()),
            position: 1,
            parallelizable: false,
            status: LoopSpecStatus::Pending,
            started_at: None,
            completed_at: None,
            spec_start_head: None,
            workdir: None,
        };
        let node = LoopNode {
            id: "node-1".to_string(),
            spec_id: Some("spec".to_string()),
            loop_id: None,
            name: "Agent".to_string(),
            kind: LoopNodeKind::Agent,
            config: serde_json::json!({}),
            position: 1,
            created_at: chrono::Utc::now(),
        };

        let prompt = render_agent_prompt(
            &lp,
            &spec,
            &node,
            "{{spec_content}}",
            Some(&serde_json::json!({"feedback":"ok"})),
            &lp.workdir,
        );

        assert!(prompt.contains("loop_complete_node"));
        assert!(prompt.contains("loop_report_blocker"));
        assert!(prompt.contains("Do the thing"));
        assert!(prompt.contains("\"feedback\": \"ok\""));
    }

    #[test]
    fn select_next_node_dedupes_identical_edges_to_same_target() {
        let edge = |id: &str, to: &str, condition| LoopEdge {
            id: id.to_string(),
            spec_id: Some("spec".to_string()),
            loop_id: None,
            from_node: "implement".to_string(),
            to_node: to.to_string(),
            condition,
        };
        let edges = vec![
            edge(
                "e1",
                "review",
                crate::domain::loops::LoopEdgeCondition::Always,
            ),
            edge(
                "e2",
                "review",
                crate::domain::loops::LoopEdgeCondition::Always,
            ),
        ];

        let next = select_next_node(&edges, "implement", LoopRunStatus::Pass).unwrap();

        assert_eq!(next, Some("review"));
    }

    #[test]
    fn select_next_node_errors_on_distinct_targets() {
        let edge = |id: &str, to: &str, condition| LoopEdge {
            id: id.to_string(),
            spec_id: Some("spec".to_string()),
            loop_id: None,
            from_node: "implement".to_string(),
            to_node: to.to_string(),
            condition,
        };
        let edges = vec![
            edge(
                "e1",
                "review",
                crate::domain::loops::LoopEdgeCondition::Always,
            ),
            edge(
                "e2",
                "deploy",
                crate::domain::loops::LoopEdgeCondition::Always,
            ),
        ];

        let err = select_next_node(&edges, "implement", LoopRunStatus::Pass).unwrap_err();

        assert!(err.to_string().contains("ambiguous outgoing edges"));
    }

    fn second_spec(loop_id: &str, id: &str, position: i64) -> LoopSpec {
        LoopSpec {
            id: id.to_string(),
            loop_id: Some(loop_id.to_string()),
            name: format!("Spec {id}"),
            description: Some(
                "Functional Requirements:\n- A\n\nNon-Functional Requirements:\n- B\n\nObjective:\n- C\n\nConstraints:\n- D\n\nGuidelines:\n- E\n\nIn Scope:\n- F\n\nOut of Scope:\n- G".to_string(),
            ),
            position,
            parallelizable: false,
            status: LoopSpecStatus::Pending,
            started_at: None,
            completed_at: None,
            spec_start_head: None,
            workdir: None,
        }
    }

    #[tokio::test]
    async fn loop_engine_runs_loop_level_graph_across_two_specs() {
        // Neither spec has nodes of its own; both walk the loop's shared
        // top-level graph. A loop defined once should drive every spec.
        let (_dir, db, engine, loop_id, spec1_id) = loop_fixture().unwrap();
        let spec2 = second_spec(&loop_id, "spec-2", 2);
        db.insert_loop_spec(&spec2).unwrap();

        let check = LoopNode {
            id: "loop-check".to_string(),
            spec_id: None,
            loop_id: Some(loop_id.clone()),
            name: "check".to_string(),
            kind: LoopNodeKind::Check,
            config: serde_json::json!({
                "command": "printf APPROVED",
                "success_condition": "exit_code_0"
            }),
            position: 1,
            created_at: chrono::Utc::now(),
        };
        let gate = LoopNode {
            id: "loop-gate".to_string(),
            spec_id: None,
            loop_id: Some(loop_id.clone()),
            name: "gate".to_string(),
            kind: LoopNodeKind::Gate,
            config: serde_json::json!({
                "evaluate": "output_contains",
                "value": "APPROVED",
                "pass_route": "next_spec"
            }),
            position: 2,
            created_at: chrono::Utc::now(),
        };
        db.insert_loop_node(&check).unwrap();
        db.insert_loop_node(&gate).unwrap();
        db.insert_loop_edge(&LoopEdge {
            id: "loop-edge-pass".to_string(),
            spec_id: None,
            loop_id: Some(loop_id.clone()),
            from_node: check.id.clone(),
            to_node: gate.id.clone(),
            condition: crate::domain::loops::LoopEdgeCondition::Pass,
        })
        .unwrap();

        engine.run_loop(loop_id.clone(), None, None).await.unwrap();

        let lp = db.get_loop(&loop_id).unwrap().unwrap();
        assert_eq!(lp.status, LoopStatus::Completed);

        for spec_id in [spec1_id.as_str(), spec2.id.as_str()] {
            let spec = db.get_loop_spec(spec_id).unwrap().unwrap();
            assert_eq!(spec.status, LoopSpecStatus::Completed);
            let runs = db.list_loop_runs_for_spec(spec_id).unwrap();
            assert_eq!(runs.len(), 2);
            assert!(runs.iter().all(|run| run.spec_id == spec_id));
            assert!(runs.iter().any(|run| run.node_id == "loop-check"));
            assert!(runs.iter().any(|run| run.node_id == "loop-gate"));
        }
    }

    #[tokio::test]
    async fn loop_engine_spec_with_own_graph_ignores_loop_level_graph() {
        // The loop-level graph always fails; if it were used, the spec would
        // fail. The spec's own graph always passes, and precedence must
        // favor it — full backwards compatibility.
        let (_dir, db, engine, loop_id, spec_id) = loop_fixture().unwrap();

        db.insert_loop_node(&LoopNode {
            id: "loop-check".to_string(),
            spec_id: None,
            loop_id: Some(loop_id.clone()),
            name: "loop-check".to_string(),
            kind: LoopNodeKind::Check,
            config: serde_json::json!({
                "command": "exit 1",
                "success_condition": "exit_code_0"
            }),
            position: 1,
            created_at: chrono::Utc::now(),
        })
        .unwrap();

        db.insert_loop_node(&LoopNode {
            id: "spec-check".to_string(),
            spec_id: Some(spec_id.clone()),
            loop_id: None,
            name: "spec-check".to_string(),
            kind: LoopNodeKind::Check,
            config: serde_json::json!({
                "command": "printf APPROVED",
                "success_condition": "exit_code_0"
            }),
            position: 1,
            created_at: chrono::Utc::now(),
        })
        .unwrap();

        engine.run_loop(loop_id.clone(), None, None).await.unwrap();

        let lp = db.get_loop(&loop_id).unwrap().unwrap();
        let spec = db.get_loop_spec(&spec_id).unwrap().unwrap();
        let runs = db.list_loop_runs_for_spec(&spec_id).unwrap();

        assert_eq!(lp.status, LoopStatus::Completed);
        assert_eq!(spec.status, LoopSpecStatus::Completed);
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].node_id, "spec-check");
    }

    #[tokio::test]
    async fn loop_engine_iteration_budget_resets_between_specs_on_loop_graph() {
        // A single loop-level node that self-loops on failure, gated by a
        // counter file shared across the whole run. It fails 9 times then
        // passes on the 10th call — exactly the per-node iteration cap. If
        // spec 2's budget carried over from spec 1 instead of resetting, its
        // first attempt would already read as iteration 11 and the spec
        // would fail before the check command ever runs again.
        let (_dir, db, engine, loop_id, spec1_id) = loop_fixture().unwrap();
        let spec2 = second_spec(&loop_id, "spec-2", 2);
        db.insert_loop_spec(&spec2).unwrap();

        db.insert_loop_node(&LoopNode {
            id: "flaky".to_string(),
            spec_id: None,
            loop_id: Some(loop_id.clone()),
            name: "flaky".to_string(),
            kind: LoopNodeKind::Check,
            config: serde_json::json!({
                "command": "n=$(cat counter.txt 2>/dev/null || echo 0); n=$((n+1)); echo $n > counter.txt; test $n -ge 10",
                "success_condition": "exit_code_0"
            }),
            position: 1,
            created_at: chrono::Utc::now(),
        })
        .unwrap();
        db.insert_loop_edge(&LoopEdge {
            id: "self-loop".to_string(),
            spec_id: None,
            loop_id: Some(loop_id.clone()),
            from_node: "flaky".to_string(),
            to_node: "flaky".to_string(),
            condition: crate::domain::loops::LoopEdgeCondition::Fail,
        })
        .unwrap();

        engine.run_loop(loop_id.clone(), None, None).await.unwrap();

        let lp = db.get_loop(&loop_id).unwrap().unwrap();
        assert_eq!(lp.status, LoopStatus::Completed);

        let spec1 = db.get_loop_spec(&spec1_id).unwrap().unwrap();
        let spec1_runs = db.list_loop_runs_for_spec(&spec1_id).unwrap();
        assert_eq!(spec1.status, LoopSpecStatus::Completed);
        assert_eq!(spec1_runs.len(), 10);

        let spec2_saved = db.get_loop_spec(&spec2.id).unwrap().unwrap();
        let spec2_runs = db.list_loop_runs_for_spec(&spec2.id).unwrap();
        assert_eq!(spec2_saved.status, LoopSpecStatus::Completed);
        // Fresh budget: the counter file is already at 10 from spec 1, so
        // spec 2's first (and only) fresh-budget attempt passes immediately.
        assert_eq!(spec2_runs.len(), 1);
    }

    #[tokio::test]
    async fn loop_engine_loop_level_entry_fallback_by_lowest_position() {
        // Retry cycle (implement <-> review): every node has an incoming
        // edge, so there is no source node and the engine must fall back to
        // the lowest-position node as the entry point.
        let (_dir, db, engine, loop_id, spec_id) = loop_fixture().unwrap();

        let implement = LoopNode {
            id: "implement".to_string(),
            spec_id: None,
            loop_id: Some(loop_id.clone()),
            name: "implement".to_string(),
            kind: LoopNodeKind::Check,
            config: serde_json::json!({
                "command": "printf IMPLEMENT",
                "success_condition": "exit_code_0"
            }),
            position: 1,
            created_at: chrono::Utc::now(),
        };
        let review = LoopNode {
            id: "review".to_string(),
            spec_id: None,
            loop_id: Some(loop_id.clone()),
            name: "review".to_string(),
            kind: LoopNodeKind::Check,
            config: serde_json::json!({
                "command": "printf REVIEW",
                "success_condition": "exit_code_0"
            }),
            position: 2,
            created_at: chrono::Utc::now(),
        };
        db.insert_loop_node(&implement).unwrap();
        db.insert_loop_node(&review).unwrap();
        db.insert_loop_edge(&LoopEdge {
            id: "e1".to_string(),
            spec_id: None,
            loop_id: Some(loop_id.clone()),
            from_node: "implement".to_string(),
            to_node: "review".to_string(),
            condition: crate::domain::loops::LoopEdgeCondition::Always,
        })
        .unwrap();
        db.insert_loop_edge(&LoopEdge {
            id: "e2".to_string(),
            spec_id: None,
            loop_id: Some(loop_id.clone()),
            from_node: "review".to_string(),
            to_node: "implement".to_string(),
            condition: crate::domain::loops::LoopEdgeCondition::Fail,
        })
        .unwrap();

        engine.run_loop(loop_id.clone(), None, None).await.unwrap();

        let lp = db.get_loop(&loop_id).unwrap().unwrap();
        let spec = db.get_loop_spec(&spec_id).unwrap().unwrap();
        let runs = db.list_loop_runs_for_spec(&spec_id).unwrap();

        assert_eq!(lp.status, LoopStatus::Completed);
        assert_eq!(spec.status, LoopSpecStatus::Completed);
        assert_eq!(runs.len(), 2);
        // "implement" must be the entry: it ran with no previous-node input.
        // "review" ran second, fed by implement's output — proving the walk
        // started at the lowest-position node, not an arbitrary one.
        let implement_run = runs.iter().find(|run| run.node_id == "implement").unwrap();
        let review_run = runs.iter().find(|run| run.node_id == "review").unwrap();
        assert!(implement_run.input.is_none());
        assert!(review_run.input.is_some());
    }

    #[tokio::test]
    async fn loop_engine_fails_spec_with_no_graph_anywhere_and_loop_moves_on() {
        // Neither the spec nor the loop has a graph: the spec must fail with
        // an actionable error instead of the engine erroring out before the
        // spec is even marked failed.
        let (_dir, db, engine, loop_id, spec_id) = loop_fixture().unwrap();

        engine.run_loop(loop_id.clone(), None, None).await.unwrap();

        let lp = db.get_loop(&loop_id).unwrap().unwrap();
        let spec = db.get_loop_spec(&spec_id).unwrap().unwrap();

        assert_eq!(lp.status, LoopStatus::Failed);
        assert_eq!(spec.status, LoopSpecStatus::Failed);
    }

    // ── R5: `loop_run` with a pool ──────────────────────────────────────

    /// A loop with no bound specs — the pool's own standalone specs supply
    /// the work instead. Distinct from [`loop_fixture`], which always seeds
    /// one bound spec.
    fn bare_loop_fixture() -> Result<(TempDir, Arc<Database>, LoopEngine, String)> {
        let dir = tempdir()?;
        let db = Arc::new(Database::new(&dir.path().join("test.db"))?);
        let lp = crate::domain::loops::Loop {
            id: "wf-test".to_string(),
            name: "Loop".to_string(),
            description: None,
            workdir: dir.path().to_string_lossy().to_string(),
            status: LoopStatus::Draft,
            trigger: None,
            created_at: chrono::Utc::now(),
            started_at: None,
            completed_at: None,
            autorun_at: None,
        };
        db.insert_loop(&lp)?;
        Ok((
            dir,
            Arc::clone(&db),
            LoopEngine::new(db, Arc::new(DefaultNotificationService)),
            lp.id,
        ))
    }

    /// A standalone spec (`loop_id: None`), the shape pool members take —
    /// pool membership never binds the spec to a loop.
    fn standalone_spec(id: &str, position: i64) -> LoopSpec {
        LoopSpec {
            id: id.to_string(),
            loop_id: None,
            name: id.to_string(),
            description: Some(
                "Functional Requirements:\n- A\n\nNon-Functional Requirements:\n- B\n\nObjective:\n- C\n\nConstraints:\n- D\n\nGuidelines:\n- E\n\nIn Scope:\n- F\n\nOut of Scope:\n- G".to_string(),
            ),
            position,
            parallelizable: false,
            status: LoopSpecStatus::Pending,
            started_at: None,
            completed_at: None,
            spec_start_head: None,
            workdir: None,
        }
    }

    fn insert_pool_with_members(db: &Database, pool_id: &str, member_ids: &[&str]) {
        db.insert_pool(&crate::domain::pools::Pool {
            id: pool_id.to_string(),
            name: pool_id.to_string(),
            created_at: chrono::Utc::now(),
        })
        .unwrap();
        for spec_id in member_ids {
            db.append_pool_member(pool_id, spec_id).unwrap();
        }
    }

    #[tokio::test]
    async fn loop_engine_pool_run_walks_loop_graph_across_pool_specs_in_queue_order() {
        // Two standalone specs, queued into the pool in the *opposite* order
        // of their `position` field — proving the pool's queue order drives
        // execution, not the spec's own position. Each pass through the
        // shared loop-level check node commits to the workdir's git repo, so
        // the spec that captures the pre-commit HEAD ran first.
        let (dir, db, engine, loop_id) = bare_loop_fixture().unwrap();
        init_git_repo(dir.path());
        let initial_head = git_head(dir.path());

        let spec_a = standalone_spec("pool-spec-a", 1);
        let spec_b = standalone_spec("pool-spec-b", 2);
        db.insert_loop_spec(&spec_a).unwrap();
        db.insert_loop_spec(&spec_b).unwrap();
        // Queue order: b, then a — the reverse of position order.
        insert_pool_with_members(&db, "pool-1", &[&spec_b.id, &spec_a.id]);

        db.insert_loop_node(&LoopNode {
            id: "loop-check".to_string(),
            spec_id: None,
            loop_id: Some(loop_id.clone()),
            name: "check".to_string(),
            kind: LoopNodeKind::Check,
            config: serde_json::json!({
                "command": "echo committed >> log.txt && git add -A && git commit -q -m spec && printf APPROVED",
                "success_condition": "exit_code_0"
            }),
            position: 1,
            created_at: chrono::Utc::now(),
        })
        .unwrap();

        engine
            .run_loop(loop_id.clone(), Some("pool-1".to_string()), None)
            .await
            .unwrap();

        let lp = db.get_loop(&loop_id).unwrap().unwrap();
        assert_eq!(lp.status, LoopStatus::Completed);

        let spec_a_after = db.get_loop_spec(&spec_a.id).unwrap().unwrap();
        let spec_b_after = db.get_loop_spec(&spec_b.id).unwrap().unwrap();
        assert_eq!(spec_a_after.status, LoopSpecStatus::Completed);
        assert_eq!(spec_b_after.status, LoopSpecStatus::Completed);
        assert_eq!(db.list_loop_runs_for_spec(&spec_a.id).unwrap().len(), 1);
        assert_eq!(db.list_loop_runs_for_spec(&spec_b.id).unwrap().len(), 1);

        // spec_b ran first: nothing had been committed yet.
        assert_eq!(
            spec_b_after.spec_start_head.as_deref(),
            Some(initial_head.as_str())
        );
        // spec_a ran second: spec_b's node had already committed by then.
        assert_ne!(
            spec_a_after.spec_start_head.as_deref(),
            Some(initial_head.as_str())
        );
    }

    #[tokio::test]
    async fn loop_engine_run_workdir_override_is_used_as_check_node_cwd() {
        // The loop's own workdir must be left untouched by an override — only
        // the check node's actual working directory should change.
        let db_dir = tempdir().unwrap();
        let loop_workdir = tempdir().unwrap();
        let override_workdir = tempdir().unwrap();
        let db = Arc::new(Database::new(&db_dir.path().join("test.db")).unwrap());
        let engine = LoopEngine::new(Arc::clone(&db), Arc::new(DefaultNotificationService));

        let lp = crate::domain::loops::Loop {
            id: "wf-workdir".to_string(),
            name: "Loop".to_string(),
            description: None,
            workdir: loop_workdir.path().to_string_lossy().to_string(),
            status: LoopStatus::Draft,
            trigger: None,
            created_at: chrono::Utc::now(),
            started_at: None,
            completed_at: None,
            autorun_at: None,
        };
        db.insert_loop(&lp).unwrap();
        let spec = standalone_spec("bound-spec", 1);
        let mut bound_spec = spec.clone();
        bound_spec.loop_id = Some(lp.id.clone());
        db.insert_loop_spec(&bound_spec).unwrap();

        let override_path = override_workdir.path().to_string_lossy().to_string();
        db.insert_loop_node(&LoopNode {
            id: "loop-check".to_string(),
            spec_id: None,
            loop_id: Some(lp.id.clone()),
            name: "check".to_string(),
            kind: LoopNodeKind::Check,
            config: serde_json::json!({
                "command": format!("test \"$(pwd)\" = \"{override_path}\""),
                "success_condition": "exit_code_0"
            }),
            position: 1,
            created_at: chrono::Utc::now(),
        })
        .unwrap();

        engine
            .run_loop(lp.id.clone(), None, Some(override_path.clone()))
            .await
            .unwrap();

        let lp_after = db.get_loop(&lp.id).unwrap().unwrap();
        let spec_after = db.get_loop_spec(&bound_spec.id).unwrap().unwrap();
        assert_eq!(lp_after.status, LoopStatus::Completed);
        assert_eq!(spec_after.status, LoopSpecStatus::Completed);
        // The loop's own workdir is unchanged by the run-level override.
        assert_eq!(
            lp_after.workdir,
            loop_workdir.path().to_string_lossy().to_string()
        );
    }

    #[tokio::test]
    async fn loop_engine_legacy_run_without_pool_id_only_touches_bound_specs() {
        // A standalone spec exists in the DB (e.g. pool backlog) but isn't
        // added to any pool and isn't bound to this loop. Calling run_loop
        // without pool_id must behave exactly as before pools existed: only
        // the loop's own bound specs are touched.
        let (_dir, db, engine, loop_id, bound_spec_id) = loop_fixture().unwrap();
        let untouched = standalone_spec("untouched-standalone", 99);
        db.insert_loop_spec(&untouched).unwrap();

        db.insert_loop_node(&LoopNode {
            id: "loop-check".to_string(),
            spec_id: None,
            loop_id: Some(loop_id.clone()),
            name: "check".to_string(),
            kind: LoopNodeKind::Check,
            config: serde_json::json!({
                "command": "printf APPROVED",
                "success_condition": "exit_code_0"
            }),
            position: 1,
            created_at: chrono::Utc::now(),
        })
        .unwrap();

        engine.run_loop(loop_id.clone(), None, None).await.unwrap();

        let lp = db.get_loop(&loop_id).unwrap().unwrap();
        let bound = db.get_loop_spec(&bound_spec_id).unwrap().unwrap();
        let untouched_after = db.get_loop_spec(&untouched.id).unwrap().unwrap();

        assert_eq!(lp.status, LoopStatus::Completed);
        assert_eq!(bound.status, LoopSpecStatus::Completed);
        assert_eq!(untouched_after.status, LoopSpecStatus::Pending);
        assert!(db
            .list_loop_runs_for_spec(&untouched.id)
            .unwrap()
            .is_empty());
    }

    #[tokio::test]
    async fn loop_engine_pool_run_skips_already_completed_members() {
        let (_dir, db, engine, loop_id) = bare_loop_fixture().unwrap();

        let mut done = standalone_spec("pool-done", 1);
        done.status = LoopSpecStatus::Completed;
        let pending = standalone_spec("pool-pending", 2);
        db.insert_loop_spec(&done).unwrap();
        db.insert_loop_spec(&pending).unwrap();
        insert_pool_with_members(&db, "pool-1", &[&done.id, &pending.id]);

        db.insert_loop_node(&LoopNode {
            id: "loop-check".to_string(),
            spec_id: None,
            loop_id: Some(loop_id.clone()),
            name: "check".to_string(),
            kind: LoopNodeKind::Check,
            config: serde_json::json!({
                "command": "printf APPROVED",
                "success_condition": "exit_code_0"
            }),
            position: 1,
            created_at: chrono::Utc::now(),
        })
        .unwrap();

        engine
            .run_loop(loop_id.clone(), Some("pool-1".to_string()), None)
            .await
            .unwrap();

        let lp = db.get_loop(&loop_id).unwrap().unwrap();
        let done_after = db.get_loop_spec(&done.id).unwrap().unwrap();
        let pending_after = db.get_loop_spec(&pending.id).unwrap().unwrap();

        assert_eq!(lp.status, LoopStatus::Completed);
        assert_eq!(done_after.status, LoopSpecStatus::Completed);
        assert_eq!(pending_after.status, LoopSpecStatus::Completed);
        // The already-completed spec was skipped outright: no run recorded.
        assert!(db.list_loop_runs_for_spec(&done.id).unwrap().is_empty());
        assert_eq!(db.list_loop_runs_for_spec(&pending.id).unwrap().len(), 1);
    }

    // ── R6: live pools — append and reorder while running ────────────────

    async fn wait_for_file(path: &std::path::Path) {
        for _ in 0..500 {
            if path.exists() {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        panic!("timed out waiting for file: {}", path.display());
    }

    fn record_node(id: &str, spec_id: &str, log: &std::path::Path, label: &str) -> LoopNode {
        LoopNode {
            id: id.to_string(),
            spec_id: Some(spec_id.to_string()),
            loop_id: None,
            name: id.to_string(),
            kind: LoopNodeKind::Check,
            config: serde_json::json!({
                "command": format!(
                    "echo {label} >> \"{log}\" && printf APPROVED",
                    log = log.display(),
                ),
                "success_condition": "exit_code_0"
            }),
            position: 1,
            created_at: chrono::Utc::now(),
        }
    }

    fn touch_gate_node(
        id: &str,
        spec_id: &str,
        marker: &std::path::Path,
        gate: &std::path::Path,
    ) -> LoopNode {
        LoopNode {
            id: id.to_string(),
            spec_id: Some(spec_id.to_string()),
            loop_id: None,
            name: id.to_string(),
            kind: LoopNodeKind::Check,
            config: serde_json::json!({
                "command": format!(
                    "touch \"{marker}\"; while [ ! -f \"{gate}\" ]; do sleep 0.02; done; printf APPROVED",
                    marker = marker.display(),
                    gate = gate.display(),
                ),
                "success_condition": "exit_code_0"
            }),
            position: 1,
            created_at: chrono::Utc::now(),
        }
    }

    #[tokio::test]
    async fn loop_engine_pool_run_picks_up_spec_appended_mid_run() {
        // A spec appended to the pool while the run is in flight must still
        // get executed before the run ends: the engine re-queries the pool
        // for its next pending member at each spec boundary instead of
        // iterating a list frozen at launch.
        let (dir, db, engine, loop_id) = bare_loop_fixture().unwrap();
        let started_marker = dir.path().join("started.marker");
        let go_marker = dir.path().join("go.marker");
        let order_log = dir.path().join("order.log");

        let spec_a = standalone_spec("pool-spec-a", 1);
        let spec_b = standalone_spec("pool-spec-b", 2);
        db.insert_loop_spec(&spec_a).unwrap();
        db.insert_loop_spec(&spec_b).unwrap();
        // spec_b exists in the DB but is NOT yet in the pool — it's appended
        // below, while spec_a is mid-run.
        insert_pool_with_members(&db, "pool-1", &[&spec_a.id]);

        db.insert_loop_node(&touch_gate_node(
            "node-a",
            &spec_a.id,
            &started_marker,
            &go_marker,
        ))
        .unwrap();
        db.insert_loop_node(&record_node("node-b", &spec_b.id, &order_log, "spec-b"))
            .unwrap();

        let run_engine = engine.clone();
        let run_loop_id = loop_id.clone();
        let handle = tokio::spawn(async move {
            run_engine
                .run_loop(run_loop_id, Some("pool-1".to_string()), None)
                .await
        });

        wait_for_file(&started_marker).await;
        // spec_a is mid-run (blocked on the gate). Append spec_b to the pool
        // now, while the run is in flight.
        db.append_pool_member("pool-1", &spec_b.id).unwrap();
        std::fs::write(&go_marker, "").unwrap();

        handle.await.unwrap().unwrap();

        let lp = db.get_loop(&loop_id).unwrap().unwrap();
        let spec_a_after = db.get_loop_spec(&spec_a.id).unwrap().unwrap();
        let spec_b_after = db.get_loop_spec(&spec_b.id).unwrap().unwrap();
        assert_eq!(lp.status, LoopStatus::Completed);
        assert_eq!(spec_a_after.status, LoopSpecStatus::Completed);
        assert_eq!(
            spec_b_after.status,
            LoopSpecStatus::Completed,
            "spec appended mid-run must still be executed before the run ends"
        );
        assert_eq!(db.list_loop_runs_for_spec(&spec_b.id).unwrap().len(), 1);
    }

    #[tokio::test]
    async fn loop_engine_pool_run_reorder_changes_pick_order_mid_run() {
        // Reordering the pool's PENDING members while a run is in flight
        // must change which one the engine picks next — proving the pick is
        // a live, fresh query, not a list captured at launch.
        let (dir, db, engine, loop_id) = bare_loop_fixture().unwrap();
        let started_marker = dir.path().join("started.marker");
        let go_marker = dir.path().join("go.marker");
        let order_log = dir.path().join("order.log");

        let spec_a = standalone_spec("pool-spec-a", 1);
        let spec_b = standalone_spec("pool-spec-b", 2);
        let spec_c = standalone_spec("pool-spec-c", 3);
        db.insert_loop_spec(&spec_a).unwrap();
        db.insert_loop_spec(&spec_b).unwrap();
        db.insert_loop_spec(&spec_c).unwrap();
        // Queue order at launch: a, b, c.
        insert_pool_with_members(&db, "pool-1", &[&spec_a.id, &spec_b.id, &spec_c.id]);

        db.insert_loop_node(&touch_gate_node(
            "node-a",
            &spec_a.id,
            &started_marker,
            &go_marker,
        ))
        .unwrap();
        db.insert_loop_node(&record_node("node-b", &spec_b.id, &order_log, "spec-b"))
            .unwrap();
        db.insert_loop_node(&record_node("node-c", &spec_c.id, &order_log, "spec-c"))
            .unwrap();

        let run_engine = engine.clone();
        let run_loop_id = loop_id.clone();
        let handle = tokio::spawn(async move {
            run_engine
                .run_loop(run_loop_id, Some("pool-1".to_string()), None)
                .await
        });

        wait_for_file(&started_marker).await;
        // spec_a is mid-run. Swap the two PENDING members' order: c before b.
        db.reorder_pool_members(
            "pool-1",
            &[spec_a.id.clone(), spec_c.id.clone(), spec_b.id.clone()],
        )
        .unwrap();
        std::fs::write(&go_marker, "").unwrap();

        handle.await.unwrap().unwrap();

        let lp = db.get_loop(&loop_id).unwrap().unwrap();
        assert_eq!(lp.status, LoopStatus::Completed);

        let order = std::fs::read_to_string(&order_log).unwrap();
        let lines: Vec<&str> = order.lines().collect();
        assert_eq!(
            lines,
            vec!["spec-c", "spec-b"],
            "reorder mid-run must change which pending spec runs next"
        );
    }

    #[tokio::test]
    async fn loop_engine_pool_run_ends_when_no_pending_members_remain() {
        // Sanity check underpinning both tests above: with no gating at all,
        // a pool run with N pending members ends after exactly N specs run,
        // and picks up an appended spec before completing.
        let (dir, db, engine, loop_id) = bare_loop_fixture().unwrap();
        let order_log = dir.path().join("order.log");

        let spec_a = standalone_spec("pool-spec-a", 1);
        let spec_b = standalone_spec("pool-spec-b", 2);
        db.insert_loop_spec(&spec_a).unwrap();
        db.insert_loop_spec(&spec_b).unwrap();
        insert_pool_with_members(&db, "pool-1", &[&spec_a.id, &spec_b.id]);

        db.insert_loop_node(&record_node("node-a", &spec_a.id, &order_log, "spec-a"))
            .unwrap();
        db.insert_loop_node(&record_node("node-b", &spec_b.id, &order_log, "spec-b"))
            .unwrap();

        engine
            .run_loop(loop_id.clone(), Some("pool-1".to_string()), None)
            .await
            .unwrap();

        let lp = db.get_loop(&loop_id).unwrap().unwrap();
        assert_eq!(lp.status, LoopStatus::Completed);
        assert!(db.pool_next_pending_spec_id("pool-1").unwrap().is_none());

        let order = std::fs::read_to_string(&order_log).unwrap();
        assert_eq!(order.lines().collect::<Vec<_>>(), vec!["spec-a", "spec-b"]);
    }
}
