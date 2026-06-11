//! MCP Server handler implementing all canopy tools.
//!
//! Uses the `rmcp` SDK's `#[tool_router]` and `#[tool_handler]` macros
//! with `Parameters<T>` for proper MCP protocol compliance.

use std::sync::Arc;

use axum::http::request::Parts;
use rmcp::handler::server::common::AsRequestContext;
use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::*;
use rmcp::tool;
use rmcp::tool_handler;
use rmcp::tool_router;
use rmcp::ErrorData as McpError;
use rmcp::ServerHandler;
use tokio::sync::Notify;

#[derive(Clone, Debug)]
pub struct OptionalExtension<T>(pub Option<T>);

impl<C, T> rmcp::handler::server::common::FromContextPart<C> for OptionalExtension<T>
where
    C: AsRequestContext,
    T: Send + Sync + 'static + Clone,
{
    fn from_context_part(context: &mut C) -> Result<Self, rmcp::ErrorData> {
        Ok(OptionalExtension(
            context.as_request_context().extensions.get::<T>().cloned(),
        ))
    }
}

use crate::application::notification_service::NotificationService;
use crate::application::ports::{AgentRepository, RunRepository, StateRepository};
use crate::daemon::handler_formatting::{
    format_agent_info, format_catalog_models, format_log_output, format_temporal_agents,
    format_uptime, internal_error, make_log_path, recent_runs_output, resolve_log_path,
};
use crate::daemon::handler_helpers::{
    apply_scalar_updates, apply_trigger_updates, handle_timed_out_run, load_bound_seed_identity,
    map_action_result, new_agent_base, parse_report_status, prepare_cron_task, prepare_watch_task,
    resolve_effective_project_hash, update_agent_last_run, validate_report_summary,
    validate_run_transition, watcher_restart_needed,
};
use crate::daemon::helpers::{data_dir, error_result, notify_run_result, success_result};
use crate::daemon::params::*;
use crate::db::intelligence::IntelligenceNodeRecord;
use crate::db::Database;
use crate::domain::models::{Agent, Trigger};
use crate::domain::sync::{MessageKind, MissionImpact, WorkspaceStatus};
use crate::domain::validation::validate_id;
use crate::domain::workflow::{
    validate_spec_description_template, Workflow, WorkflowDetails, WorkflowEdge,
    WorkflowEdgeCondition, WorkflowNode, WorkflowNodeKind, WorkflowRunStatus, WorkflowSpec,
    WorkflowSpecStatus, WorkflowStatus,
};
use crate::executor::Executor;
use crate::rag::rate_limiter::RateLimiter;
use crate::shared::sync_identity::{
    header_str, CANOPY_AGENT_ID_ENV, CANOPY_AGENT_ID_HEADER, CANOPY_CLIENT_NAME_ENV,
    CANOPY_CLIENT_NAME_HEADER,
};
use crate::sync_manager::SyncManager;
use crate::watchers::WatcherEngine;
use crate::workflow_engine::WorkflowEngine;

const MISSING_SYNC_IDENTITY_MESSAGE: &str =
    "Missing Canopy session identity. Launch via `canopy bridge --id <AGENT_ID>` so requests include Canopy identity headers.";

fn missing_sync_identity_error() -> McpError {
    McpError::invalid_params(MISSING_SYNC_IDENTITY_MESSAGE.to_string(), None)
}

fn validate_non_empty(value: &str, field: &str) -> Result<(), String> {
    if value.trim().is_empty() {
        Err(format!("{field} must not be empty."))
    } else {
        Ok(())
    }
}

fn validate_absolute_dir(path: &str) -> Result<(), String> {
    let p = std::path::Path::new(path);
    if !p.is_absolute() {
        return Err("Workflow workdir must be an absolute path.".into());
    }
    if !p.is_dir() {
        return Err("Workflow workdir must point to an existing directory.".into());
    }
    Ok(())
}

fn validate_workflow_exists(db: &Database, workflow_id: &str) -> Result<(), String> {
    db.get_workflow(workflow_id)
        .map_err(|e| e.to_string())?
        .is_some()
        .then_some(())
        .ok_or_else(|| format!("Workflow '{workflow_id}' not found."))
}

fn validate_spec_exists(db: &Database, spec_id: &str) -> Result<WorkflowSpec, String> {
    db.get_workflow_spec(spec_id)
        .map_err(|e| e.to_string())?
        .ok_or_else(|| format!("Spec '{spec_id}' not found."))
}

fn validate_node_exists(db: &Database, node_id: &str) -> Result<WorkflowNode, String> {
    db.get_workflow_node(node_id)
        .map_err(|e| e.to_string())?
        .ok_or_else(|| format!("Workflow node '{node_id}' not found."))
}

fn validate_edge_exists(db: &Database, edge_id: &str) -> Result<WorkflowEdge, String> {
    db.get_workflow_edge(edge_id)
        .map_err(|e| e.to_string())?
        .ok_or_else(|| format!("Workflow edge '{edge_id}' not found."))
}

fn validate_position_conflict(
    db: &Database,
    workflow_id: &str,
    exclude_spec_id: &str,
    position: i64,
) -> Result<(), String> {
    let conflict = db
        .list_workflow_specs(workflow_id)
        .map_err(|e| e.to_string())?
        .into_iter()
        .any(|s| s.id != exclude_spec_id && s.position == position);
    if conflict {
        Err(format!(
            "Workflow '{workflow_id}' already has a spec at position {position}."
        ))
    } else {
        Ok(())
    }
}

fn validate_node_position_conflict(
    db: &Database,
    spec_id: &str,
    exclude_node_id: &str,
    position: i64,
) -> Result<(), String> {
    let conflict = db
        .list_workflow_nodes(spec_id)
        .map_err(|e| e.to_string())?
        .into_iter()
        .any(|n| n.id != exclude_node_id && n.position == position);
    if conflict {
        Err(format!(
            "Spec '{spec_id}' already has a node at position {position}."
        ))
    } else {
        Ok(())
    }
}

fn validate_edge_condition(condition: &str) -> Result<WorkflowEdgeCondition, String> {
    WorkflowEdgeCondition::from_str(condition.trim())
        .ok_or_else(|| "Workflow edge condition must be one of: pass, fail, always.".to_string())
}

fn validate_node_kind(kind: &str) -> Result<WorkflowNodeKind, String> {
    WorkflowNodeKind::from_str(kind.trim())
        .ok_or_else(|| "Workflow node kind must be one of: agent, check, gate.".to_string())
}

fn validate_at_least_one_bool(updates: &[bool], field_name: &str) -> Result<(), String> {
    if updates.iter().all(|&b| !b) {
        Err(format!(
            "{field_name} requires at least one field to update."
        ))
    } else {
        Ok(())
    }
}

fn build_workflow_update_response(workflow_id: &str) -> CallToolResult {
    success_result(&format!("Workflow '{workflow_id}' updated."))
}

fn build_spec_update_response(spec_id: &str) -> CallToolResult {
    success_result(&format!("Workflow spec '{spec_id}' updated."))
}

fn build_node_update_response(node_id: &str) -> CallToolResult {
    success_result(&format!("Workflow node '{node_id}' updated."))
}

fn build_id_result(id: &str, key: &str) -> CallToolResult {
    let mut map = serde_json::Map::new();
    map.insert(key.to_string(), serde_json::json!(id));
    CallToolResult::success(vec![Content::text(
        serde_json::to_string_pretty(&serde_json::Value::Object(map)).unwrap_or_default(),
    )])
}

struct SpecRunInfo {
    name: String,
    current_node: Option<String>,
    blocker: Option<String>,
}

fn build_spec_run_info(db: &Database, spec: &WorkflowSpec) -> Result<SpecRunInfo, McpError> {
    let runs = db
        .list_workflow_runs_for_spec(&spec.id)
        .map_err(internal_error)?;
    let current_node = runs
        .iter()
        .rev()
        .find(|run| run.status == WorkflowRunStatus::Running)
        .or_else(|| runs.last())
        .map(|run| run.node_id.clone());
    let blocker = runs.last().and_then(workflow_run_blocker);
    Ok(SpecRunInfo {
        name: spec.name.clone(),
        current_node,
        blocker,
    })
}

fn build_workflow_summary_json(
    db: &Database,
    workflow: &Workflow,
) -> Result<serde_json::Value, McpError> {
    let specs = db
        .list_workflow_specs(&workflow.id)
        .map_err(internal_error)?;
    let current_spec = specs
        .into_iter()
        .find(|spec| {
            matches!(
                spec.status,
                WorkflowSpecStatus::Running | WorkflowSpecStatus::Pending
            )
        })
        .map(|spec| build_spec_run_info(db, &spec))
        .transpose()?;

    Ok(serde_json::json!({
        "id": workflow.id,
        "name": workflow.name,
        "status": workflow.status.as_str(),
        "current_spec": current_spec.as_ref().map(|v| &v.name),
        "current_node": current_spec.as_ref().and_then(|v| v.current_node.as_ref()),
        "blocked": current_spec.as_ref().is_some_and(|v| v.blocker.is_some()),
        "blocker": current_spec.and_then(|v| v.blocker),
        "created_at": workflow.created_at.to_rfc3339(),
        "workdir": workflow.workdir,
    }))
}

fn build_workflow_list_json(
    db: &Database,
    workflows: &[Workflow],
) -> Result<Vec<serde_json::Value>, McpError> {
    workflows
        .iter()
        .map(|workflow| build_workflow_summary_json(db, workflow))
        .collect()
}

