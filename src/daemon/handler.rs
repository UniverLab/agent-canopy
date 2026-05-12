//! MCP Server handler implementing all canopy tools.
//!
//! Uses the `rmcp` SDK's `#[tool_router]` and `#[tool_handler]` macros
//! with `Parameters<T>` for proper MCP protocol compliance.

use std::sync::Arc;

use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::*;
use rmcp::tool;
use rmcp::tool_handler;
use rmcp::tool_router;
use rmcp::ErrorData as McpError;
use rmcp::ServerHandler;
use tokio::sync::Notify;

use crate::application::notification_service::NotificationService;
use crate::application::ports::{AgentRepository, RunRepository};
use crate::daemon::handler_formatting::{
    format_agent_info, format_log_output, format_temporal_agents, format_uptime, internal_error,
    make_log_path, recent_runs_output, resolve_log_path,
};
use crate::daemon::handler_helpers::{
    apply_scalar_updates, apply_trigger_updates, handle_timed_out_run, map_action_result,
    new_agent_base, parse_report_status, prepare_cron_task, prepare_watch_task,
    update_agent_last_run, validate_report_summary, validate_run_transition,
    watcher_restart_needed,
};
use crate::daemon::helpers::{data_dir, error_result, notify_run_result, success_result};
use crate::daemon::params::*;
use crate::db::Database;
use crate::domain::models::{Agent, Trigger};
use crate::domain::sync::{MessageKind, MissionImpact, WorkspaceStatus};
use crate::domain::validation::validate_id;
use crate::executor::Executor;
use crate::rag::rate_limiter::RateLimiter;
use crate::sync_manager::SyncManager;
use crate::watchers::WatcherEngine;

