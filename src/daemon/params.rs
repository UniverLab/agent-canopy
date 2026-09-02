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
    /// Opt this agent into a desktop toast on every *successful* run. When
    /// false (the default), successful scheduled/watch runs stay silent and
    /// only failures notify — so a frequent agent can't spam notifications.
    /// Manual `agent_run` executions always report success regardless.
    pub notify_on_success: Option<bool>,
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

#[derive(Debug, Default, Deserialize, schemars::JsonSchema)]
pub struct TaskModelsParams {
    /// Optional platform/CLI name (e.g. "opencode", "claude") to return only
    /// the models available to that configured platform. Omit for the full
    /// provider list.
    #[serde(default)]
    pub platform: Option<String>,
    /// When true, force a fresh fetch from models.dev instead of serving a
    /// still-fresh local cache — use this to pick up newly published models.
    #[serde(default)]
    pub refresh: Option<bool>,
    /// When true, bypass the per-provider and per-listing caps to show every
    /// model. Defaults to false, which truncates long listings with a notice
    /// naming the provider, how many were shown, and how many exist.
    ///
    /// An explicit flag rather than a bigger implicit cap for platform-scoped
    /// queries: a single platform can still map to a provider with a large
    /// native catalog (e.g. a gateway CLI), so "has a platform filter" isn't
    /// a reliable proxy for "small enough to show uncapped" — an opt-in flag
    /// keeps the worst case bounded and predictable regardless of query shape.
    #[serde(default)]
    pub full: Option<bool>,
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
    #[serde(default)]
    #[schemars(schema_with = "arbitrary_json_value_schema")]
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

/// Schema for a field that accepts any well-formed JSON value. Plain
/// `serde_json::Value` fields otherwise emit an untyped `{}` schema — list
/// every JSON Schema primitive explicitly so a shallow schema reader (and
/// the registered-tool schema regression guard) can see this parameter has
/// a declared type, without narrowing what it actually accepts.
fn arbitrary_json_value_schema(_generator: &mut schemars::SchemaGenerator) -> schemars::Schema {
    schemars::json_schema!({
        "type": ["object", "array", "string", "number", "boolean", "null"],
    })
}

// `#[schemars(inline)]` makes every use site of this type emit its full
// object schema in place instead of a bare `$ref` into `$defs`. Without it,
// `IntelligenceUpsertParams.node_data` advertises only `{"$ref": "..."}`
// with no sibling `type`, which is indistinguishable from an untyped
// parameter to a client that doesn't resolve `$ref` — that client then
// falls back to sending the object as a JSON-encoded string.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[schemars(inline)]
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
    #[serde(default)]
    #[schemars(schema_with = "arbitrary_json_value_schema")]
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
pub struct IntelligenceDeleteNodeParams {
    /// ID of the intelligence node to delete.
    pub node_id: String,
    /// Optional project hash to explicitly scope the deletion, the same way
    /// `intelligence_get_context` allows an explicit override of the
    /// project auto-detected from the session workdir.
    pub project_hash: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct IntelligenceDeleteRelationParams {
    /// ID of the relation (edge) to delete, as returned in the `edges` list
    /// of `intelligence_graph_walk`.
    pub edge_id: i64,
    /// Optional project hash to explicitly scope the deletion, the same way
    /// `intelligence_get_context` allows an explicit override of the
    /// project auto-detected from the session workdir.
    pub project_hash: Option<String>,
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
pub struct ProjectRemapParams {
    /// The project's current workdir_hash (see `project_search` or
    /// `canopy clean`'s orphan report).
    pub project_hash: String,
    /// The project's new absolute path on disk (where the directory was
    /// renamed or moved to).
    pub new_path: String,
    /// Preview which rows would move without changing anything. Default: false.
    pub dry_run: Option<bool>,
    /// Remap even if `new_path` doesn't exist on disk yet. Default: false.
    pub force: Option<bool>,
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
    /// Optional pre-wired target for infrastructure failures (`Break` edges).
    /// When set, every new agent/check/gate node auto-creates a `Break` edge
    /// to this node.
    pub infra_node_id: Option<String>,
}

/// Config for a loop's `on_completed` hook (N2) — an agent-node-style
/// payload (platform/model/timeout_minutes), but with its own `prompt` field
/// rather than a node's `prompt_template` since the hook has no spec/node
/// graph context to template against. See [`crate::loop_engine`]'s
/// `render_completion_hook_prompt` for the placeholders `prompt` supports:
/// `{{loop_name}}`, `{{completed_specs}}`, `{{workdir}}`.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct LoopCompletionHookParams {
    /// CLI platform to run the hook with (e.g. "mimo", "claude").
    pub platform: String,
    /// Optional model override.
    pub model: Option<String>,
    /// Hook prompt template. Supports {{loop_name}}, {{completed_specs}},
    /// {{workdir}}.
    pub prompt: String,
    /// Timeout in minutes for the hook's process (default: 30, same default
    /// as an agent node).
    pub timeout_minutes: Option<u64>,
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
    /// New `on_completed` post-completion hook config, or null to clear it.
    /// Omit to leave the current hook unchanged. Fires at most once per run,
    /// exactly when the loop transitions to `completed` (never on
    /// failed/paused, never retroactively).
    pub on_completed: Option<Option<LoopCompletionHookParams>>,
    /// New pre-wired target for infrastructure failures (`Break` edges), or
    /// null to clear. Omit to leave unchanged.
    pub infra_node_id: Option<Option<String>>,
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
pub struct SpecCreateParams {
    /// Human-readable spec name.
    pub name: String,
    /// Spec description, following the same required template as
    /// `loop_add_spec` (functional/non-functional requirements, objective,
    /// constraints, guidelines, in/out of scope).
    pub description: String,
    /// Optional absolute workdir tag, for backlog filtering only — it does
    /// not drive execution.
    pub workdir: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct SpecListParams {
    /// Filter to specs tagged with this absolute workdir.
    pub workdir: Option<String>,
    /// Filter to specs in this status (pending, running, completed, failed, skipped).
    pub status: Option<String>,
    /// Only return specs not yet assigned to any loop.
    pub unassigned_only: Option<bool>,
    /// Include full spec descriptions in the output. Default: false (compact — id, name, status, workdir, loop_id only).
    pub include_descriptions: Option<bool>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct SpecUpdateParams {
    /// Existing spec ID.
    pub spec_id: String,
    /// New human-readable spec name.
    pub name: Option<String>,
    /// New spec description, following the required template.
    pub description: Option<String>,
    /// New absolute workdir tag, or null to clear it.
    pub workdir: Option<Option<String>>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct SpecSetStatusParams {
    /// Existing spec ID.
    pub spec_id: String,
    /// Target status: `completed`, `skipped`, or `pending` (reopen).
    pub status: String,
    /// Reason for the administrative transition.
    pub reason: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct SpecDeleteParams {
    /// Existing spec ID.
    pub spec_id: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct LoopAddNodeParams {
    /// Existing spec ID. Provide exactly one of `spec_id`/`loop_id`.
    pub spec_id: Option<String>,
    /// Existing loop ID, to add this node to the loop's top-level graph
    /// instead of a spec's graph. Provide exactly one of `spec_id`/`loop_id`.
    pub loop_id: Option<String>,
    /// Human-readable node name.
    pub name: String,
    /// Node kind: agent, check, or gate. Optional when `blueprint` is given —
    /// defaults to the blueprint's own kind.
    pub kind: Option<String>,
    /// Kind-specific configuration object. Alternative to `blueprint`;
    /// provide exactly one of `config`/`blueprint`.
    pub config: Option<serde_json::Map<String, serde_json::Value>>,
    /// Name of an existing blueprint (builtin or custom) to base this node
    /// on, instead of a full `config`. See `blueprint_list`.
    pub blueprint: Option<String>,
    /// Shallow overrides merged onto the blueprint's config template —
    /// override keys win, every other templated key is preserved. Only used
    /// together with `blueprint`.
    pub config_overrides: Option<serde_json::Map<String, serde_json::Value>>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct BlueprintCreateParams {
    /// Unique blueprint name.
    pub name: String,
    /// Node kind: agent, check, or gate.
    pub kind: String,
    /// Config template. `loop_add_node` uses this as the node's config,
    /// optionally shallow-merged with `config_overrides`.
    pub config: serde_json::Map<String, serde_json::Value>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct BlueprintDeleteParams {
    /// Existing custom blueprint name. Builtins can't be deleted.
    pub name: String,
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
    pub config: Option<serde_json::Map<String, serde_json::Value>>,
    /// New visual position within the spec.
    pub position: Option<i64>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct LoopAddEdgeParams {
    /// Existing spec ID. Provide exactly one of `spec_id`/`loop_id`.
    pub spec_id: Option<String>,
    /// Existing loop ID, to add this edge to the loop's top-level graph
    /// instead of a spec's graph. Provide exactly one of `spec_id`/`loop_id`.
    pub loop_id: Option<String>,
    /// Source node ID.
    pub from_node: String,
    /// Destination node ID.
    pub to_node: String,
    /// Routing condition: pass, fail, always, or route.
    pub condition: String,
    /// Route label this edge serves — required when `condition` is
    /// `"route"`, and must name one of `from_node`'s declared routes (see
    /// `loop_add_node`'s router `config.routes`). Ignored otherwise.
    pub route: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct LoopUpdateEdgeParams {
    /// Existing edge ID.
    pub edge_id: String,
    /// New routing condition: pass, fail, always, or route.
    pub condition: String,
    /// Route label this edge serves — required when `condition` is
    /// `"route"`, and must name one of the edge's `from_node`'s declared
    /// routes. Ignored otherwise.
    pub route: Option<String>,
    /// New destination node ID — retargets the edge instead of recreating
    /// it, preserving its `edge_id` and any run history keyed against it.
    /// Must belong to the same spec/loop graph as the edge. Omit to leave
    /// the edge's target unchanged.
    pub to_node: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct LoopDeleteEdgeParams {
    /// Existing edge ID to delete.
    pub edge_id: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct LoopDeleteNodeParams {
    /// Existing node ID to delete. Cascades to every edge naming it as
    /// `from_node` or `to_node`. Rejected if the node is the graph's entry
    /// point.
    pub node_id: String,
}

/// One ensemble member: differs from its siblings by platform/model and,
/// optionally, its own prompt — see `loop_add_ensemble`.
#[derive(Debug, Clone, Deserialize, schemars::JsonSchema)]
pub struct EnsembleMemberParams {
    /// CLI platform for this member (e.g. "claude", "openrouter").
    pub platform: String,
    /// Optional model override for this member.
    pub model: Option<String>,
    /// Optional prompt for this member only, replacing the ensemble's shared
    /// `prompt_template` — same placeholders (e.g. `{{spec_content}}`,
    /// `{{previous_feedback}}`), rendered the same way. Lets a panel review
    /// the same input from several angles (context, security, conventions...)
    /// in one ensemble instead of one prompt across different models. Omit to
    /// use the shared prompt, same as every member before this field existed.
    /// Independent of `platform`/`model`.
    pub prompt_override: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct LoopAddEnsembleParams {
    /// Existing spec ID. Provide exactly one of `spec_id`/`loop_id`.
    pub spec_id: Option<String>,
    /// Existing loop ID, to add this ensemble to the loop's top-level graph
    /// instead of a spec's graph. Provide exactly one of `spec_id`/`loop_id`.
    pub loop_id: Option<String>,
    /// Human-readable ensemble name.
    pub name: String,
    /// Ensemble execution strategy: "parallel" (default), "cascade", or
    /// "round_robin". Parallel runs all members concurrently and counts
    /// passes against min_pass. Cascade tries members in order; the first
    /// usable result wins. Round_robin rotates across members.
    pub kind: Option<String>,
    /// The one shared prompt every member renders — supports the same
    /// placeholders as an agent node's `prompt_template`. Required unless
    /// `blueprint` supplies one.
    pub prompt_template: Option<String>,
    /// 2-8 members: parallel proposer/reviewer variants that differ by
    /// platform/model and, optionally, per-member `prompt_override`. Required
    /// unless `blueprint` supplies them.
    pub members: Option<Vec<EnsembleMemberParams>>,
    /// Name of an existing ensemble blueprint (e.g. "ensemble-proposers") to
    /// source `prompt_template`/`members` from when they're omitted above.
    /// An explicit `prompt_template`/`members` still wins if both are given.
    pub blueprint: Option<String>,
    /// Existing node ID this ensemble is wired from. Every member gets an
    /// incoming edge from this node with `condition`.
    pub from_node: String,
    /// Entry routing condition from `from_node`: pass, fail, or always.
    pub condition: String,
    /// Members required to pass for the quorum to report `pass`. Defaults to
    /// every member.
    pub min_pass: Option<i64>,
    /// Minutes a member may run before the quorum kills it and counts it as
    /// failed. Defaults to `timeout_minutes` (the members' own agent
    /// timeout).
    pub straggler_timeout_minutes: Option<i64>,
    /// Shared agent timeout (minutes) applied to every member. Defaults to
    /// 30, matching an ordinary agent node.
    pub timeout_minutes: Option<i64>,
    /// Existing node ID the quorum routes to on `pass` (e.g. an arbiter node).
    pub on_pass_to: String,
    /// Existing node ID the quorum routes to on `fail`. Omit for a dead end on
    /// fail, same as any other node with no matching outgoing edge.
    pub on_fail_to: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct LoopUpdateEnsembleParams {
    /// Existing ensemble ID.
    pub ensemble_id: String,
    /// New ensemble kind (parallel/cascade/round_robin), or omit to leave unchanged.
    pub kind: Option<String>,
    /// New shared prompt, propagated to every current member.
    pub prompt_template: Option<String>,
    /// Replacement member list (2-8 entries) — added/removed/replaced by
    /// position, always the full list (there is no way to patch a single
    /// member in place). Each entry may set its own `prompt_override`; a
    /// member without one uses the (possibly also-updated) shared
    /// `prompt_template`.
    pub members: Option<Vec<EnsembleMemberParams>>,
    /// New pass threshold.
    pub min_pass: Option<i64>,
    /// New straggler timeout in minutes, or null to fall back to
    /// `timeout_minutes` again.
    pub straggler_timeout_minutes: Option<Option<i64>>,
    /// New shared member agent timeout in minutes.
    pub timeout_minutes: Option<i64>,
    /// New `pass` exit target node ID.
    pub on_pass_to: Option<String>,
    /// New `fail` exit target node ID, or null to clear it (dead end on
    /// fail).
    pub on_fail_to: Option<Option<String>>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct LoopCopyNodeParams {
    /// Node to copy. Only its config is copied — never any runtime state
    /// (runs, iterations, statuses). Cannot be an ensemble member/quorum node
    /// (copy those with loop_copy_ensemble).
    pub source_node_id: String,
    /// Target spec ID for the copy. Provide at most one of spec_id/loop_id;
    /// omit both to copy into the source node's own graph.
    pub spec_id: Option<String>,
    /// Target loop ID (the loop's top-level graph). Cross-loop copy is
    /// allowed. Provide at most one of spec_id/loop_id; omit both to copy into
    /// the source node's own graph.
    pub loop_id: Option<String>,
    /// New node name. Defaults to the source node's name.
    pub name: Option<String>,
    /// Config keys to shallow-merge over the copied config — e.g. swap an
    /// agent's prompt with {"prompt_template": "..."}, or override
    /// platform/model/timeout_minutes/command/value.
    pub config_overrides: Option<serde_json::Map<String, serde_json::Value>>,
    /// Optional incoming wiring: create an edge from this existing node in the
    /// target graph to the copy. Omit to leave the copy without an entry edge.
    pub entry_from_node: Option<String>,
    /// Condition for the entry edge (pass/fail/always). Defaults to always.
    /// Ignored unless entry_from_node is set.
    pub entry_condition: Option<String>,
    /// Optional outgoing edge: wire the copy to this node on a `pass` result.
    pub on_pass_to: Option<String>,
    /// Optional outgoing edge: wire the copy to this node on a `fail` result.
    pub on_fail_to: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct LoopCopyEnsembleParams {
    /// Ensemble to copy. Members, join config, and shared prompt are copied
    /// (config only — never runtime state). Every id in the copy is new.
    pub source_ensemble_id: String,
    /// Target spec ID for the copy. Provide at most one of spec_id/loop_id;
    /// omit both to copy into the source ensemble's own graph.
    pub spec_id: Option<String>,
    /// Target loop ID (the loop's top-level graph). Cross-loop copy is
    /// allowed. Provide at most one of spec_id/loop_id; omit both to copy into
    /// the source ensemble's own graph.
    pub loop_id: Option<String>,
    /// New ensemble name. Defaults to the source name with a " (copy)" suffix.
    pub name: Option<String>,
    /// New shared prompt for every member — e.g. swap a proposer prompt for a
    /// review prompt. Defaults to the source's prompt.
    pub prompt_template: Option<String>,
    /// Replacement member list (2-8). Defaults to the source's members.
    pub members: Option<Vec<EnsembleMemberParams>>,
    /// New pass threshold. Defaults to the source's (clamped to the member
    /// count).
    pub min_pass: Option<i64>,
    /// New shared member agent timeout in minutes. Defaults to the source's.
    pub timeout_minutes: Option<i64>,
    /// New straggler timeout in minutes. Defaults to the source's.
    pub straggler_timeout_minutes: Option<i64>,
    /// Entry wiring override: the node the copy is wired from. Defaults to the
    /// source's entry node — required for a cross-graph copy where that node
    /// doesn't exist in the target.
    pub from_node: Option<String>,
    /// Entry routing condition (pass/fail/always). Defaults to the source's.
    pub condition: Option<String>,
    /// `pass` exit target node ID. Defaults to the source's — required for a
    /// cross-graph copy where the source's target doesn't exist there.
    pub on_pass_to: Option<String>,
    /// `fail` exit target node ID. Defaults to the source's (which may be
    /// none).
    pub on_fail_to: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct QueueCreateParams {
    /// Human-readable queue name.
    pub name: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct QueueAddSpecParams {
    /// Existing queue ID.
    pub queue_id: String,
    /// Existing spec ID to append to the end of the queue.
    pub spec_id: String,
    /// Optional context group. Specs sharing a group in the same queue reuse
    /// one warm harness session: a grouped spec resumes the session captured
    /// by the previous successfully-completed sibling in the group instead of
    /// re-analyzing the repo from cold. Omit for an independent, ungrouped
    /// member.
    #[serde(default)]
    pub group: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct QueueListParams {
    /// Existing queue ID. Omit to list every queue (summary only, no members).
    pub queue_id: Option<String>,
    /// Include full spec descriptions in queue member listings. Default: false (compact — id, name, status, position, group only).
    pub include_descriptions: Option<bool>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct QueueRemoveSpecParams {
    /// Existing queue ID.
    pub queue_id: String,
    /// Spec ID to remove from the queue.
    pub spec_id: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct QueueReorderParams {
    /// Existing queue ID.
    pub queue_id: String,
    /// Full list of spec IDs currently in the queue, in the desired final
    /// order. Must be a total permutation of the queue's current members —
    /// every spec id exactly once.
    pub spec_ids: Vec<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct LoopGetParams {
    /// Loop ID.
    pub loop_id: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct LoopExportParams {
    /// Loop ID to export.
    pub loop_id: String,
    /// Include each agent node's/ensemble member's `platform`/`model` in the
    /// document. Defaults to `false` — a shared design should never pin the
    /// recipient to a harness or model they may not have; use `true` only
    /// when exporting your own loop to restore later on your own machine.
    #[serde(default)]
    pub with_models: Option<bool>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct LoopImportParams {
    /// The exported loop document (the object `loop_export` returns).
    #[schemars(schema_with = "arbitrary_json_value_schema")]
    pub document: serde_json::Value,
    /// Absolute workdir for the new loop.
    pub workdir: String,
    /// Loop name to use instead of the document's own `name`.
    pub name: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct LoopNodeRunsListParams {
    /// Loop ID whose node runs to list.
    pub loop_id: String,
    /// Narrow to one spec's runs.
    pub spec_id: Option<String>,
    /// Narrow to one node's runs.
    pub node_id: Option<String>,
    /// Maximum number of runs to return, most recent first. Defaults to 20,
    /// capped at 200.
    pub limit: Option<u32>,
    /// Number of most-recent runs to skip before returning `limit` more —
    /// page past the default page (e.g. `offset: 20` for the next page after
    /// the default). Defaults to 0.
    pub offset: Option<u32>,
    /// Compact mode: emit only node_name, status, iteration, spec_name, started_at, completed_at. Default: false.
    pub compact: Option<bool>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct LoopNodeRunGetParams {
    /// The node run ID (the `id` field from loop_node_runs_list's results).
    pub run_id: String,
}

#[derive(Debug, Default, Deserialize, schemars::JsonSchema)]
pub struct AgentProbeParams {
    /// Platform to probe (e.g. "claude"). Omit to probe every platform
    /// configured in canopy, each with its own default model.
    #[serde(default)]
    pub platform: Option<String>,
    /// Model to probe for `platform` — validates the exact platform+model
    /// pair a loop node would use, instead of the platform's default.
    /// Requires `platform`; omit both to sweep every configured platform.
    #[serde(default)]
    pub model: Option<String>,
    /// Seconds to wait for a response before reporting a timeout (distinct
    /// from a response that came back but didn't contain the probe token).
    /// Defaults to 30, clamped to [5, 120] — this is a liveness check, not a
    /// capability benchmark.
    #[serde(default)]
    pub timeout_seconds: Option<u64>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct LoopPreflightParams {
    /// Loop ID to preflight.
    pub loop_id: String,
    /// Seconds to wait for each probe's response before reporting a
    /// timeout. Defaults to 30, clamped to [5, 120].
    #[serde(default)]
    pub timeout_seconds: Option<u64>,
}

#[derive(Debug, Default, Deserialize, schemars::JsonSchema)]
pub struct LoopListParams {
    /// Optional absolute workdir filter.
    pub workdir: Option<String>,
    /// Include archived loops in the results. Defaults to `false` — the
    /// same "browsing" view as the TUI's main Loops list, which excludes
    /// archived loops. An archived loop is still reachable directly by id
    /// via `loop_get` regardless of this flag.
    #[serde(default)]
    pub include_archived: Option<bool>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct LoopArchiveParams {
    /// Loop ID to archive.
    pub loop_id: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct LoopRestoreParams {
    /// Loop ID to restore from the archive back to the main list.
    pub loop_id: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct LoopRunParams {
    /// Loop ID.
    pub loop_id: String,
    /// Optional queue ID. When set, the loop runs the queue's pending specs (in
    /// queue order) through the loop's graph instead of its own bound specs.
    /// Queue membership is unaffected — specs stay standalone.
    pub queue_id: Option<String>,
    /// Optional absolute workdir override for this run only. Wins over the
    /// loop's own `workdir`; the loop's `workdir` is left unchanged.
    pub workdir: Option<String>,
    /// Optional free-form text fed to nodes as `{{spec_content}}` when the loop
    /// has no bound specs and no queue. The loop must still have a top-level
    /// graph. Mutually exclusive with `queue_id`.
    pub idea: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct LoopPauseParams {
    /// Loop ID.
    pub loop_id: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct LoopResetParams {
    /// Loop ID.
    pub loop_id: String,
    /// Specific spec IDs to reset to pending, even if already completed.
    /// Omit to reset every spec that isn't already completed, leaving
    /// completed specs untouched so loop_run resumes at the first pending one.
    pub specs: Option<Vec<String>>,
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
    /// Omit (or pass null) to cancel any pending autorun schedule instead of
    /// setting a new one. Mutually exclusive with `quota_reset_message` —
    /// prefer that field for a quota-limit CLI message instead of computing
    /// the timestamp yourself.
    pub at: Option<String>,
    /// Raw CLI quota-limit message, e.g. "You've hit your session limit ·
    /// resets 1pm (America/Bogota)". When set, the engine parses the stated
    /// local reset time and timezone itself and computes the UTC resume
    /// instant deterministically (plus a small safety margin) instead of
    /// requiring you to do that arithmetic — this is the preferred way to
    /// reschedule after a quota failure. Mutually exclusive with `at`.
    pub quota_reset_message: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct LoopScheduleContinueParams {
    /// Loop ID.
    pub loop_id: String,
    /// ISO 8601 timestamp at which a still-paused loop should auto-continue,
    /// e.g. "2026-07-10T09:00:00Z". Fires once, then the schedule is
    /// cleared. Omit (or pass null) to cancel any pending auto-continue
    /// schedule instead of setting a new one.
    pub at: Option<String>,
    /// `loop_continue` action to apply when it fires: retry_current_node or
    /// skip_next_spec. Defaults to retry_current_node when omitted.
    pub action: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct LoopCompleteNodeParams {
    /// The exact node run ID this report belongs to (given to you in the
    /// [REPORTING] section of your prompt). Required so a report can never
    /// be misattributed to a different, newer attempt at the same node.
    pub run_id: String,
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
    /// The exact node run ID this report belongs to (given to you in the
    /// [REPORTING] section of your prompt). Required so a report can never
    /// be misattributed to a different, newer attempt at the same node.
    pub run_id: String,
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

// ── Dynamic skill store tool parameter types ──────────────────────────

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct SkillGetParams {
    /// Skill directory name, as reported by `skill_list` (e.g. "code-review").
    pub name: String,
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

    /// Regression test for bug D: the JSON Schema for `config` used to omit
    /// "type", so MCP clients would serialize it as a JSON-encoded string
    /// instead of an object. Pin that the generated schema now declares
    /// `config` as an object.
    #[test]
    fn loop_add_node_params_schema_declares_config_as_object() {
        let schema = schemars::schema_for!(LoopAddNodeParams);
        let value = serde_json::to_value(&schema).expect("schema should serialize");
        let config_schema = &value["properties"]["config"];
        // `config` is now optional (an alternative to `blueprint`), so the
        // "object" type may appear directly or nested under a $ref/anyOf
        // produced for `Option<..>`.
        let type_str = config_schema["type"].as_str();
        assert!(
            type_str == Some("object") || config_schema.to_string().contains("\"object\""),
            "expected config schema to declare an object type, got {config_schema}"
        );
    }

    #[test]
    fn loop_update_node_params_schema_declares_config_as_object() {
        let schema = schemars::schema_for!(LoopUpdateNodeParams);
        let value = serde_json::to_value(&schema).expect("schema should serialize");
        let config_schema = &value["properties"]["config"];
        // Optional fields are wrapped, so the "object" type may appear either
        // directly or nested under a $ref/anyOf produced for `Option<..>`.
        let type_str = config_schema["type"].as_str();
        assert!(
            type_str == Some("object") || config_schema.to_string().contains("\"object\""),
            "expected config schema to declare an object type, got {config_schema}"
        );
    }

    #[test]
    fn loop_add_node_params_rejects_string_config() {
        let value = serde_json::json!({
            "spec_id": "spec-1",
            "name": "n",
            "kind": "agent",
            "config": "{\"platform\": \"claude\"}"
        });

        let error = serde_json::from_value::<LoopAddNodeParams>(value).unwrap_err();
        assert!(
            error.to_string().contains("invalid type"),
            "expected a type error, got: {error}"
        );
    }

    #[test]
    fn loop_add_node_params_accepts_object_config() {
        let value = serde_json::json!({
            "spec_id": "spec-1",
            "name": "n",
            "kind": "agent",
            "config": { "platform": "claude" }
        });

        let params: LoopAddNodeParams =
            serde_json::from_value(value).expect("object config should deserialize");
        assert_eq!(
            params
                .config
                .as_ref()
                .and_then(|c| c.get("platform"))
                .and_then(|v| v.as_str()),
            Some("claude")
        );
    }

    #[test]
    fn loop_update_node_params_rejects_string_config() {
        let value = serde_json::json!({
            "node_id": "node-1",
            "config": "{\"platform\": \"claude\"}"
        });

        let error = serde_json::from_value::<LoopUpdateNodeParams>(value).unwrap_err();
        assert!(
            error.to_string().contains("invalid type"),
            "expected a type error, got: {error}"
        );
    }
}