fn build_get_tools_response(scope: &str) -> serde_json::Value {
    match scope {
        "session_start" => serde_json::json!({
            "scope": "session_start",
            "risk": "low",
            "protocol": [
                "1. Call intelligence_get_context(scope=\"light\") to load workspace brief.",
                "2. Check active_missions in sync context — align your work with open missions.",
                "3. If starting a new thread of work, call sync_declare_intent to register your mission.",
                "4. Respond to the user with context in hand."
            ],
            "tools": [
                "intelligence_get_context — pull session history, facts, patterns",
                "sync_get_context — check active missions and workspace vibe",
                "sync_declare_intent — announce your mission (impact: low/medium/high/breaking)"
            ]
        }),
        "file_write" => serde_json::json!({
            "scope": "file_write",
            "risk": "high",
            "protocol": [
                "1. Call sync_get_context(workdir=...) — check for conflicting missions on this path.",
                "2. If no conflict, call sync_declare_intent(impact=\"high\", mission=\"...\").",
                "3. Modify the file.",
                "4. Call sync_report_status(status=\"stable\", message=\"Changes complete: ...\")."
            ],
            "tools": [
                "sync_get_context — check active missions, detect conflicts",
                "sync_declare_intent — announce what you're changing and why",
                "sync_report_status — report stable/unstable/testing after the change"
            ]
        }),
        "test_run" => serde_json::json!({
            "scope": "test_run",
            "risk": "medium",
            "protocol": [
                "1. Call sync_broadcast(kind=\"info\", message=\"Running tests: <suite/command>\").",
                "2. Run the tests.",
                "3. Call sync_broadcast(kind=\"info\", message=\"Tests complete: PASSED/FAILED — <summary>\").",
                "4. If failed, call sync_report_status(status=\"unstable\", message=\"Test failure: ...\")."
            ],
            "tools": [
                "sync_broadcast — announce test start and result to peer agents",
                "sync_report_status — mark workspace unstable if tests fail"
            ]
        }),
        "close_session" => serde_json::json!({
            "scope": "close_session",
            "risk": "low",
            "protocol": [
                "1. Call intelligence_upsert(kind=\"session\", title=\"<mission>\", body=\"<what was done>\", \
                   metadata={workdir, summary, ...}) — store session summary in project context.",
                "2. Call sync_report_status(status=\"stable\", message=\"Mission complete: <summary>\") \
                   — the daemon closes your mission automatically on exit.",
                "3. Do NOT manually call any close/shutdown tool — daemon handles it."
            ],
            "tools": [
                "intelligence_upsert — persist session summary as a 'session' node in project context",
                "sync_report_status — leave a clean 'stable' marker for the next agent"
            ]
        }),
        "multi_agent" => serde_json::json!({
            "scope": "multi_agent",
            "risk": "varies",
            "protocol": [
                "Follow the action-risk table: low=execute, medium=broadcast, high=declare+execute+report, breaking=same as high with impact=breaking.",
                "Always non-blocking — act on last-known state, never wait for responses.",
                "Communicate intent not implementation — missions explain what and why, not how."
            ],
            "tools": [
                "sync_get_context — check active missions and workspace vibe (call first)",
                "sync_declare_intent — announce mission (impact: low/medium/high/breaking)",
                "sync_broadcast — send info/query/answer messages to peers",
                "sync_report_status — report stable/unstable/testing after actions",
                "intelligence_get_context(scope=\"full\") — deep project-context pull for architecture work",
                "intelligence_upsert — persist facts, patterns, session summaries",
                "intelligence_search — find prior art or decisions in project context"
            ]
        }),
        _ => unreachable!(),
    }
}

fn rag_result_json(r: &crate::rag::vector_store::SearchResult) -> serde_json::Value {
    serde_json::json!({
        "source": r.file_path,
        "content": r.content,
        "distance": r.distance,
    })
}

#[derive(Clone)]
pub struct TaskTriggerHandler {
    pub db: Arc<Database>,
    pub executor: Arc<Executor>,
    pub watcher_engine: Arc<WatcherEngine>,
    pub scheduler_notify: Arc<Notify>,
    pub workflow_engine: Arc<WorkflowEngine>,
    pub notification_service: Arc<dyn NotificationService>,
    pub sync_manager: Arc<SyncManager>,
    /// Rate limiters for rag_search (10 calls/min). Keyed by agent_id.
    pub rag_limiters: Arc<tokio::sync::Mutex<std::collections::HashMap<String, RateLimiter>>>,
    pub start_time: std::time::Instant,
    pub port: u16,
    #[allow(dead_code)]
    tool_router: ToolRouter<Self>,
}

#[tool_router]
#[allow(clippy::too_many_arguments)]
impl TaskTriggerHandler {
    fn resolve_sync_agent_id(&self, parts: Option<&Parts>) -> Result<String, McpError> {
        if let Some(parts) = parts {
            if let Some(agent_id) = header_str(parts, CANOPY_AGENT_ID_HEADER) {
                return Ok(agent_id.to_string());
            }
        }
        if let Ok(agent_id) = std::env::var(CANOPY_AGENT_ID_ENV) {
            let trimmed = agent_id.trim();
            if !trimmed.is_empty() {
                return Ok(trimmed.to_string());
            }
        }
        Err(missing_sync_identity_error())
    }