#[derive(Clone)]
pub struct TaskTriggerHandler {
    pub db: Arc<Database>,
    pub executor: Arc<Executor>,
    pub watcher_engine: Arc<WatcherEngine>,
    pub scheduler_notify: Arc<Notify>,
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
    pub fn new(
        db: Arc<Database>,
        executor: Arc<Executor>,
        watcher_engine: Arc<WatcherEngine>,
        scheduler_notify: Arc<Notify>,
        notification_service: Arc<dyn NotificationService>,
        sync_manager: Arc<SyncManager>,
        port: u16,
    ) -> Self {
        Self {
            db,
            executor,
            watcher_engine,
            scheduler_notify,
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
        let models = [
            ("OpenAI", "gpt-4.1"),
            ("OpenAI", "gpt-4o"),
            ("OpenAI", "gpt-4o-mini"),
            ("OpenAI", "o1"),
            ("OpenAI", "o3"),
            ("OpenAI", "o4-mini"),
            ("Anthropic", "claude-sonnet-4-20250514"),
            ("Anthropic", "claude-opus-4-20250514"),
            ("Anthropic", "claude-3-5-sonnet-20241022"),
            ("Anthropic", "claude-3-7-sonnet-20250219"),
            ("Google", "gemini-2.5-pro"),
            ("Google", "gemini-2.5-flash"),
            ("Google", "gemini-2.0-flash"),
            ("Amazon", "nova-pro"),
            ("Amazon", "nova-lite"),
            ("Mistral", "mistral-large-2411"),
            ("Meta", "llama-4-maverick"),
            ("Meta", "llama-4-scout"),
        ];

        let output = models
            .iter()
            .map(|(provider, model)| format!("  {}  ({})", model, provider))
            .collect::<Vec<_>>()
            .join("\n");

        let result = format!(
            "Available models (use the second column value as the model field):\n\
             {}\n\n\
             Note: Model availability depends on the CLI's configured API keys.\n\
             If model is omitted, the CLI uses its own default.",
            output
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
         Use this to announce major work before you start changing things."
    )]
    async fn sync_declare_intent(
        &self,
        Parameters(params): Parameters<SyncDeclareIntentParams>,
    ) -> Result<CallToolResult, McpError> {
        let Some(impact) = MissionImpact::from_str(&params.impact) else {
            return Ok(error_result(
                "Invalid impact. Must be: low, high, breaking.",
            ));
        };

        Ok(map_action_result(
            self.sync_manager
                .declare_intent(
                    &params.workdir,
                    &params.agent_id,
                    &params.agent_name,
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
         status: stable | unstable | testing."
    )]
    async fn sync_report_status(
        &self,
        Parameters(params): Parameters<SyncReportStatusParams>,
    ) -> Result<CallToolResult, McpError> {
        let Some(status) = WorkspaceStatus::from_str(&params.status) else {
            return Ok(error_result(
                "Invalid status. Must be: stable, unstable, testing.",
            ));
        };

        Ok(map_action_result(
            self.sync_manager
                .report_status(
                    &params.workdir,
                    &params.agent_id,
                    &params.agent_name,
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
         kind: info | query | answer."
    )]
    async fn sync_broadcast(
        &self,
        Parameters(params): Parameters<SyncBroadcastParams>,
    ) -> Result<CallToolResult, McpError> {
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
        Ok(map_action_result(
            self.sync_manager
                .broadcast(
                    &params.workdir,
                    &params.agent_id,
                    &params.agent_name,
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
    ) -> Result<CallToolResult, McpError> {
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
        description = "Return PIL context at the requested scope: light or full."
    )]
    async fn intelligence_get_context(
        &self,
        Parameters(params): Parameters<IntelligenceGetContextParams>,
    ) -> Result<CallToolResult, McpError> {
        let scope = params.scope.trim().to_lowercase();
        let (session_limit, knowledge_limit, sync_limit, dependency_limit) = match scope.as_str() {
            "light" => (2, 5, 8, 0),
            "full" => (10, 20, 25, 30),
            _ => return Ok(error_result("Invalid scope. Must be: light or full.")),
        };

        let sessions = self
            .db
            .list_intelligence_nodes(Some("session"), session_limit)
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;
        let knowledge = self
            .db
            .list_intelligence_nodes(None, knowledge_limit)
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;
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
        let summary_prefix = if scope == "light" {
            "Light context"
        } else {
            "Full context"
        };

        let out = serde_json::json!({
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

        Ok(CallToolResult::success(vec![Content::text(
            serde_json::to_string_pretty(&out).unwrap_or_default(),
        )]))
    }

    #[tool(
        name = "intelligence_upsert",
        description = "Create or update an intelligence node and optional relations."
    )]
    async fn intelligence_upsert(
        &self,
        Parameters(params): Parameters<IntelligenceUpsertParams>,
    ) -> Result<CallToolResult, McpError> {
        let node = crate::db::intelligence::IntelligenceNodeInput {
            id: params.node_data.id,
            kind: params.node_data.kind,
            title: params.node_data.title,
            body: params.node_data.body,
            metadata: params.node_data.metadata,
            project_hash: params.node_data.project_hash,
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
    ) -> Result<CallToolResult, McpError> {
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
        description = "Walk the PIL graph from a node up to the requested depth."
    )]
    async fn intelligence_graph_walk(
        &self,
        Parameters(params): Parameters<IntelligenceGraphWalkParams>,
    ) -> Result<CallToolResult, McpError> {
        let depth = params.depth.unwrap_or(2).min(8);
        let Some(graph) = self
            .db
            .walk_intelligence_graph(&params.node_id, depth)
            .map_err(|e| McpError::internal_error(e.to_string(), None))?
        else {
            return Ok(error_result(&format!(
                "Intelligence node '{}' not found.",
                params.node_id
            )));
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
    ) -> Result<CallToolResult, McpError> {
        let scope = params.scope.trim().to_lowercase();
        let out = match scope.as_str() {
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
            "file_write" => {
                let path_hint = params.path.as_deref().unwrap_or("(not specified)");
                serde_json::json!({
                    "scope": "file_write",
                    "risk": "high",
                    "path": path_hint,
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
                })
            }
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
                       metadata={workdir, summary, ...}) — store session summary in PIL.",
                    "2. Call sync_report_status(status=\"stable\", message=\"Mission complete: <summary>\") \
                       — the daemon closes your mission automatically on exit.",
                    "3. Do NOT manually call any close/shutdown tool — daemon handles it."
                ],
                "tools": [
                    "intelligence_upsert — persist session summary as a 'session' node in PIL",
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
                    "intelligence_get_context(scope=\"full\") — deep PIL pull for architecture work",
                    "intelligence_upsert — persist facts, patterns, session summaries",
                    "intelligence_search — find prior art or decisions in PIL"
                ]
            }),
            _ => {
                return Ok(error_result(
                    "Invalid scope. Must be one of: session_start, file_write, test_run, close_session, multi_agent.",
                ))
            }
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
    ) -> Result<CallToolResult, McpError> {
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

        let out: Vec<serde_json::Value> = results
            .iter()
            .map(|r| {
                serde_json::json!({
                    "source": r.file_path,
                    "content": r.content,
                    "distance": r.distance,
                })
            })
            .collect();

        Ok(CallToolResult::success(vec![Content::text(
            serde_json::to_string_pretty(&out).unwrap_or_default(),
        )]))
    }
}

impl TaskTriggerHandler {
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
