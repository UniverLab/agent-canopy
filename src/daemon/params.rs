use rmcp::schemars;
use serde::{Deserialize, Serialize};

// ── Legacy MCP tool parameter types (used by backward-compatible tools) ──

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct TaskAddParams {
    /// Unique identifier. Lowercase, hyphens, underscores.
    pub id: String,
    /// The instruction the CLI will execute headlessly.
    pub prompt: String,
    /// Standard 5-field cron expression: minute hour day month weekday.
    pub schedule: String,
    /// CLI to use. Auto-detects if omitted.
    pub cli: Option<String>,
    /// Optional provider/model string.
    pub model: Option<String>,
    /// Auto-expire after N minutes from registration.
    pub duration_minutes: Option<i64>,
    /// Working directory for the CLI.
    pub working_dir: Option<String>,
    /// Timeout in minutes for execution locking. Default: 15.
    pub timeout_minutes: Option<u32>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct TaskWatchParams {
    /// Unique identifier for the watcher.
    pub id: String,
    /// Absolute path to file or directory to watch.
    pub path: String,
    /// Events to watch: "create", "modify", "delete", "move", or "all".
    pub events: Vec<String>,
    /// Instruction for the CLI on trigger.
    pub prompt: String,
    /// CLI to use. Auto-detects if omitted.
    pub cli: Option<String>,
    /// Optional provider/model string.
    pub model: Option<String>,
    /// Debounce window in seconds (default: 2).
    pub debounce_seconds: Option<u64>,
    /// Watch subdirectories (default: false).
    pub recursive: Option<bool>,
    /// Timeout in minutes for execution locking. Default: 15.
    pub timeout_minutes: Option<u32>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct TaskUpdateParams {
    /// ID of the agent to update.
    pub id: String,
    /// New agent ID to rename this agent to. Must be unique and valid
    /// (lowercase alphanumerics, hyphens, underscores). Updates the agent
    /// row, its log path, and all run references atomically.
    pub new_id: Option<String>,
    /// New prompt/instruction.
    pub prompt: Option<String>,
    /// New CLI platform name.
    pub cli: Option<String>,
    /// New provider/model string, or null to clear.
    pub model: Option<Option<String>>,
    /// New 5-field cron expression (cron agents only), e.g. `"30 * * * *"`
    /// (top of every hour at :30). Standard cron syntax: minute hour day
    /// month weekday, where `*` means "any value". Pass the value as a
    /// normal JSON string — no shell quoting or escaping is needed.
    pub schedule: Option<String>,
    /// New working directory, or null to clear.
    pub working_dir: Option<Option<String>>,
    /// New duration in minutes from now, or null to clear expiration.
    pub duration_minutes: Option<Option<i64>>,
    /// New absolute path to watch (watch agents only).
    pub path: Option<String>,
    /// New event list (watch agents only).
    pub events: Option<Vec<String>>,
    /// New debounce window in seconds (watch agents only).
    pub debounce_seconds: Option<u64>,
    /// Watch subdirectories (watch agents only).
    pub recursive: Option<bool>,
    /// Enable or disable the agent.
    pub enabled: Option<bool>,
}

// ── Shared parameter types ─────────────────────────────────────────────

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct TaskLogsParams {
    /// Agent ID.
    pub id: String,
    /// Last N lines to return (default: 50).
    pub lines: Option<usize>,
    /// ISO 8601 timestamp filter — only return logs after this time.
    pub since: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct IdParam {
    /// Agent ID.
    pub id: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct AgentScheduleEnableParams {
    /// Agent ID.
    pub id: String,
    /// ISO 8601 timestamp at which the agent should be enabled, e.g.
    /// "2026-07-10T09:00:00Z". The agent stays disabled until then.
    pub at: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct TaskReportParams {
    /// The run ID (UUID) provided in the agent execution prompt.
    pub run_id: String,
    /// Execution status: `in_progress`, `success`, or `error`.
    pub status: String,
    /// Brief summary of what happened (required for success/error).
    pub summary: Option<String>,
}

// ── Sync tool parameter types ──────────────────────────────────────────

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct SyncDeclareIntentParams {
    /// Workdir this mission belongs to.
    pub workdir: String,
    /// High-level mission being started.
    pub mission: String,
    /// Impact on the workspace: low | high | breaking.
    pub impact: String,
    /// Optional human-readable details.
    pub description: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct SyncReportStatusParams {
    /// Workdir this status applies to.
    pub workdir: String,
    /// Workspace state: stable | unstable | testing.
    pub status: String,
    /// Optional status details shown to peers.
    pub message: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct SyncBroadcastParams {
    /// Workdir channel to broadcast to.
    pub workdir: String,
    /// Message kind: info | query | answer.
    pub kind: String,
    /// Human-readable message.
    pub message: String,
    /// Optional JSON metadata.
    pub metadata: Option<serde_json::Value>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct SyncGetContextParams {
    /// Workdir to query.
    pub workdir: String,
    /// Number of recent messages to return (default: 10).
    pub limit: Option<usize>,
}

// ── Intelligence tool parameter types ─────────────────────────────

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct IntelligenceGetContextParams {
    /// Context depth: light or full.
    pub scope: String,
    /// Optional project hash to scope facts/patterns to a specific project.
    pub project_hash: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct IntelligenceRelationParams {
    /// Target node ID for the relation.
    pub to_node_id: String,
    /// Relation label, e.g. "depends_on" or "summarizes".
    pub relation: String,
    /// Optional edge weight.
    pub weight: Option<f64>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct IntelligenceNodeParams {
    /// Optional stable node ID. If omitted, a new UUID is generated.
    pub id: Option<String>,
    /// Node kind: project, session, fact, or pattern.
    pub kind: String,
    /// Human-readable title for the node.
    pub title: String,
    /// Main body/content of the node.
    pub body: String,
    /// Optional structured metadata.
    pub metadata: Option<serde_json::Value>,
    /// Optional project hash this node belongs to.
    pub project_hash: Option<String>,
    /// Optional session ID this node belongs to.
    pub session_id: Option<String>,
    /// Optional outgoing relations to other nodes.
    pub relations: Option<Vec<IntelligenceRelationParams>>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct IntelligenceUpsertParams {
    /// Node payload to create or update.
    pub node_data: IntelligenceNodeParams,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct IntelligenceSearchParams {
    /// Free-text search query.
    pub query: String,
    /// Optional kind filter.
    pub kind: Option<String>,
    /// Maximum number of results to return.
    pub limit: Option<usize>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct IntelligenceGraphWalkParams {
    /// Starting node ID.
    pub node_id: String,
    /// Maximum traversal depth.
    pub depth: Option<usize>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct GetToolsParams {
    /// Scope of the action. One of: session_start, file_write, test_run, close_session, multi_agent.
    pub scope: String,
    /// Optional file path hint (used with file_write scope to check conflicts).
    pub path: Option<String>,
}

// ── RAG tool parameter types ───────────────────────────────────────────

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ProjectSearchParams {
    /// Search query matched against project name and description.
    pub query: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ProjectUpdateParams {
    /// Project hash (workdir_hash).
    pub project_hash: String,
    /// New description.
    pub description: Option<String>,
    /// New tags list.
    pub tags: Option<Vec<String>>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct RagSearchParams {
    /// Natural-language search query.
    pub query: String,
    /// Optional caller identity used for per-agent throttling.
    pub agent_id: Option<String>,
    /// Max results (default: 5).
    pub limit: Option<usize>,
}

// ── Loop tool parameter types ────────────────────────────────────────

/// Optional automatic trigger for a loop, mirroring agent triggers. A loop can
/// fire on a cron schedule or a file-system watch instead of only `loop_run`.
/// Also `Serialize` so the TUI's loop form can send it over MCP verbatim.
#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
pub struct LoopTriggerParams {
    /// Trigger kind: "cron", "watch", or "manual". "manual" (the default)
    /// clears any existing trigger, so the loop only runs via loop_run.
    pub kind: String,
    /// 5-field cron expression (required when kind = "cron").
    pub schedule: Option<String>,
    /// Absolute path to a file or directory to watch (required when kind = "watch").
    pub path: Option<String>,
    /// Events to watch: "create", "modify", "delete", "move", or "all" (watch only).
    pub events: Option<Vec<String>>,
    /// Debounce window in seconds (watch only, default: 2).
    pub debounce_seconds: Option<u64>,
    /// Watch subdirectories recursively (watch only, default: false).
    pub recursive: Option<bool>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct LoopCreateParams {
    /// Human-readable loop name.
    pub name: String,
    /// Optional loop description.
    pub description: Option<String>,
    /// Absolute working directory for the loop.
    pub workdir: String,
    /// Optional automatic trigger (cron/watch). Omit for a manual loop.
    pub trigger: Option<LoopTriggerParams>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct LoopUpdateParams {
    /// Loop ID.
    pub loop_id: String,
    /// New human-readable loop name.
    pub name: Option<String>,
    /// New loop description, or null to clear.
    pub description: Option<Option<String>>,
    /// New absolute workdir for the loop.
    pub workdir: Option<String>,
    /// New automatic trigger. Provide kind = "manual" to clear it. Omit to
    /// leave the current trigger unchanged.
    pub trigger: Option<LoopTriggerParams>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct LoopAddSpecParams {
    /// Existing loop ID.
    pub loop_id: String,
    /// Human-readable spec name.
    pub name: String,
    /// Optional spec description.
    pub description: Option<String>,
    /// Execution order within the loop.
    pub position: i64,
    /// Whether the spec is allowed to run in parallel in future engine phases.
    pub parallelizable: bool,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct LoopUpdateSpecParams {
    /// Existing spec ID.
    pub spec_id: String,
    /// New human-readable spec name.
    pub name: Option<String>,
    /// New spec description following the required template.
    pub description: Option<String>,
    /// New execution order within the loop.
    pub position: Option<i64>,
    /// Whether the spec is allowed to run in parallel.
    pub parallelizable: Option<bool>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct LoopAddNodeParams {
    /// Existing spec ID.
    pub spec_id: String,
    /// Human-readable node name.
    pub name: String,
    /// Node kind: agent, check, or gate.
    pub kind: String,
    /// Kind-specific configuration object.
    pub config: serde_json::Value,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct LoopUpdateNodeParams {
    /// Existing node ID.
    pub node_id: String,
    /// New human-readable node name.
    pub name: Option<String>,
    /// New node kind: agent, check, or gate.
    pub kind: Option<String>,
    /// Replacement node config payload.
    pub config: Option<serde_json::Value>,
    /// New visual position within the spec.
    pub position: Option<i64>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct LoopAddEdgeParams {
    /// Existing spec ID.
    pub spec_id: String,
    /// Source node ID.
    pub from_node: String,
    /// Destination node ID.
    pub to_node: String,
    /// Routing condition: pass, fail, or always.
    pub condition: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct LoopUpdateEdgeParams {
    /// Existing edge ID.
    pub edge_id: String,
    /// New routing condition: pass, fail, or always.
    pub condition: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct LoopGetParams {
    /// Loop ID.
    pub loop_id: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct LoopListParams {
    /// Optional absolute workdir filter.
    pub workdir: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct LoopRunParams {
    /// Loop ID.
    pub loop_id: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct LoopPauseParams {
    /// Loop ID.
    pub loop_id: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct LoopContinueParams {
    /// Loop ID.
    pub loop_id: String,
    /// Continue mode: retry_current_node or skip_next_spec.
    pub action: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct LoopScheduleAutorunParams {
    /// Loop ID.
    pub loop_id: String,
    /// ISO 8601 timestamp at which the loop should resume, e.g.
    /// "2026-07-10T09:00:00Z". Fires once, then the schedule is cleared.
    pub at: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct LoopCompleteNodeParams {
    /// Loop node ID.
    pub node_id: String,
    /// pass or fail.
    pub status: String,
    /// Node output payload.
    pub output: String,
    /// Human-readable summary.
    pub summary: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct LoopReportBlockerParams {
    /// Loop node ID.
    pub node_id: String,
    /// Human-readable blocker description.
    pub description: String,
}

// ── Seed Identity tool parameter types ─────────────────────────────

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct EvolveIdentityParams {
    /// New directives list (replaces existing).
    pub new_directives: Option<Vec<String>>,
    /// New traits map (updates existing keys).
    pub new_traits: Option<std::collections::HashMap<String, String>>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct CreateSeedParams {
    /// Unique display name (enforced case-insensitive across all seeds).
    pub name: String,
    /// Behavioral directives injected into prompts.
    pub directives: Option<crate::domain::seeds::SeedDirectives>,
    /// Personality/style traits.
    pub traits: Option<crate::domain::seeds::SeedTraits>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct RemoveSeedParams {
    /// Seed ID to remove.
    pub seed_id: String,
}

// ── Intelligence V2 tool parameter types ─────────────────────────────

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct IntelligenceListProjectsParams {
    /// Optional search query to filter projects by name/body.
    pub query: Option<String>,
    /// Maximum number of results to return (default: 20).
    pub limit: Option<usize>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct IntelligenceLinkProjectsParams {
    /// Source project hash.
    pub from_project_hash: String,
    /// Target project hash.
    pub to_project_hash: String,
    /// Relation label (default: "relates_to").
    pub relation: Option<String>,
    /// Optional edge weight (default: 1.0).
    pub weight: Option<f64>,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Regression test for T22: `agent_update` (MCP tool `agent_update`)
    /// previously appeared to fail on cron schedules containing `*`
    /// (e.g. "30 * * * *") with "JSON Parse error: Unexpected EOF". This
    /// pins that `TaskUpdateParams` deserializes such a schedule string
    /// correctly — `*` is an ordinary JSON string character and requires
    /// no special handling in serde.
    #[test]
    fn task_update_params_deserializes_cron_schedule_with_asterisks() {
        let value = serde_json::json!({
            "id": "x",
            "schedule": "30 * * * *"
        });

        let params: TaskUpdateParams = serde_json::from_value(value).expect("should deserialize");

        assert_eq!(params.id, "x");
        assert_eq!(params.schedule, Some("30 * * * *".to_string()));
    }

    #[test]
    fn task_update_params_deserializes_cron_schedule_with_asterisks_from_str() {
        let raw = r#"{"id":"x","schedule":"*/5 * * * *"}"#;

        let params: TaskUpdateParams = serde_json::from_str(raw).expect("should deserialize");

        assert_eq!(params.schedule, Some("*/5 * * * *".to_string()));
    }
}