    fn resolve_sync_client_name(&self, parts: Option<&Parts>) -> Option<String> {
        if let Some(parts) = parts {
            if let Some(client_name) = header_str(parts, CANOPY_CLIENT_NAME_HEADER) {
                return Some(client_name.to_string());
            }
        }
        std::env::var(CANOPY_CLIENT_NAME_ENV)
            .ok()
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty())
    }

    fn reject_if_nursery(&self, parts: Option<&Parts>) -> Result<(), McpError> {
        let agent_id = self.resolve_sync_agent_id(parts)?;
        match self.db.get_session_type(&agent_id) {
            Ok(Some(st)) if st == "nursery" => {
                Err(McpError::invalid_params(
                    "This tool is not available during a seed creation session (nursery). Complete the seed identity interview first.".to_string(),
                    None,
                ))
            }
            Ok(_) => Ok(()),
            Err(e) => Err(McpError::internal_error(e.to_string(), None)),
        }
    }

    /// Fetch knowledge for the context endpoint.
    /// When project_hash is provided, returns project-scoped facts/patterns
    /// alongside session nodes; otherwise returns all node kinds.
    fn fetch_context_knowledge(
        &self,
        project_hash: Option<&str>,
        scope: &str,
    ) -> Result<
        (
            Vec<crate::db::intelligence::IntelligenceNodeRecord>,
            Vec<crate::db::intelligence::IntelligenceNodeRecord>,
        ),
        String,
    > {
        let knowledge_limit = if scope == "full" { 20 } else { 5 };
        if let Some(ph) = project_hash {
            let pk_limit = if scope == "full" { 50 } else { 20 };
            let pk = self
                .db
                .list_project_knowledge(ph, None, pk_limit)
                .map_err(|e| e.to_string())?;
            let generic = self
                .db
                .list_intelligence_nodes(Some("session"), knowledge_limit)
                .map_err(|e| e.to_string())?;
            Ok((generic, pk))
        } else {
            let generic = self
                .db
                .list_intelligence_nodes(None, knowledge_limit)
                .map_err(|e| e.to_string())?;
            Ok((generic, Vec::new()))
        }
    }

    pub fn new(
        db: Arc<Database>,
        executor: Arc<Executor>,
        watcher_engine: Arc<WatcherEngine>,
        scheduler_notify: Arc<Notify>,
        workflow_engine: Arc<WorkflowEngine>,
        notification_service: Arc<dyn NotificationService>,
        sync_manager: Arc<SyncManager>,
        port: u16,
    ) -> Self {
        Self {
            db,
            executor,
            watcher_engine,
            scheduler_notify,
            workflow_engine,
            notification_service,
            sync_manager,
            rag_limiters: Arc::new(tokio::sync::Mutex::new(std::collections::HashMap::new())),
            start_time: std::time::Instant::now(),
            port,
            tool_router: Self::tool_router(),
        }
    }

    /// Create or update an agent. Supports cron triggers (schedule), watch triggers
    /// (path + events), or manual-only agents (no trigger). When updating an existing
    /// agent, only the fields you provide are changed.
    #[tool(
        name = "agent_add",
        description = "Create or update a scheduled background_agent. \
         Use agent_models to see available model options. \
         The schedule field must be a standard 5-field cron expression. \
         Common patterns: '*/5 * * * *' (every 5 min), '0 9 * * *' (daily 9am), \
         '0 9 * * 1-5' (weekdays 9am), '0 */2 * * *' (every 2 hours), \
         '30 14 1,15 * *' (1st and 15th at 2:30pm). \
         Fields: minute(0-59) hour(0-23) day(1-31) month(1-12) weekday(0-6, 0=Sun). \
         Use duration_minutes for temporary agents that auto-expire. \
         The cli parameter is optional — if omitted, auto-detects from registry. \
         The model parameter is optional — if omitted, the CLI uses its configured default."
    )]
    async fn task_add(
        &self,
        Parameters(params): Parameters<TaskAddParams>,
    ) -> Result<CallToolResult, McpError> {
        use crate::scheduler::validate_cron;

        let prepared = match prepare_cron_task(&params, &validate_cron) {
            Ok(prepared) => prepared,
            Err(e) => return Ok(error_result(&e)),
        };

        let log_path = make_log_path(&params.id)?;
        let mut agent = new_agent_base(
            params.id.clone(),
            params.prompt,
            prepared.cli,
            params.model,
            params.working_dir,
            params.timeout_minutes,
            log_path,
        );
        agent.trigger = Some(Trigger::Cron {
            schedule_expr: prepared.schedule_expr.clone(),
        });
        agent.expires_at = prepared.expires_at;

        self.db.upsert_agent(&agent).map_err(internal_error)?;
        if let Some(workdir) = agent.working_dir.as_deref() {
            if let Err(e) = self.db.register_project_path(std::path::Path::new(workdir)) {
                tracing::debug!("Could not register project at {workdir}: {e}");
            }
        }
        self.scheduler_notify.notify_one();

        Ok(success_result(&format!(
            "Agent '{}' registered with schedule '{}'{}\nThe daemon's internal scheduler will execute this agent automatically.",
            agent.id,
            prepared.schedule_expr,
            agent
                .expires_at
                .map(|expires_at| format!(" (expires: {})", expires_at.to_rfc3339()))
                .unwrap_or_default()
        )))
    }

    /// Register a file or directory watcher.
    #[tool(
        name = "agent_watch",
        description = "Watch a file or directory for changes and execute a prompt when events occur. \
         The cli parameter is optional — if omitted, auto-detects from registry. \
         The model parameter is optional — if omitted, the CLI uses its configured default model."
    )]
    async fn task_watch(
        &self,
        Parameters(params): Parameters<TaskWatchParams>,
    ) -> Result<CallToolResult, McpError> {
        let prepared = match prepare_watch_task(&params) {
            Ok(prepared) => prepared,
            Err(e) => return Ok(error_result(&e)),
        };

        let log_path = make_log_path(&params.id)?;
        let mut agent = new_agent_base(
            params.id.clone(),
            params.prompt,
            prepared.cli,
            params.model,
            None,
            params.timeout_minutes,
            log_path,
        );
        agent.trigger = Some(Trigger::Watch {
            path: params.path.clone(),
            events: prepared.events,
            debounce_seconds: prepared.debounce_seconds,
            recursive: prepared.recursive,
        });

        self.db.upsert_agent(&agent).map_err(internal_error)?;

        if let Err(e) = self.watcher_engine.start_watcher(&agent).await {
            tracing::warn!("Watcher '{}' saved but failed to start: {}", agent.id, e);
            return Ok(CallToolResult::success(vec![Content::text(format!(
                "Watcher '{}' registered but could not start watching '{}': {}. It will be retried on daemon restart.",
                agent.id, params.path, e
            ))]));
        }

        Ok(success_result(&format!(
            "Watcher '{}' active on '{}' for events: {:?}",
            agent.id, params.path, params.events
        )))
    }

    /// List all registered agents with status.
    #[tool(
        name = "agent_list",
        description = "List all registered scheduled agents with their current status"
    )]
    async fn task_list(&self) -> Result<CallToolResult, McpError> {
        let agents = self
            .db
            .list_agents()
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;

        if agents.is_empty() {
            return Ok(success_result("No agents registered."));
        }

        let mut lines = vec![format!("Found {} agent(s):\n", agents.len())];

        for a in &agents {
            let mut info = format_agent_info(a);

            if a.is_watch() {
                let runtime_active = self.watcher_engine.is_active(&a.id).await;
                let watch_status = if !a.enabled {
                    "paused"
                } else if runtime_active {
                    "active"
                } else {
                    "registered (not running)"
                };
                info.push_str(&format!("  Watch status: {}\n", watch_status));
            }

            lines.push(info);
        }

        Ok(CallToolResult::success(vec![Content::text(
            lines.join("\n"),
        )]))
    }

    /// Remove an agent completely.
    #[tool(
        name = "agent_remove",
        description = "Remove an agent completely — deletes from database and stops any active watcher"
    )]
    async fn task_remove(
        &self,
        Parameters(IdParam { id }): Parameters<IdParam>,
    ) -> Result<CallToolResult, McpError> {
        let existing = self
            .db
            .get_agent(&id)
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;

        if existing.is_none() {
            return Ok(error_result(&format!("No agent found with ID '{}'", id)));
        }

        let _ = self.watcher_engine.stop_watcher(&id).await;

        self.db
            .delete_agent(&id)
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;

        self.scheduler_notify.notify_one();
        Ok(success_result(&format!("Agent '{}' removed", id)))
    }

    /// Enable a disabled agent.
    #[tool(
        name = "agent_enable",
        description = "Enable a disabled agent — resumes scheduling or file watching"
    )]
    async fn task_enable(
        &self,
        Parameters(IdParam { id }): Parameters<IdParam>,
    ) -> Result<CallToolResult, McpError> {
        let Some(existing) = self
            .db
            .get_agent(&id)
            .map_err(|e| McpError::internal_error(e.to_string(), None))?
        else {
            return Ok(error_result(&format!("No agent found with ID '{}'", id)));
        };

        if existing.is_expired() {
            let mut updated = existing.clone();
            updated.expires_at = None;
            self.db
                .upsert_agent(&updated)
                .map_err(|e| McpError::internal_error(e.to_string(), None))?;
        }

        self.db
            .update_agent_enabled(&id, true)
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;

        self.scheduler_notify.notify_one();

        if existing.is_watch() {
            let _ = self.watcher_engine.start_watcher(&existing).await;
        }

        Ok(success_result(&format!("Agent '{}' enabled", id)))
    }

    /// Disable an agent without removing it.
    #[tool(
        name = "agent_disable",
        description = "Disable an agent without removing it — pauses scheduling or file watching"
    )]
    async fn task_disable(
        &self,
        Parameters(IdParam { id }): Parameters<IdParam>,
    ) -> Result<CallToolResult, McpError> {
        let Some(existing) = self
            .db
            .get_agent(&id)
            .map_err(|e| McpError::internal_error(e.to_string(), None))?
        else {
            return Ok(error_result(&format!("No agent found with ID '{}'", id)));
        };

        self.db
            .update_agent_enabled(&id, false)
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;

        if existing.is_watch() {
            let _ = self.watcher_engine.stop_watcher(&id).await;
        }

        self.scheduler_notify.notify_one();
        Ok(success_result(&format!("Agent '{}' disabled", id)))
    }

    /// Execute an agent immediately, outside its schedule.
    #[tool(
        name = "agent_run",
        description = "Execute an agent immediately outside its schedule — useful for testing"
    )]
    async fn agent_run(
        &self,
        Parameters(IdParam { id }): Parameters<IdParam>,
    ) -> Result<CallToolResult, McpError> {
        let existing = self
            .db
            .get_agent(&id)
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;

        let Some(agent) = existing else {
            return Ok(error_result(&format!("No agent found with ID '{}'", id)));
        };

        let executor = Arc::clone(&self.executor);
        let notification_service = self.notification_service.clone();
        let agent_id = id.clone();

        tokio::spawn(async move {
            let result = executor.execute_agent(&agent, true).await;
            notify_run_result(
                &notification_service,
                &agent_id,
                result,
                "Manual run failed",
            );
        });

        Ok(success_result(&format!(
            "Agent '{}' launched in background. Use agent_logs to check progress.",
            id
        )))
    }

    /// Get daemon status and statistics.
    #[tool(
        name = "agent_status",
        description = "Get overall daemon health, scheduler state, and statistics"
    )]
    async fn task_status(&self) -> Result<CallToolResult, McpError> {
        let agents = self
            .db
            .list_agents()
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;

        let active_agents = agents
            .iter()
            .filter(|a| a.enabled && !a.is_expired())
            .count();
        let active_watchers = self.watcher_engine.active_count().await;
        let uptime_str = format_uptime(self.start_time.elapsed().as_secs());
        let log_dir = data_dir()
            .map(|d| d.join("logs").to_string_lossy().to_string())
            .unwrap_or_else(|_| "unknown".to_string());

        let cron_count = agents.iter().filter(|a| a.is_cron()).count();
        let watch_count = agents.iter().filter(|a| a.is_watch()).count();
        let manual_count = agents.len() - cron_count - watch_count;

        let (transport, port_str) = transport_details(self.port);

        let mut status = format!(
            "canopy v{}\n\
             Uptime: {}\n\
             Transport: {}\n\
             Port: {}\n\
             Scheduler: internal (tokio)\n\
             Active agents: {} / {} (cron: {}, watch: {}, manual: {})\n\
             Active watchers: {}\n\
             Log directory: {}",
            env!("CARGO_PKG_VERSION"),
            uptime_str,
            transport,
            port_str,
            active_agents,
            agents.len(),
            cron_count,
            watch_count,
            manual_count,
            active_watchers,
            log_dir,
        );

        append_temporal_agents_section(&mut status, &agents);

        Ok(CallToolResult::success(vec![Content::text(status)]))
    }

    /// List available AI models.
    #[tool(
        name = "agent_models",
        description = "List common AI models available for use with agents. Returns provider/model strings that can be passed to the model field of agent_add or agent_watch."
    )]
    async fn task_models(&self) -> Result<CallToolResult, McpError> {
        // The catalog load touches disk and possibly the network; keep it off
        // the async executor.
        let catalog = tokio::task::spawn_blocking(crate::domain::models_db::load_catalog)
            .await
            .ok()
            .flatten();

        let Some(catalog) = catalog else {
            return Ok(error_result(
                "Model catalog unavailable: could not reach models.dev and no local \
                 cache exists at ~/.canopy/models_cache.json. Omit the model field to \
                 use the CLI's default, or retry once network access is restored.",
            ));
        };

        let result = format!(
            "Available models (use the model id as the model field):\n\
             {}\n\n\
             Note: Model availability depends on the CLI's configured API keys.\n\
             If model is omitted, the CLI uses its own default.",
            format_catalog_models(&catalog)
        );

        Ok(CallToolResult::success(vec![Content::text(result)]))
    }

    /// Get log output for an agent.
    #[tool(
        name = "agent_logs",
        description = "Get the log output for an agent with optional line and time filters"
    )]
    async fn task_logs(
        &self,
        Parameters(params): Parameters<TaskLogsParams>,
    ) -> Result<CallToolResult, McpError> {
        let log_path = resolve_log_path(&self.db, &params.id)?;
        let path = std::path::Path::new(&log_path);
        if !path.exists() {
            return Ok(success_result(&format!(
                "No logs found for '{}'. The agent has not been executed yet.",
                params.id
            )));
        }

        let output = format_log_output(
            path,
            &params.id,
            params.since.as_deref(),
            params.lines.unwrap_or(50),
        )?;
        let Some(run_info) = recent_runs_output(&self.db, &params.id) else {
            return Ok(CallToolResult::success(vec![Content::text(output)]));
        };

        Ok(CallToolResult::success(vec![Content::text(format!(
            "{output}{run_info}"
        ))]))
    }

    /// Update fields of an existing agent without recreating it.
    #[tool(
        name = "agent_update",
        description = "Modify an existing agent. Only the provided fields are updated — omitted fields remain unchanged. Auto-detects whether the agent is a cron or watch agent and applies the appropriate fields."
    )]
    async fn task_update(
        &self,
        Parameters(params): Parameters<TaskUpdateParams>,
    ) -> Result<CallToolResult, McpError> {
        use crate::scheduler::validate_cron;

        if let Err(e) = validate_id(&params.id) {
            return Ok(error_result(&e));
        }

        let Some(mut agent) = self.db.get_agent(&params.id).map_err(internal_error)? else {
            return Ok(error_result(&format!(
                "No agent found with ID '{}'",
                params.id
            )));
        };

        if let Err(e) = apply_scalar_updates(&mut agent, &params) {
            return Ok(error_result(&e));
        }
        if let Err(e) = apply_trigger_updates(&mut agent, &params, &validate_cron) {
            return Ok(error_result(&e));
        }

        self.db.upsert_agent(&agent).map_err(internal_error)?;
        if agent.is_cron() {
            self.scheduler_notify.notify_one();
        }

        if let Some(result) = self.restart_updated_watcher(&params, &agent).await? {
            return Ok(result);
        }

        Ok(success_result(&format!(
            "Agent '{}' updated successfully.",
            params.id
        )))
    }

    /// Report execution status from within a running agent.
    #[tool(
        name = "agent_report",
        description = "Report execution status for a running agent. The run_id is provided in the agent execution prompt. Call with status='in_progress' immediately when starting, then status='success' or status='error' with a summary when finished."
    )]
    async fn task_report(
        &self,
        Parameters(params): Parameters<TaskReportParams>,
    ) -> Result<CallToolResult, McpError> {
        let status = match parse_report_status(&params.status) {
            Ok(status) => status,
            Err(e) => return Ok(error_result(e)),
        };
        if let Err(e) = validate_report_summary(status, params.summary.as_deref()) {
            return Ok(error_result(e));
        }

        let Some(run) = self.db.get_run(&params.run_id).map_err(internal_error)? else {
            return Err(internal_error(format!(
                "Run '{}' not found.",
                params.run_id
            )));
        };
        if let Some(result) = handle_timed_out_run(&self.db, &params.run_id, &run) {
            return Ok(result);
        }
        if let Err(e) = validate_run_transition(run.status, status) {
            return Ok(error_result(&e));
        }

        let updated = self
            .db
            .update_run_status(&params.run_id, status, params.summary.as_deref())
            .map_err(internal_error)?;
        if !updated {
            return Ok(error_result(&format!(
                "Failed to update run '{}'.",
                params.run_id
            )));
        }

        update_agent_last_run(&self.db, &run, status);
        Ok(success_result(&format!(
            "Run '{}' status updated to '{}'.",
            params.run_id, status
        )))
    }

    #[tool(
        name = "sync_declare_intent",
        description = "Declare a high-level mission for the current workdir. Non-blocking. \
         Use this to announce major work before you start changing things. \
         The actor identity is resolved automatically from the current Canopy session."
    )]
    async fn sync_declare_intent(
        &self,
        Parameters(params): Parameters<SyncDeclareIntentParams>,
        OptionalExtension(parts): OptionalExtension<Parts>,
    ) -> Result<CallToolResult, McpError> {
        self.reject_if_nursery(parts.as_ref())?;
        let Some(impact) = MissionImpact::from_str(&params.impact) else {
            return Ok(error_result(
                "Invalid impact. Must be: low, high, breaking.",
            ));
        };
        let agent_id = self.resolve_sync_agent_id(parts.as_ref())?;
        let client_name = self.resolve_sync_client_name(parts.as_ref());

        Ok(map_action_result(
            self.sync_manager
                .declare_intent(
                    &params.workdir,
                    &agent_id,
                    client_name.as_deref(),
                    &params.mission,
                    impact,
                    &params.description,
                )
                .await,
            "Intent declared.",
        ))
    }

    #[tool(
        name = "sync_report_status",
        description = "Report the current workspace status for your mission. Non-blocking. \
         status: stable | unstable | testing. The actor identity is resolved \
         automatically from the current Canopy session."
    )]
    async fn sync_report_status(
        &self,
        Parameters(params): Parameters<SyncReportStatusParams>,
        OptionalExtension(parts): OptionalExtension<Parts>,
    ) -> Result<CallToolResult, McpError> {
        self.reject_if_nursery(parts.as_ref())?;
        let Some(status) = WorkspaceStatus::from_str(&params.status) else {
            return Ok(error_result(
                "Invalid status. Must be: stable, unstable, testing.",
            ));
        };
        let agent_id = self.resolve_sync_agent_id(parts.as_ref())?;
        let client_name = self.resolve_sync_client_name(parts.as_ref());

        Ok(map_action_result(
            self.sync_manager
                .report_status(
                    &params.workdir,
                    &agent_id,
                    client_name.as_deref(),
                    status,
                    &params.message,
                )
                .await,
            "Status reported.",
        ))
    }

    /// Broadcast a message to the workdir sync channel (non-blocking).
    #[tool(
        name = "sync_broadcast",
        description = "Broadcast a message to the workdir sync channel. Non-blocking. \
         kind: info | query | answer. The actor identity is resolved \
         automatically from the current Canopy session."
    )]
    async fn sync_broadcast(
        &self,
        Parameters(params): Parameters<SyncBroadcastParams>,
        OptionalExtension(parts): OptionalExtension<Parts>,
    ) -> Result<CallToolResult, McpError> {
        self.reject_if_nursery(parts.as_ref())?;
        let Some(kind) = MessageKind::from_str(&params.kind) else {
            return Ok(error_result("Invalid kind. Must be: info, query, answer."));
        };
        if !kind.is_chatter() {
            return Ok(error_result(
                "sync_broadcast only accepts: info, query, answer.",
            ));
        }

        let payload = params
            .metadata
            .as_ref()
            .map(|metadata| metadata.to_string());
        let agent_id = self.resolve_sync_agent_id(parts.as_ref())?;
        let client_name = self.resolve_sync_client_name(parts.as_ref());
        Ok(map_action_result(
            self.sync_manager
                .broadcast(
                    &params.workdir,
                    &agent_id,
                    client_name.as_deref(),
                    kind,
                    &params.message,
                    payload.as_deref(),
                )
                .await,
            "Message broadcast.",
        ))
    }

    #[tool(
        name = "sync_get_context",
        description = "Get active missions, recent sync chatter, and current workspace vibe for a workdir."
    )]
    async fn sync_get_context(
        &self,
        Parameters(params): Parameters<SyncGetContextParams>,
        OptionalExtension(parts): OptionalExtension<Parts>,
    ) -> Result<CallToolResult, McpError> {
        self.reject_if_nursery(parts.as_ref())?;
        let limit = params.limit.unwrap_or(10);

        let context = self
            .sync_manager
            .get_context(&params.workdir, limit)
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;

        let intents_json: Vec<serde_json::Value> = context
            .active_intents
            .iter()
            .map(|intent| {
                serde_json::json!({
                    "agent_id": intent.agent_id,
                    "agent_name": intent.agent_name,
                    "mission": intent.mission,
                    "impact": intent.impact.as_str(),
                    "description": intent.description,
                    "status": intent.status.as_str(),
                    "since": intent.since,
                })
            })
            .collect();

        let chatter_json: Vec<serde_json::Value> = context
            .recent_chatter
            .iter()
            .map(|message| {
                serde_json::json!({
                    "agent_id": message.agent_id,
                    "agent_name": message.agent_name,
                    "kind": message.kind.as_str(),
                    "message": message.message,
                    "ts": message.created_at,
                })
            })
            .collect();

        let summary = if context.active_intents.is_empty() {
            "No active missions.".to_owned()
        } else {
            let parts: Vec<String> = context
                .active_intents
                .iter()
                .map(|intent| format!("{}: {}", intent.agent_name, intent.mission))
                .collect();
            parts.join("; ")
        };

        let out = serde_json::json!({
            "active_intents": intents_json,
            "recent_chatter": chatter_json,
            "vibe": context.vibe.as_str(),
            "summary": summary,
        });

        Ok(CallToolResult::success(vec![Content::text(
            serde_json::to_string_pretty(&out).unwrap_or_default(),
        )]))
    }

    #[tool(
        name = "intelligence_get_context",
        description = "Return project context at the requested scope: light or full. \
         When project_hash is provided (or auto-detected from session workdir), \
         returns project-scoped facts and patterns instead of generic knowledge."
    )]
    async fn intelligence_get_context(
        &self,
        Parameters(params): Parameters<IntelligenceGetContextParams>,
        OptionalExtension(parts): OptionalExtension<Parts>,
    ) -> Result<CallToolResult, McpError> {
        self.reject_if_nursery(parts.as_ref())?;
        let scope = params.scope.trim().to_lowercase();
        let (session_limit, _knowledge_limit, sync_limit, dependency_limit) = match scope.as_str() {
            "light" => (2, 5, 8, 0),
            "full" => (10, 20, 25, 30),
            _ => return Ok(error_result("Invalid scope. Must be: light or full.")),
        };

        // Auto-detect project_hash from session workdir if not provided.
        let resolved_agent_id = self.resolve_sync_agent_id(parts.as_ref()).ok();
        let effective_project_hash = resolved_agent_id.as_deref().and_then(|agent_id| {
            resolve_effective_project_hash(&self.db, params.project_hash.as_deref(), agent_id)
        });

        let sessions = self
            .db
            .list_intelligence_nodes(Some("session"), session_limit)
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;

        let (knowledge, project_knowledge) = self
            .fetch_context_knowledge(effective_project_hash.as_deref(), &scope)
            .map_err(|e| McpError::internal_error(e, None))?;

        let sync_messages = self
            .db
            .list_recent_sync_messages(sync_limit)
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;
        let cross_project_dependencies = if scope == "full" {
            self.db
                .list_cross_project_dependencies(dependency_limit)
                .map_err(|e| McpError::internal_error(e.to_string(), None))?
        } else {
            Vec::new()
        };

        let related_projects = if scope == "full" {
            if let Some(ref ph) = effective_project_hash {
                self.db
                    .list_related_projects(ph, 10)
                    .map_err(|e| McpError::internal_error(e.to_string(), None))?
            } else {
                Vec::new()
            }
        } else {
            Vec::new()
        };

        let summary_prefix = if scope == "light" {
            "Light context"
        } else {
            "Full context"
        };

        let mut out = serde_json::json!({
            "scope": scope,
            "summary": format!(
                "{}: {} session node(s), {} knowledge node(s), {} sync message(s).",
                summary_prefix,
                sessions.len(),
                knowledge.len(),
                sync_messages.len()
            ),
            "sessions": sessions.iter().map(intelligence_node_json).collect::<Vec<_>>(),
            "knowledge": knowledge.iter().map(intelligence_node_json).collect::<Vec<_>>(),
            "sync_messages": sync_messages.iter().map(sync_message_json).collect::<Vec<_>>(),
            "cross_project_dependencies": cross_project_dependencies,
        });

        inject_project_context(
            &mut out,
            effective_project_hash.as_deref(),
            &project_knowledge,
            &related_projects,
        );
        inject_seed_identity(
            &mut out,
            &self.db,
            parts.as_ref(),
            resolved_agent_id.as_deref(),
        );

        Ok(CallToolResult::success(vec![Content::text(
            serde_json::to_string_pretty(&out).unwrap_or_default(),
        )]))
    }

    #[tool(
        name = "intelligence_upsert",
        description = "Create or update an intelligence node and optional relations. \
         project_hash is auto-detected from the session workdir if not provided."
    )]
    async fn intelligence_upsert(
        &self,
        Parameters(params): Parameters<IntelligenceUpsertParams>,
        OptionalExtension(parts): OptionalExtension<Parts>,
    ) -> Result<CallToolResult, McpError> {
        self.reject_if_nursery(parts.as_ref())?;
        // Auto-detect project_hash from session workdir if not provided.
        let project_hash = params.node_data.project_hash.or_else(|| {
            let agent_id = self.resolve_sync_agent_id(parts.as_ref()).ok()?;
            let workdir = self.db.get_session_workdir(&agent_id).ok()??;
            Some(crate::domain::project::workdir_hash(&workdir))
        });

        let node = crate::db::intelligence::IntelligenceNodeInput {
            id: params.node_data.id,
            kind: params.node_data.kind,
            title: params.node_data.title,
            body: params.node_data.body,
            metadata: params.node_data.metadata,
            project_hash,
            session_id: params.node_data.session_id,
            relations: params.node_data.relations.map(|relations| {
                relations
                    .into_iter()
                    .map(
                        |relation| crate::db::intelligence::IntelligenceRelationInput {
                            to_node_id: relation.to_node_id,
                            relation: relation.relation,
                            weight: relation.weight,
                        },
                    )
                    .collect()
            }),
        };

        let record = self
            .db
            .upsert_intelligence_node(node)
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;

        Ok(CallToolResult::success(vec![Content::text(
            serde_json::to_string_pretty(&serde_json::json!({
                "node": intelligence_node_json(&record),
            }))
            .unwrap_or_default(),
        )]))
    }

    #[tool(
        name = "intelligence_search",
        description = "Search intelligence nodes by free text and optional kind."
    )]
    async fn intelligence_search(
        &self,
        Parameters(params): Parameters<IntelligenceSearchParams>,
        OptionalExtension(parts): OptionalExtension<Parts>,
    ) -> Result<CallToolResult, McpError> {
        self.reject_if_nursery(parts.as_ref())?;
        let limit = params.limit.unwrap_or(10).min(50);
        let results = self
            .db
            .search_intelligence_nodes(&params.query, params.kind.as_deref(), limit)
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;

        let out = serde_json::json!({
            "query": params.query,
            "kind": params.kind,
            "count": results.len(),
            "results": results.iter().map(intelligence_node_json).collect::<Vec<_>>(),
        });

        Ok(CallToolResult::success(vec![Content::text(
            serde_json::to_string_pretty(&out).unwrap_or_default(),
        )]))
    }

    #[tool(
        name = "intelligence_graph_walk",
        description = "Walk the project-context graph from a node up to the requested depth."
    )]
    async fn intelligence_graph_walk(
        &self,
        Parameters(params): Parameters<IntelligenceGraphWalkParams>,
    ) -> Result<CallToolResult, McpError> {
        let depth = params.depth.unwrap_or(2).min(8);
        let graph = match self.db.walk_intelligence_graph(&params.node_id, depth) {
            Ok(Some(g)) => g,
            Ok(None) => {
                return Ok(error_result(&format!(
                    "Intelligence node '{}' not found.",
                    params.node_id
                )))
            }
            Err(e) => return Err(McpError::internal_error(e.to_string(), None)),
        };

        let out = serde_json::json!({
            "root": intelligence_node_json(&graph.root),
            "nodes": graph.nodes.iter().map(intelligence_node_json).collect::<Vec<_>>(),
            "edges": graph.edges.iter().map(intelligence_edge_json).collect::<Vec<_>>(),
        });

        Ok(CallToolResult::success(vec![Content::text(
            serde_json::to_string_pretty(&out).unwrap_or_default(),
        )]))
    }

    #[tool(
        name = "get_identity",
        description = "Read the agent's own structured identity contract (identity.toml). \
         Returns the full TOML content including name, directives, and traits."
    )]
    async fn get_identity(
        &self,
        OptionalExtension(parts): OptionalExtension<Parts>,
    ) -> Result<CallToolResult, McpError> {
        self.reject_if_nursery(parts.as_ref())?;
        let agent_id = self.resolve_sync_agent_id(parts.as_ref())?;
        let (_, identity) = load_bound_seed_identity(&self.db, parts.as_ref(), &agent_id)
            .map_err(|e| McpError::invalid_params(e, None))?;
        let toml_str = identity
            .to_toml()
            .map_err(|e| McpError::internal_error(e, None))?;
        Ok(CallToolResult::success(vec![Content::text(toml_str)]))
    }

    #[tool(
        name = "evolve_identity",
        description = "Suggest refinements to the agent's directives or traits based on \
         the current session's learnings. The Daemon validates updates against the schema \
         and the 4KB size cap, then overwrites the global identity.toml."
    )]
    async fn evolve_identity(
        &self,
        Parameters(params): Parameters<EvolveIdentityParams>,
        OptionalExtension(parts): OptionalExtension<Parts>,
    ) -> Result<CallToolResult, McpError> {
        self.reject_if_nursery(parts.as_ref())?;
        let agent_id = self.resolve_sync_agent_id(parts.as_ref())?;
        let (seed_id, mut identity) = load_bound_seed_identity(&self.db, parts.as_ref(), &agent_id)
            .map_err(|e| McpError::invalid_params(e, None))?;
        identity
            .evolve(params.new_directives, params.new_traits)
            .map_err(|e| McpError::invalid_params(e, None))?;
        crate::domain::seeds::save_seed(&seed_id, &identity)
            .map_err(|e| McpError::internal_error(e, None))?;
        let _ = self.db.set_state("gamification:identity_evolved", "1");
        Ok(success_result("Identity evolved and saved successfully."))
    }

    #[tool(
        name = "list_seeds",
        description = "List all existing seed identities. Returns seed IDs and their names."
    )]
    async fn list_seeds(&self) -> Result<CallToolResult, McpError> {
        let seed_ids =
            crate::domain::seeds::list_seeds().map_err(|e| McpError::internal_error(e, None))?;
        let mut seeds_info = Vec::new();
        for seed_id in &seed_ids {
            if let Ok(identity) = crate::domain::seeds::load_seed(seed_id) {
                seeds_info.push(serde_json::json!({
                    "id": seed_id,
                    "name": identity.name,
                }));
            }
        }
        Ok(CallToolResult::success(vec![Content::text(
            serde_json::to_string_pretty(&seeds_info).unwrap_or_default(),
        )]))
    }

    #[tool(
        name = "create_seed",
        description = "Create a new seed identity. Validates name uniqueness, structure, and 4KB size cap."
    )]
    async fn create_seed(
        &self,
        Parameters(params): Parameters<CreateSeedParams>,
        OptionalExtension(parts): OptionalExtension<Parts>,
    ) -> Result<CallToolResult, McpError> {
        self.reject_if_nursery(parts.as_ref())?;
        let seed_id = crate::domain::nursery::slugify(&params.name);
        let identity = crate::domain::seeds::SeedIdentity {
            name: params.name,
            created_at: chrono::Utc::now(),
            directives: params.directives.unwrap_or_default(),
            traits: params.traits.unwrap_or_default(),
        };
        crate::domain::seeds::save_seed(&seed_id, &identity)
            .map_err(|e| McpError::invalid_params(e, None))?;
        Ok(CallToolResult::success(vec![Content::text(format!(
            "Seed '{seed_id}' created successfully."
        ))]))
    }

    #[tool(
        name = "remove_seed",
        description = "Remove a seed identity completely by ID."
    )]
    async fn remove_seed(
        &self,
        Parameters(params): Parameters<RemoveSeedParams>,
    ) -> Result<CallToolResult, McpError> {
        crate::domain::seeds::remove_seed(&params.seed_id)
            .map_err(|e| McpError::invalid_params(e, None))?;
        Ok(CallToolResult::success(vec![Content::text(format!(
            "Seed '{}' removed successfully.",
            params.seed_id
        ))]))
    }

    #[tool(
        name = "intelligence_list_projects",
        description = "List all indexed projects for the project picker. \
         Returns project nodes with their hash, name, and description. \
         Supports optional query filtering by name/body."
    )]
    async fn intelligence_list_projects(
        &self,
        Parameters(params): Parameters<IntelligenceListProjectsParams>,
    ) -> Result<CallToolResult, McpError> {
        let limit = params.limit.unwrap_or(20).min(100);
        let projects = self
            .db
            .list_intelligence_projects(params.query.as_deref(), limit)
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;

        let projects_json: Vec<serde_json::Value> = projects
            .iter()
            .map(|p| {
                serde_json::json!({
                    "id": p.id,
                    "title": p.title,
                    "body": p.body,
                    "project_hash": p.project_hash,
                    "metadata": p.metadata.as_ref().and_then(|m| serde_json::from_str::<serde_json::Value>(m).ok()),
                    "updated_at": p.updated_at,
                })
            })
            .collect();

        let out = serde_json::json!({
            "count": projects_json.len(),
            "projects": projects_json,
        });

        Ok(CallToolResult::success(vec![Content::text(
            serde_json::to_string_pretty(&out).unwrap_or_default(),
        )]))
    }

    #[tool(
        name = "intelligence_link_projects",
        description = "Create a relationship between two indexed projects. \
         The relation defaults to 'relates_to' if not specified. \
         Both projects must exist as kind='project' nodes in the intelligence graph."
    )]
    async fn intelligence_link_projects(
        &self,
        Parameters(params): Parameters<IntelligenceLinkProjectsParams>,
    ) -> Result<CallToolResult, McpError> {
        if let Err(e) = validate_non_empty(&params.from_project_hash, "from_project_hash") {
            return Ok(error_result(&e));
        }
        if let Err(e) = validate_non_empty(&params.to_project_hash, "to_project_hash") {
            return Ok(error_result(&e));
        }

        let relation = params.relation.as_deref().unwrap_or("relates_to");
        if let Err(e) = validate_non_empty(relation, "Relation") {
            return Ok(error_result(&e));
        }

        let edge = self
            .db
            .link_projects(
                &params.from_project_hash,
                &params.to_project_hash,
                relation,
                params.weight,
            )
            .map_err(|e| McpError::invalid_params(e.to_string(), None))?;

        let out = serde_json::json!({
            "edge_id": edge.id,
            "from_node_id": edge.from_node_id,
            "to_node_id": edge.to_node_id,
            "relation": edge.relation,
            "weight": edge.weight,
        });

        Ok(CallToolResult::success(vec![Content::text(
            serde_json::to_string_pretty(&out).unwrap_or_default(),
        )]))
    }

    #[tool(
        name = "workflow_create",
        description = "Create a workflow container for a background graph of specs and nodes."
    )]
    async fn workflow_create(
        &self,
        Parameters(params): Parameters<WorkflowCreateParams>,
    ) -> Result<CallToolResult, McpError> {
        let name = params.name.trim();
        if let Err(e) = validate_non_empty(name, "Workflow name") {
            return Ok(error_result(&e));
        }
        let workdir = params.workdir.trim();
        if let Err(e) = validate_non_empty(workdir, "Workflow workdir") {
            return Ok(error_result(&e));
        }
        if let Err(e) = validate_absolute_dir(workdir) {
            return Ok(error_result(&e));
        }

        let workflow = Workflow {
            id: uuid::Uuid::new_v4().to_string(),
            name: name.to_string(),
            description: params.description.filter(|value| !value.trim().is_empty()),
            workdir: workdir.to_string(),
            status: WorkflowStatus::Draft,
            created_at: chrono::Utc::now(),
            started_at: None,
            completed_at: None,
        };

        self.db.insert_workflow(&workflow).map_err(internal_error)?;
        if let Err(error) = self.db.register_project_path(std::path::Path::new(workdir)) {
            tracing::debug!("Could not register workflow project at {workdir}: {error}");
        }

        Ok(build_id_result(&workflow.id, "workflow_id"))
    }

    #[tool(
        name = "workflow_update",
        description = "Update workflow metadata such as name, description, or workdir."
    )]
    async fn workflow_update(
        &self,
        Parameters(params): Parameters<WorkflowUpdateParams>,
    ) -> Result<CallToolResult, McpError> {
        let workflow_id = params.workflow_id.trim();
        if let Err(e) = validate_non_empty(workflow_id, "Workflow ID") {
            return Ok(error_result(&e));
        }
        if let Err(e) = validate_workflow_exists(&self.db, workflow_id) {
            return Ok(error_result(&e));
        }

        let name = match params.name.as_deref().map(str::trim) {
            Some("") => return Ok(error_result("Workflow name must not be empty.")),
            Some(value) => Some(value),
            None => None,
        };
        let description = params.description.as_ref().map(|value| {
            value
                .as_deref()
                .map(str::trim)
                .filter(|description| !description.is_empty())
        });
        let workdir = match params.workdir.as_deref().map(str::trim) {
            Some("") => return Ok(error_result("Workflow workdir must not be empty.")),
            Some(value) => {
                if let Err(e) = validate_absolute_dir(value) {
                    return Ok(error_result(&e));
                }
                Some(value)
            }
            None => None,
        };

        if let Err(e) = validate_at_least_one_bool(
            &[name.is_some(), description.is_some(), workdir.is_some()],
            "workflow_update",
        ) {
            return Ok(error_result(&e));
        }

        self.db
            .update_workflow_details(workflow_id, name, description, workdir)
            .map_err(internal_error)?;

        Ok(build_workflow_update_response(workflow_id))
    }

    #[tool(
        name = "workflow_add_spec",
        description = "Add an ordered spec to an existing workflow."
    )]
    async fn workflow_add_spec(
        &self,
        Parameters(params): Parameters<WorkflowAddSpecParams>,
    ) -> Result<CallToolResult, McpError> {
        let name = params.name.trim();
        if let Err(e) = validate_non_empty(name, "Workflow spec name") {
            return Ok(error_result(&e));
        }
        let workflow_id = params.workflow_id.trim();
        if let Err(e) = validate_workflow_exists(&self.db, workflow_id) {
            return Ok(error_result(&e));
        }

        let existing_specs = self
            .db
            .list_workflow_specs(workflow_id)
            .map_err(internal_error)?;
        if existing_specs
            .iter()
            .any(|spec| spec.position == params.position)
        {
            return Ok(error_result(&format!(
                "Workflow '{workflow_id}' already has a spec at position {}.",
                params.position
            )));
        }
        let Some(description) = params.description.as_deref().map(str::trim) else {
            return Ok(error_result(
                "Workflow spec description is required and must follow the minimum template.",
            ));
        };
        if let Err(error) = validate_spec_description_template(description) {
            return Ok(error_result(&error));
        }

        let spec = WorkflowSpec {
            id: uuid::Uuid::new_v4().to_string(),
            workflow_id: workflow_id.to_string(),
            name: name.to_string(),
            description: Some(description.to_string()),
            position: params.position,
            parallelizable: params.parallelizable,
            status: WorkflowSpecStatus::Pending,
            started_at: None,
            completed_at: None,
        };
        self.db
            .insert_workflow_spec(&spec)
            .map_err(internal_error)?;

        Ok(build_id_result(&spec.id, "spec_id"))
    }

    #[tool(
        name = "workflow_update_spec",
        description = "Update an existing workflow spec."
    )]
    async fn workflow_update_spec(
        &self,
        Parameters(params): Parameters<WorkflowUpdateSpecParams>,
    ) -> Result<CallToolResult, McpError> {
        let spec_id = params.spec_id.trim();
        let spec = match validate_spec_exists(&self.db, spec_id) {
            Ok(spec) => spec,
            Err(e) => return Ok(error_result(&e)),
        };

        let name = match params.name.as_deref().map(str::trim) {
            Some("") => return Ok(error_result("Workflow spec name must not be empty.")),
            Some(value) => Some(value),
            None => None,
        };
        let description = match params.description.as_deref().map(str::trim) {
            Some("") => {
                return Ok(error_result(
                    "Workflow spec description must not be empty and must follow the template.",
                ))
            }
            Some(value) => {
                if let Err(error) = validate_spec_description_template(value) {
                    return Ok(error_result(&error));
                }
                Some(value)
            }
            None => None,
        };

        if let Some(position) = params.position {
            if let Err(e) =
                validate_position_conflict(&self.db, &spec.workflow_id, spec_id, position)
            {
                return Ok(error_result(&e));
            }
        }

        if let Err(e) = validate_at_least_one_bool(
            &[
                name.is_some(),
                description.is_some(),
                params.position.is_some(),
                params.parallelizable.is_some(),
            ],
            "workflow_update_spec",
        ) {
            return Ok(error_result(&e));
        }

        self.db
            .update_workflow_spec_details(
                spec_id,
                name,
                description,
                params.position,
                params.parallelizable,
            )
            .map_err(internal_error)?;

        Ok(build_spec_update_response(spec_id))
    }

    #[tool(
        name = "workflow_add_node",
        description = "Add a graph node to an existing workflow spec."
    )]
    async fn workflow_add_node(
        &self,
        Parameters(params): Parameters<WorkflowAddNodeParams>,
    ) -> Result<CallToolResult, McpError> {
        let name = params.name.trim();
        if let Err(e) = validate_non_empty(name, "Workflow node name") {
            return Ok(error_result(&e));
        }
        let spec_id = params.spec_id.trim();
        if let Err(e) = validate_spec_exists(&self.db, spec_id) {
            return Ok(error_result(&e));
        }

        let kind = match validate_node_kind(params.kind.trim()) {
            Ok(kind) => kind,
            Err(e) => return Ok(error_result(&e)),
        };

        let next_position = self
            .db
            .list_workflow_nodes(spec_id)
            .map_err(internal_error)?
            .last()
            .map(|node| node.position + 1)
            .unwrap_or(1);

        let node = WorkflowNode {
            id: uuid::Uuid::new_v4().to_string(),
            spec_id: spec_id.to_string(),
            name: name.to_string(),
            kind,
            config: params.config,
            position: next_position,
            created_at: chrono::Utc::now(),
        };
        self.db
            .insert_workflow_node(&node)
            .map_err(internal_error)?;

        Ok(build_id_result(&node.id, "node_id"))
    }

    #[tool(
        name = "workflow_update_node",
        description = "Update an existing workflow node."
    )]
    async fn workflow_update_node(
        &self,
        Parameters(params): Parameters<WorkflowUpdateNodeParams>,
    ) -> Result<CallToolResult, McpError> {
        let node_id = params.node_id.trim();
        let node = match validate_node_exists(&self.db, node_id) {
            Ok(node) => node,
            Err(e) => return Ok(error_result(&e)),
        };

        let name = match params.name.as_deref().map(str::trim) {
            Some("") => return Ok(error_result("Workflow node name must not be empty.")),
            Some(value) => Some(value),
            None => None,
        };
        let kind = match params.kind.as_deref().map(str::trim) {
            Some("") => return Ok(error_result("Workflow node kind must not be empty.")),
            Some(value) => match validate_node_kind(value) {
                Ok(kind) => Some(kind),
                Err(e) => return Ok(error_result(&e)),
            },
            None => None,
        };

        if let Some(position) = params.position {
            if let Err(e) =
                validate_node_position_conflict(&self.db, &node.spec_id, node_id, position)
            {
                return Ok(error_result(&e));
            }
        }

        if let Err(e) = validate_at_least_one_bool(
            &[
                name.is_some(),
                kind.is_some(),
                params.config.is_some(),
                params.position.is_some(),
            ],
            "workflow_update_node",
        ) {
            return Ok(error_result(&e));
        }

        self.db
            .update_workflow_node_details(
                node_id,
                name,
                kind,
                params.config.as_ref(),
                params.position,
            )
            .map_err(internal_error)?;

        Ok(build_node_update_response(node_id))
    }

    #[tool(
        name = "workflow_add_edge",
        description = "Connect two nodes inside a workflow spec with a routing condition."
    )]
    async fn workflow_add_edge(
        &self,
        Parameters(params): Parameters<WorkflowAddEdgeParams>,
    ) -> Result<CallToolResult, McpError> {
        let condition = match validate_edge_condition(params.condition.trim()) {
            Ok(c) => c,
            Err(e) => return Ok(error_result(&e)),
        };

        let nodes = self
            .db
            .list_workflow_nodes(&params.spec_id)
            .map_err(internal_error)?;
        if nodes.is_empty() {
            return Ok(error_result(&format!(
                "Spec '{}' not found.",
                params.spec_id
            )));
        }
        let has_from = nodes.iter().any(|node| node.id == params.from_node);
        let has_to = nodes.iter().any(|node| node.id == params.to_node);
        if !has_from || !has_to {
            return Ok(error_result(
                "Both workflow edge endpoints must belong to the provided spec.",
            ));
        }

        let edge = WorkflowEdge {
            id: uuid::Uuid::new_v4().to_string(),
            spec_id: params.spec_id,
            from_node: params.from_node,
            to_node: params.to_node,
            condition,
        };
        self.db
            .insert_workflow_edge(&edge)
            .map_err(internal_error)?;

        Ok(CallToolResult::success(vec![Content::text(
            serde_json::to_string_pretty(&serde_json::json!({ "ok": true })).unwrap_or_default(),
        )]))
    }

    #[tool(
        name = "workflow_update_edge",
        description = "Update the routing condition of an existing workflow edge."
    )]
    async fn workflow_update_edge(
        &self,
        Parameters(params): Parameters<WorkflowUpdateEdgeParams>,
    ) -> Result<CallToolResult, McpError> {
        let edge = match validate_edge_exists(&self.db, params.edge_id.trim()) {
            Ok(e) => e,
            Err(e) => return Ok(error_result(&e)),
        };
        let condition = match validate_edge_condition(params.condition.trim()) {
            Ok(c) => c,
            Err(e) => return Ok(error_result(&e)),
        };

        if edge.condition == condition {
            return Ok(success_result(&format!(
                "Workflow edge '{}' already uses condition '{}'.",
                edge.id,
                condition.as_str()
            )));
        }

        self.db
            .update_workflow_edge_condition(&edge.id, condition)
            .map_err(internal_error)?;

        Ok(success_result(&format!(
            "Workflow edge '{}' updated.",
            edge.id
        )))
    }

    #[tool(
        name = "workflow_get",
        description = "Return a workflow with its ordered specs, nodes, and edges."
    )]
    async fn workflow_get(
        &self,
        Parameters(params): Parameters<WorkflowGetParams>,
    ) -> Result<CallToolResult, McpError> {
        let workflow = match self.db.get_workflow_details(&params.workflow_id) {
            Ok(Some(w)) => w,
            Ok(None) => {
                return Ok(error_result(&format!(
                    "Workflow '{}' not found.",
                    params.workflow_id
                )))
            }
            Err(e) => return Err(internal_error(e.to_string())),
        };

        Ok(CallToolResult::success(vec![Content::text(
            serde_json::to_string_pretty(
                &workflow_details_json(&self.db, &workflow).map_err(internal_error)?,
            )
            .unwrap_or_default(),
        )]))
    }

    #[tool(
        name = "workflow_list",
        description = "List workflows, optionally filtered by workdir."
    )]
    async fn workflow_list(
        &self,
        Parameters(params): Parameters<WorkflowListParams>,
    ) -> Result<CallToolResult, McpError> {
        let workflows = self
            .db
            .list_workflows(params.workdir.as_deref())
            .map_err(internal_error)?;

        let out = build_workflow_list_json(&self.db, &workflows)?;

        Ok(CallToolResult::success(vec![Content::text(
            serde_json::to_string_pretty(&out).unwrap_or_default(),
        )]))
    }

    #[tool(
        name = "workflow_run",
        description = "Run a workflow in the background, spec by spec."
    )]
    async fn workflow_run(
        &self,
        Parameters(params): Parameters<WorkflowRunParams>,
    ) -> Result<CallToolResult, McpError> {
        let workflow = match self.db.get_workflow(&params.workflow_id) {
            Ok(Some(w)) => w,
            Ok(None) => {
                return Ok(error_result(&format!(
                    "Workflow '{}' not found.",
                    params.workflow_id
                )))
            }
            Err(e) => return Err(internal_error(e.to_string())),
        };

        if workflow.status == WorkflowStatus::Running {
            return Ok(error_result(&format!(
                "Workflow '{}' is already running.",
                params.workflow_id
            )));
        }
        if matches!(
            workflow.status,
            WorkflowStatus::Completed | WorkflowStatus::Failed
        ) {
            return Ok(error_result(
                "Completed or failed workflows cannot be resumed yet.",
            ));
        }

        Arc::clone(&self.workflow_engine).start_background(params.workflow_id.clone());
        Ok(success_result(&format!(
            "Workflow '{}' launched in background.",
            params.workflow_id
        )))
    }

    #[tool(
        name = "workflow_pause",
        description = "Pause a running workflow after the current node finishes."
    )]
    async fn workflow_pause(
        &self,
        Parameters(params): Parameters<WorkflowPauseParams>,
    ) -> Result<CallToolResult, McpError> {
        let paused = self
            .workflow_engine
            .request_pause(&params.workflow_id)
            .map_err(internal_error)?;
        if paused {
            Ok(success_result(&format!(
                "Workflow '{}' marked to pause.",
                params.workflow_id
            )))
        } else {
            Ok(error_result(&format!(
                "Workflow '{}' is not running or does not exist.",
                params.workflow_id
            )))
        }
    }

    #[tool(
        name = "workflow_continue",
        description = "Continue a paused workflow by retrying the current node or skipping to the next spec."
    )]
    async fn workflow_continue(
        &self,
        Parameters(params): Parameters<WorkflowContinueParams>,
    ) -> Result<CallToolResult, McpError> {
        let workflow = match self.db.get_workflow(&params.workflow_id) {
            Ok(Some(w)) => w,
            Ok(None) => {
                return Ok(error_result(&format!(
                    "Workflow '{}' not found.",
                    params.workflow_id
                )))
            }
            Err(e) => return Err(internal_error(e.to_string())),
        };
        if workflow.status != WorkflowStatus::Paused {
            return Ok(error_result(&format!(
                "Workflow '{}' is not paused.",
                params.workflow_id
            )));
        }

        match params.action.trim() {
            "retry_current_node" => {}
            "skip_next_spec" => self.handle_skip_next_spec(&params.workflow_id)?,
            _ => {
                return Ok(error_result(
                    "workflow_continue action must be retry_current_node or skip_next_spec.",
                ));
            }
        }

        self.db
            .update_workflow_status(&params.workflow_id, WorkflowStatus::Running, None, None)
            .map_err(internal_error)?;
        Arc::clone(&self.workflow_engine).start_background(params.workflow_id.clone());

        Ok(success_result(&format!(
            "Workflow '{}' resumed with action '{}'.",
            params.workflow_id, params.action
        )))
    }

    #[tool(
        name = "workflow_complete_node",
        description = "Mark the active run for a workflow node as pass or fail and attach its output."
    )]
    async fn workflow_complete_node(
        &self,
        Parameters(params): Parameters<WorkflowCompleteNodeParams>,
    ) -> Result<CallToolResult, McpError> {
        let status = match params.status.trim() {
            "pass" => Some(WorkflowRunStatus::Pass),
            "fail" => Some(WorkflowRunStatus::Fail),
            _ => None,
        };
        let Some(status) = status else {
            return Ok(error_result(
                "workflow_complete_node status must be pass or fail.",
            ));
        };
        let run = match self.db.get_active_workflow_run_for_node(&params.node_id) {
            Ok(Some(r)) => r,
            Ok(None) => {
                return Ok(error_result(&format!(
                    "No active workflow run found for node '{}'.",
                    params.node_id
                )))
            }
            Err(e) => return Err(internal_error(e.to_string())),
        };

        self.db
            .update_workflow_run_result(
                &run.id,
                status,
                Some(&serde_json::json!({
                    "reported_output": params.output,
                    "summary": params.summary,
                })),
                Some(chrono::Utc::now()),
            )
            .map_err(internal_error)?;

        Ok(success_result("Workflow node result recorded."))
    }

    #[tool(
        name = "workflow_report_blocker",
        description = "Pause a workflow because the active node is blocked and needs human intervention."
    )]
    async fn workflow_report_blocker(
        &self,
        Parameters(params): Parameters<WorkflowReportBlockerParams>,
    ) -> Result<CallToolResult, McpError> {
        let run = match self.db.get_active_workflow_run_for_node(&params.node_id) {
            Ok(Some(r)) => r,
            Ok(None) => {
                return Ok(error_result(&format!(
                    "No active workflow run found for node '{}'.",
                    params.node_id
                )))
            }
            Err(e) => return Err(internal_error(e.to_string())),
        };

        self.db
            .update_workflow_run_result(
                &run.id,
                WorkflowRunStatus::Fail,
                Some(&serde_json::json!({
                    "blocker": params.description,
                })),
                Some(chrono::Utc::now()),
            )
            .map_err(internal_error)?;
        self.db
            .update_workflow_status(&run.workflow_id, WorkflowStatus::Paused, None, None)
            .map_err(internal_error)?;
        self.notification_service
            .notify_task_failed(&run.workflow_id, 1, &params.description);

        Ok(success_result(
            "Workflow blocker recorded and workflow paused.",
        ))
    }

    /// Returns the recommended tools and step-by-step protocol for a given action scope.
    /// Use this at session start, before file writes, before test runs, and on session close.
    #[tool(
        name = "get_tools",
        description = "Return the right tools and action protocol for the requested scope. \
        Scopes: session_start (what to do first), file_write (conflict check before modifying), \
        test_run (broadcast before/after running), close_session (wrap up + summary), \
        multi_agent (full sync toolkit). Call this before acting — it tells you exactly what to use."
    )]
    async fn get_tools(
        &self,
        Parameters(params): Parameters<GetToolsParams>,
        OptionalExtension(parts): OptionalExtension<Parts>,
    ) -> Result<CallToolResult, McpError> {
        self.reject_if_nursery(parts.as_ref())?;
        let scope = params.scope.trim().to_lowercase();
        if !matches!(
            scope.as_str(),
            "session_start" | "file_write" | "test_run" | "close_session" | "multi_agent"
        ) {
            return Ok(error_result(
                "Invalid scope. Must be one of: session_start, file_write, test_run, close_session, multi_agent.",
            ));
        }

        let out = if scope == "file_write" {
            let mut result = build_get_tools_response(&scope);
            if let Some(obj) = result.as_object_mut() {
                obj.insert(
                    "path".to_string(),
                    serde_json::json!(params.path.as_deref().unwrap_or("(not specified)")),
                );
            }
            result
        } else {
            build_get_tools_response(&scope)
        };

        Ok(CallToolResult::success(vec![Content::text(
            serde_json::to_string_pretty(&out).unwrap_or_default(),
        )]))
    }

    /// Search registered projects by name or description.
    #[tool(
        name = "project_search",
        description = "Search projects by name/description, returns workdir_hash and metadata."
    )]
    async fn project_search(
        &self,
        Parameters(params): Parameters<ProjectSearchParams>,
    ) -> Result<CallToolResult, McpError> {
        let projects = self
            .db
            .search_projects(&params.query)
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;

        if projects.is_empty() {
            return Ok(success_result("No projects found matching the query."));
        }

        let out: Vec<serde_json::Value> = projects
            .iter()
            .map(|p| {
                serde_json::json!({
                    "workdir_hash": p.hash,
                    "name": p.name,
                    "path": p.path,
                    "description": p.description,
                    "tags": p.tags,
                })
            })
            .collect();

        Ok(CallToolResult::success(vec![Content::text(
            serde_json::to_string_pretty(&out).unwrap_or_default(),
        )]))
    }

    /// Update project metadata (description, tags).
    #[tool(
        name = "project_update",
        description = "Update project metadata. Agents can enrich description and tags discovered during a session."
    )]
    async fn project_update(
        &self,
        Parameters(params): Parameters<ProjectUpdateParams>,
    ) -> Result<CallToolResult, McpError> {
        let updated = self
            .db
            .update_project_meta(
                &params.project_hash,
                params.description.as_deref(),
                params.tags.as_deref(),
            )
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;

        if updated {
            Ok(success_result(&format!(
                "Project '{}' metadata updated.",
                params.project_hash
            )))
        } else {
            Ok(error_result(&format!(
                "Project '{}' not found.",
                params.project_hash
            )))
        }
    }

    /// Full-text search over personal RAG chunks (LanceDB vector search). Rate-limited: 10/min per agent.
    #[tool(
        name = "rag_search",
        description = "Search indexed personal content (markdown and PDF files) using semantic \
        vector search. Default limit: 5. Rate-limited to 10 calls/min per agent. \
        Usage examples: 'search RAG for X', 'find documents about Y', \
        'consult RAG about Z', 'what does the RAG say about W'."
    )]
    async fn rag_search(
        &self,
        Parameters(params): Parameters<RagSearchParams>,
        OptionalExtension(parts): OptionalExtension<Parts>,
    ) -> Result<CallToolResult, McpError> {
        self.reject_if_nursery(parts.as_ref())?;
        if let Some(result) = self.check_rag_rate_limit(&params).await {
            return Ok(result);
        }

        let limit = params.limit.unwrap_or(5).min(20);
        let data_dir_path = data_dir().map_err(internal_error)?;
        let config = crate::domain::canopy_config::CanopyConfig::load(&data_dir_path);

        let model = config.embeddings_model.trim();
        if model.is_empty() {
            return Ok(success_result("No embeddings model configured."));
        }

        let dimensions = match crate::rag::embedding_client::model_dimensions(model) {
            Ok(d) => d,
            Err(e) => return Ok(error_result(&format!("Unknown model dimensions: {e}"))),
        };

        let embedding_client = match crate::rag::embedding_client::client_from_config(&config) {
            Ok(client) => client,
            Err(e) => return Ok(error_result(&format!("Embedding client error: {e}"))),
        };

        let query = params.query.clone();
        let query_vec = tokio::task::spawn_blocking(move || embedding_client.embed(&query))
            .await
            .map_err(|e| internal_error(e.to_string()))?
            .map_err(|e| internal_error(e.to_string()))?;

        let store = crate::rag::vector_store::VectorStore::new(dimensions)
            .await
            .map_err(|e| internal_error(e.to_string()))?;

        let results = store
            .search_similar(&query_vec, limit)
            .await
            .map_err(|e| internal_error(e.to_string()))?;

        if results.is_empty() {
            return Ok(success_result("No results found."));
        }

        let out: Vec<serde_json::Value> = results.iter().map(rag_result_json).collect();

        Ok(CallToolResult::success(vec![Content::text(
            serde_json::to_string_pretty(&out).unwrap_or_default(),
        )]))
    }
}

impl TaskTriggerHandler {
    fn handle_skip_next_spec(&self, workflow_id: &str) -> Result<(), McpError> {
        let current_spec = self
            .db
            .list_workflow_specs(workflow_id)
            .map_err(internal_error)?
            .into_iter()
            .find(|spec| spec.status == WorkflowSpecStatus::Running)
            .ok_or_else(|| {
                McpError::invalid_params(
                    "No running spec found to skip from this paused workflow.",
                    None,
                )
            })?;

        self.db
            .update_workflow_spec_status(
                &current_spec.id,
                WorkflowSpecStatus::Skipped,
                None,
                Some(chrono::Utc::now()),
            )
            .map_err(internal_error)?;
        Ok(())
    }

    async fn restart_updated_watcher(
        &self,
        params: &TaskUpdateParams,
        agent: &Agent,
    ) -> Result<Option<CallToolResult>, McpError> {
        if !agent.is_watch() || !watcher_restart_needed(params) {
            return Ok(None);
        }

        let _ = self.watcher_engine.stop_watcher(&params.id).await;
        if !agent.enabled {
            return Ok(None);
        }

        let Err(e) = self.watcher_engine.start_watcher(agent).await else {
            return Ok(Some(success_result(&format!(
                "Agent '{}' updated successfully. Watcher restarted with new configuration.",
                params.id
            ))));
        };

        Ok(Some(CallToolResult::success(vec![Content::text(format!(
            "Agent '{}' updated but watcher failed to restart: {}. It will be retried on daemon restart.",
            params.id, e
        ))])))
    }

    async fn check_rag_rate_limit(&self, params: &RagSearchParams) -> Option<CallToolResult> {
        let limiter_key = params
            .agent_id
            .clone()
            .unwrap_or_else(|| "global".to_owned());
        let mut limiters = self.rag_limiters.lock().await;
        let limiter = limiters
            .entry(limiter_key)
            .or_insert_with(|| crate::rag::rate_limiter::RateLimiter::new(10));

        limiter
            .check()
            .err()
            .map(|retry_after| error_result(&format!("rate_limited: retry_after={retry_after}s")))
    }
}

fn intelligence_node_json(
    node: &crate::db::intelligence::IntelligenceNodeRecord,
) -> serde_json::Value {
    serde_json::json!({
        "id": node.id,
        "kind": node.kind,
        "title": node.title,
        "body": node.body,
        "metadata": node.metadata,
        "project_hash": node.project_hash,
        "session_id": node.session_id,
        "created_at": node.created_at,
        "updated_at": node.updated_at,
    })
}

fn intelligence_edge_json(
    edge: &crate::db::intelligence::IntelligenceEdgeRecord,
) -> serde_json::Value {
    serde_json::json!({
        "id": edge.id,
        "from_node_id": edge.from_node_id,
        "to_node_id": edge.to_node_id,
        "relation": edge.relation,
        "weight": edge.weight,
        "created_at": edge.created_at,
    })
}

fn sync_message_json(message: &crate::domain::sync::SyncMessage) -> serde_json::Value {
    serde_json::json!({
        "id": message.id,
        "workdir": message.workdir,
        "agent_id": message.agent_id,
        "agent_name": message.agent_name,
        "kind": message.kind.as_str(),
        "message": message.message,
        "payload": message.payload,
        "created_at": message.created_at,
    })
}

fn workflow_details_json(
    db: &Database,
    workflow: &WorkflowDetails,
) -> anyhow::Result<serde_json::Value> {
    let specs = workflow
        .specs
        .iter()
        .map(|spec| workflow_spec_details_json(db, spec, workflow.workflow.status))
        .collect::<anyhow::Result<Vec<_>>>()?;

    Ok(serde_json::json!({
        "id": workflow.workflow.id,
        "name": workflow.workflow.name,
        "description": workflow.workflow.description,
        "workdir": workflow.workflow.workdir,
        "status": workflow.workflow.status.as_str(),
        "created_at": workflow.workflow.created_at.to_rfc3339(),
        "started_at": workflow.workflow.started_at.map(|value| value.to_rfc3339()),
        "completed_at": workflow.workflow.completed_at.map(|value| value.to_rfc3339()),
        "specs": specs,
    }))
}

fn workflow_spec_details_json(
    db: &Database,
    spec: &crate::domain::workflow::WorkflowSpecDetails,
    workflow_status: WorkflowStatus,
) -> anyhow::Result<serde_json::Value> {
    let runs = db.list_workflow_runs_for_spec(&spec.spec.id)?;
    let current_run = runs
        .iter()
        .rev()
        .find(|run| run.status == WorkflowRunStatus::Running)
        .or_else(|| runs.last());
    let blocker = runs.last().and_then(workflow_run_blocker);
    let resume_actions = if workflow_status == WorkflowStatus::Paused
        && spec.spec.status == WorkflowSpecStatus::Running
    {
        vec!["retry_current_node", "skip_next_spec"]
    } else {
        Vec::new()
    };
    let runs = runs.iter().map(workflow_run_json).collect::<Vec<_>>();

    Ok(serde_json::json!({
        "id": spec.spec.id,
        "workflow_id": spec.spec.workflow_id,
        "name": spec.spec.name,
        "description": spec.spec.description,
        "position": spec.spec.position,
        "parallelizable": spec.spec.parallelizable,
        "status": spec.spec.status.as_str(),
        "current_node": current_run.map(|run| run.node_id.clone()),
        "blocked": blocker.is_some(),
        "blocker": blocker,
        "resume_actions": resume_actions,
        "started_at": spec.spec.started_at.map(|value| value.to_rfc3339()),
        "completed_at": spec.spec.completed_at.map(|value| value.to_rfc3339()),
        "nodes": spec.nodes.iter().map(workflow_node_json).collect::<Vec<_>>(),
        "edges": spec.edges.iter().map(workflow_edge_json).collect::<Vec<_>>(),
        "runs": runs,
    }))
}

fn workflow_node_json(node: &WorkflowNode) -> serde_json::Value {
    serde_json::json!({
        "id": node.id,
        "spec_id": node.spec_id,
        "name": node.name,
        "kind": node.kind.as_str(),
        "config": node.config,
        "position": node.position,
        "created_at": node.created_at.to_rfc3339(),
    })
}

fn workflow_edge_json(edge: &WorkflowEdge) -> serde_json::Value {
    serde_json::json!({
        "id": edge.id,
        "spec_id": edge.spec_id,
        "from_node": edge.from_node,
        "to_node": edge.to_node,
        "condition": edge.condition.as_str(),
    })
}

fn workflow_run_json(run: &crate::domain::workflow::WorkflowNodeRun) -> serde_json::Value {
    serde_json::json!({
        "id": run.id,
        "workflow_id": run.workflow_id,
        "spec_id": run.spec_id,
        "node_id": run.node_id,
        "status": run.status.as_str(),
        "input": run.input,
        "output": run.output,
        "started_at": run.started_at.to_rfc3339(),
        "completed_at": run.completed_at.map(|value| value.to_rfc3339()),
        "iteration": run.iteration,
    })
}

fn workflow_run_blocker(run: &crate::domain::workflow::WorkflowNodeRun) -> Option<String> {
    run.output
        .as_ref()
        .and_then(|output| output.get("blocker"))
        .and_then(serde_json::Value::as_str)
        .map(ToOwned::to_owned)
}

fn transport_details(port: u16) -> (&'static str, String) {
    if port > 0 {
        ("Streamable HTTP", port.to_string())
    } else {
        ("stdio", "N/A".to_owned())
    }
}

fn append_temporal_agents_section(status: &mut String, agents: &[Agent]) {
    let temporal = format_temporal_agents(agents);
    if temporal.is_empty() {
        return;
    }

    status.push_str("\n\nTemporal agents:\n");
    status.push_str(&temporal);
}

// ── Context injection helpers ────────────────────────────────────

fn inject_project_context(
    out: &mut serde_json::Value,
    project_hash: Option<&str>,
    project_knowledge: &[IntelligenceNodeRecord],
    related_projects: &[(
        IntelligenceNodeRecord,
        crate::db::intelligence::IntelligenceEdgeRecord,
    )],
) {
    if let Some(ph) = project_hash {
        if let Some(obj) = out.as_object_mut() {
            obj.insert("project_hash".to_string(), serde_json::json!(ph));
        }
    }

    if !project_knowledge.is_empty() {
        let pk_json: Vec<serde_json::Value> = project_knowledge
            .iter()
            .map(|n| {
                serde_json::json!({
                    "id": n.id,
                    "kind": n.kind,
                    "title": n.title,
                    "body": n.body,
                    "project_hash": n.project_hash,
                })
            })
            .collect();
        if let Some(obj) = out.as_object_mut() {
            obj.insert(
                "project_knowledge".to_string(),
                serde_json::Value::Array(pk_json),
            );
            if let Some(summary) = obj.get("summary").and_then(|s| s.as_str()) {
                let new_summary = format!(
                    "{} (+ {} project fact(s)/pattern(s))",
                    summary,
                    project_knowledge.len()
                );
                obj.insert(
                    "summary".to_string(),
                    serde_json::Value::String(new_summary),
                );
            }
        }
    }

    if !related_projects.is_empty() {
        let rp_json: Vec<serde_json::Value> = related_projects
            .iter()
            .map(|(node, edge)| {
                serde_json::json!({
                    "project": {
                        "id": node.id,
                        "title": node.title,
                        "project_hash": node.project_hash,
                    },
                    "relation": edge.relation,
                    "weight": edge.weight,
                })
            })
            .collect();
        if let Some(obj) = out.as_object_mut() {
            obj.insert(
                "related_projects".to_string(),
                serde_json::Value::Array(rp_json),
            );
        }
    }
}

fn inject_seed_identity(
    out: &mut serde_json::Value,
    db: &crate::db::Database,
    parts: Option<&axum::http::request::Parts>,
    agent_id: Option<&str>,
) {
    let Some(agent_id) = agent_id else {
        return;
    };
    if let Ok((_, identity)) = load_bound_seed_identity(db, parts, agent_id) {
        if let Some(obj) = out.as_object_mut() {
            obj.insert(
                "seed_identity".to_string(),
                serde_json::json!({
                    "name": identity.name,
                    "directives": identity.directives.general,
                    "traits": serde_json::json!({
                        "tone": identity.traits.tone,
                        "focus": identity.traits.focus,
                    }),
                    "prompt_injection": identity.prompt_injection(),
                }),
            );
        }
    }
}

#[tool_handler]
impl ServerHandler for TaskTriggerHandler {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new(
                env!("CARGO_PKG_NAME"),
                env!("CARGO_PKG_VERSION"),
            ))
            .with_instructions(
                "MCP server for registering, managing, and executing scheduled and event-driven agents. \
                 Use agent_add to create scheduled agents, agent_watch for file watchers, \
                 agent_run to test immediately, and agent_status for daemon health."
                    .to_string(),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::{header_str, missing_sync_identity_error, MISSING_SYNC_IDENTITY_MESSAGE};
    use crate::shared::sync_identity::CANOPY_AGENT_ID_HEADER;

    #[test]
    fn resolve_sync_agent_id_reads_canopy_header() {
        let request = axum::http::Request::builder()
            .header(CANOPY_AGENT_ID_HEADER, "agent-123")
            .body(())
            .unwrap();
        let (parts, _) = request.into_parts();

        assert_eq!(
            header_str(&parts, CANOPY_AGENT_ID_HEADER),
            Some("agent-123")
        );
    }

    #[test]
    fn resolve_sync_agent_id_requires_canopy_header() {
        let request = axum::http::Request::builder()
            .header("x-canopy-session-name", "cedro")
            .header("x-canopy-workdir", "/tmp/workdir")
            .body(())
            .unwrap();
        let (parts, _) = request.into_parts();

        assert_eq!(header_str(&parts, CANOPY_AGENT_ID_HEADER), None);

        let error = missing_sync_identity_error();
        assert_eq!(error.message, MISSING_SYNC_IDENTITY_MESSAGE);
    }
}
