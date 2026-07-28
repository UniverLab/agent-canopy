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
    format_agent_info, format_catalog_models, format_log_output, format_native_models,
    format_platform_models, format_temporal_agents, format_uptime, internal_error, make_log_path,
    recent_runs_output, resolve_log_path,
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
use crate::domain::blueprints::{merge_blueprint_config, validate_blueprint_deletable, Blueprint};
use crate::domain::loops::{
    validate_spec_description_template, Ensemble, EnsembleMember, Loop, LoopDetails, LoopEdge,
    LoopEdgeCondition, LoopNode, LoopNodeKind, LoopNodeRun, LoopResetOutcome, LoopRunStatus,
    LoopSpec, LoopSpecStatus, LoopStatus, SpecAdminStatusOutcome,
};
use crate::domain::models::{Agent, Trigger};
use crate::domain::pools::{Pool, PoolDetails};
use crate::domain::sync::{MessageKind, MissionImpact, WorkspaceStatus};
use crate::domain::validation::validate_id;
use crate::executor::Executor;
use crate::loop_engine::LoopEngine;
use crate::rag::rate_limiter::RateLimiter;
use crate::shared::sync_identity::{
    header_str, CANOPY_AGENT_ID_ENV, CANOPY_AGENT_ID_HEADER, CANOPY_CLIENT_NAME_ENV,
    CANOPY_CLIENT_NAME_HEADER,
};
use crate::sync_manager::SyncManager;
use crate::watchers::WatcherEngine;

const MISSING_SYNC_IDENTITY_MESSAGE: &str =
    "Missing Canopy session identity. Launch via `canopy bridge --id <AGENT_ID>` so requests include Canopy identity headers.";

fn missing_sync_identity_error() -> McpError {
    McpError::invalid_params(MISSING_SYNC_IDENTITY_MESSAGE.to_string(), None)
}

pub(crate) fn validate_non_empty(value: &str, field: &str) -> Result<(), String> {
    if value.trim().is_empty() {
        Err(format!("{field} must not be empty."))
    } else {
        Ok(())
    }
}

pub(crate) fn validate_absolute_dir(path: &str) -> Result<(), String> {
    let p = std::path::Path::new(path);
    if !p.is_absolute() {
        return Err("Loop workdir must be an absolute path.".into());
    }
    if !p.is_dir() {
        return Err("Loop workdir must point to an existing directory.".into());
    }
    Ok(())
}

/// Unlike [`validate_absolute_dir`], a spec's `workdir` tag doesn't require
/// the directory to exist yet — a backlog spec can be authored for a workdir
/// before that project is cloned or a loop targeting it is created. It's
/// only ever used for filtering (`spec_list`), never to drive execution.
fn validate_spec_workdir(path: &str) -> Result<(), String> {
    if !std::path::Path::new(path).is_absolute() {
        return Err("Spec workdir must be an absolute path.".into());
    }
    Ok(())
}

/// Build a loop [`Trigger`] from MCP parameters, reusing the same cron/watch
/// validation as agents. Returns `Ok(None)` for a manual loop (no trigger or
/// `kind = "manual"`), and an `Err(message)` for invalid input.
pub(crate) fn build_loop_trigger(
    params: &Option<LoopTriggerParams>,
) -> Result<Option<Trigger>, String> {
    let Some(params) = params else {
        return Ok(None);
    };

    match params.kind.trim().to_ascii_lowercase().as_str() {
        "" | "manual" => Ok(None),
        "cron" => {
            let schedule = params
                .schedule
                .as_deref()
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .ok_or("A cron trigger requires a 'schedule' expression.")?;
            if !crate::scheduler::validate_cron(schedule) {
                return Err(format!("Invalid cron expression: '{schedule}'."));
            }
            Ok(Some(Trigger::Cron {
                schedule_expr: schedule.to_string(),
            }))
        }
        "watch" => {
            let path = params
                .path
                .as_deref()
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .ok_or("A watch trigger requires a 'path'.")?;
            if !std::path::Path::new(path).is_absolute() {
                return Err("Watch trigger 'path' must be absolute.".into());
            }
            let event_strs = params
                .events
                .clone()
                .filter(|events| !events.is_empty())
                .ok_or("A watch trigger requires at least one event.")?;
            let events = crate::domain::models::WatchEvent::parse_list(&event_strs)?;
            Ok(Some(Trigger::Watch {
                path: path.to_string(),
                events,
                debounce_seconds: params.debounce_seconds.unwrap_or(2),
                recursive: params.recursive.unwrap_or(false),
            }))
        }
        other => Err(format!(
            "Unknown trigger kind '{other}'. Use 'cron', 'watch', or 'manual'."
        )),
    }
}

/// Build a validated [`crate::domain::loops::LoopCompletionHook`] from MCP
/// params — same shape of requirement as an agent node's config
/// (`validate_node_config`'s `LoopNodeKind::Agent` arm): a non-empty
/// `platform`. `prompt` is required outright (unlike a node's
/// `prompt_template`, which defaults) since a completion hook has no
/// spec/node graph context to fall back on.
fn build_loop_completion_hook(
    params: &LoopCompletionHookParams,
) -> Result<crate::domain::loops::LoopCompletionHook, String> {
    let platform = params.platform.trim();
    if platform.is_empty() {
        return Err("on_completed hook 'platform' must not be empty.".to_string());
    }
    let prompt = params.prompt.trim();
    if prompt.is_empty() {
        return Err("on_completed hook 'prompt' must not be empty.".to_string());
    }
    let model = params
        .model
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string);

    Ok(crate::domain::loops::LoopCompletionHook {
        platform: platform.to_string(),
        model,
        prompt: prompt.to_string(),
        timeout_minutes: params.timeout_minutes,
    })
}

const ENSEMBLE_MIN_MEMBERS: usize = 2;
const ENSEMBLE_MAX_MEMBERS: usize = 8;
const DEFAULT_ENSEMBLE_MEMBER_TIMEOUT_MINUTES: i64 = 30;

/// Validate a `loop_add_ensemble`/`loop_update_ensemble` member list: 2-8
/// entries, each with a non-empty `platform`. Returns the normalized
/// `(platform, model)` pairs in the caller's order — the order consolidation
/// and resize diffs rely on.
fn validate_ensemble_members(
    members: &[EnsembleMemberParams],
) -> Result<Vec<(String, Option<String>)>, String> {
    if members.len() < ENSEMBLE_MIN_MEMBERS || members.len() > ENSEMBLE_MAX_MEMBERS {
        return Err(format!(
            "An ensemble must have {ENSEMBLE_MIN_MEMBERS}-{ENSEMBLE_MAX_MEMBERS} members, got {}.",
            members.len()
        ));
    }
    members
        .iter()
        .map(|member| {
            let platform = member.platform.trim();
            if platform.is_empty() {
                return Err("Ensemble member 'platform' must not be empty.".to_string());
            }
            let model = member
                .model
                .as_deref()
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(str::to_string);
            Ok((platform.to_string(), model))
        })
        .collect()
}

/// Build a member agent node's `config` — the shared ensemble prompt plus
/// this member's own platform/model, the same shape `validate_node_config`'s
/// `Agent` arm expects.
fn member_node_config(
    platform: &str,
    model: Option<&str>,
    prompt_template: &str,
    timeout_minutes: i64,
) -> serde_json::Value {
    serde_json::json!({
        "platform": platform,
        "model": model,
        "prompt_template": prompt_template,
        "timeout_minutes": timeout_minutes,
    })
}

/// Validated inputs for assembling one ensemble unit — see
/// [`build_ensemble_unit`]. All wiring targets (`entry_from_node`,
/// `on_pass_to`, `on_fail_to`) must already have been checked to exist in the
/// target graph by the caller.
struct EnsembleUnitSpec<'a> {
    spec_id: Option<String>,
    loop_id: Option<String>,
    name: &'a str,
    prompt_template: &'a str,
    members: &'a [(String, Option<String>)],
    entry_from_node: &'a str,
    entry_condition: LoopEdgeCondition,
    on_pass_to: &'a str,
    on_fail_to: Option<&'a str>,
    min_pass: i64,
    timeout_minutes: i64,
    straggler_timeout_minutes: Option<i64>,
    start_position: i64,
}

/// The concrete graph pieces of one ensemble unit, all with fresh ids: the
/// join node, member nodes, wiring edges (entry fan-out, member→join fan-in,
/// join exit routing), the ensemble row, and its member rows.
#[derive(Debug)]
struct BuiltEnsembleUnit {
    ensemble: Ensemble,
    members: Vec<EnsembleMember>,
    member_nodes: Vec<LoopNode>,
    join_node: LoopNode,
    edges: Vec<LoopEdge>,
}

/// Assemble an ensemble unit from validated inputs. Shared by
/// `loop_add_ensemble` and `loop_copy_ensemble` so the two can never drift in
/// how members, the join, and the wiring are laid out. Purely constructs
/// in-memory values (fresh ids, no runtime state) — persistence is the
/// caller's `insert_ensemble_unit`.
fn build_ensemble_unit(spec: &EnsembleUnitSpec) -> BuiltEnsembleUnit {
    let ensemble_id = uuid::Uuid::new_v4().to_string();
    let join_node_id = uuid::Uuid::new_v4().to_string();
    let now = chrono::Utc::now();
    let mut next_position = spec.start_position;

    let mut member_nodes = Vec::with_capacity(spec.members.len());
    let mut ensemble_members = Vec::with_capacity(spec.members.len());
    let mut edges = Vec::new();

    for (index, (platform, model)) in spec.members.iter().enumerate() {
        let node_id = uuid::Uuid::new_v4().to_string();
        member_nodes.push(LoopNode {
            id: node_id.clone(),
            spec_id: spec.spec_id.clone(),
            loop_id: spec.loop_id.clone(),
            name: format!("{} [{}]", spec.name, index + 1),
            kind: LoopNodeKind::Agent,
            config: member_node_config(
                platform,
                model.as_deref(),
                spec.prompt_template,
                spec.timeout_minutes,
            ),
            position: next_position,
            created_at: now,
        });
        edges.push(LoopEdge {
            id: uuid::Uuid::new_v4().to_string(),
            spec_id: spec.spec_id.clone(),
            loop_id: spec.loop_id.clone(),
            from_node: spec.entry_from_node.to_string(),
            to_node: node_id.clone(),
            condition: spec.entry_condition,
        });
        edges.push(LoopEdge {
            id: uuid::Uuid::new_v4().to_string(),
            spec_id: spec.spec_id.clone(),
            loop_id: spec.loop_id.clone(),
            from_node: node_id.clone(),
            to_node: join_node_id.clone(),
            condition: LoopEdgeCondition::Always,
        });
        ensemble_members.push(EnsembleMember {
            ensemble_id: ensemble_id.clone(),
            node_id,
            position: index as i64,
            platform: platform.clone(),
            model: model.clone(),
        });
        next_position += 1;
    }

    let join_node = LoopNode {
        id: join_node_id.clone(),
        spec_id: spec.spec_id.clone(),
        loop_id: spec.loop_id.clone(),
        name: format!("{} (quorum)", spec.name),
        kind: LoopNodeKind::Join,
        config: serde_json::json!({ "ensemble_id": ensemble_id }),
        position: next_position,
        created_at: now,
    };

    edges.push(LoopEdge {
        id: uuid::Uuid::new_v4().to_string(),
        spec_id: spec.spec_id.clone(),
        loop_id: spec.loop_id.clone(),
        from_node: join_node_id.clone(),
        to_node: spec.on_pass_to.to_string(),
        condition: LoopEdgeCondition::Pass,
    });
    if let Some(on_fail_to) = spec.on_fail_to {
        edges.push(LoopEdge {
            id: uuid::Uuid::new_v4().to_string(),
            spec_id: spec.spec_id.clone(),
            loop_id: spec.loop_id.clone(),
            from_node: join_node_id.clone(),
            to_node: on_fail_to.to_string(),
            condition: LoopEdgeCondition::Fail,
        });
    }

    let ensemble = Ensemble {
        id: ensemble_id,
        spec_id: spec.spec_id.clone(),
        loop_id: spec.loop_id.clone(),
        name: spec.name.to_string(),
        prompt_template: spec.prompt_template.to_string(),
        join_node_id,
        entry_from_node: spec.entry_from_node.to_string(),
        entry_condition: spec.entry_condition,
        min_pass: spec.min_pass,
        straggler_timeout_minutes: spec.straggler_timeout_minutes,
        timeout_minutes: spec.timeout_minutes,
        on_pass_to: spec.on_pass_to.to_string(),
        on_fail_to: spec.on_fail_to.map(str::to_string),
        created_at: now,
    };

    BuiltEnsembleUnit {
        ensemble,
        members: ensemble_members,
        member_nodes,
        join_node,
        edges,
    }
}

fn validate_loop_exists(db: &Database, loop_id: &str) -> Result<(), String> {
    db.get_loop(loop_id)
        .map_err(|e| e.to_string())?
        .is_some()
        .then_some(())
        .ok_or_else(|| format!("Loop '{loop_id}' not found."))
}

fn validate_spec_exists(db: &Database, spec_id: &str) -> Result<LoopSpec, String> {
    db.get_loop_spec(spec_id)
        .map_err(|e| e.to_string())?
        .ok_or_else(|| format!("Spec '{spec_id}' not found."))
}

fn validate_node_exists(db: &Database, node_id: &str) -> Result<LoopNode, String> {
    db.get_loop_node(node_id)
        .map_err(|e| e.to_string())?
        .ok_or_else(|| format!("Loop node '{node_id}' not found."))
}

fn validate_edge_exists(db: &Database, edge_id: &str) -> Result<LoopEdge, String> {
    db.get_loop_edge(edge_id)
        .map_err(|e| e.to_string())?
        .ok_or_else(|| format!("Loop edge '{edge_id}' not found."))
}

/// Where a graph node/edge belongs: a spec's own graph, or a loop's
/// top-level graph (shared across every spec in that loop).
#[derive(Debug)]
enum GraphTarget {
    Spec(String),
    Loop(String),
}

/// Resolve `spec_id`/`loop_id` MCP params into exactly one validated
/// [`GraphTarget`]. Empty/whitespace-only strings are treated as absent, so
/// a client that always sends both fields (one blank) still gets a clean
/// "provide exactly one" error instead of a confusing "not found".
fn resolve_graph_target(
    db: &Database,
    spec_id: Option<&str>,
    loop_id: Option<&str>,
) -> Result<GraphTarget, String> {
    let spec_id = spec_id.map(str::trim).filter(|s| !s.is_empty());
    let loop_id = loop_id.map(str::trim).filter(|s| !s.is_empty());
    match (spec_id, loop_id) {
        (Some(_), Some(_)) => {
            Err("Provide exactly one of spec_id or loop_id, not both.".to_string())
        }
        (None, None) => Err("Provide exactly one of spec_id or loop_id.".to_string()),
        (Some(spec_id), None) => {
            validate_spec_exists(db, spec_id)?;
            Ok(GraphTarget::Spec(spec_id.to_string()))
        }
        (None, Some(loop_id)) => {
            validate_loop_exists(db, loop_id)?;
            Ok(GraphTarget::Loop(loop_id.to_string()))
        }
    }
}

fn validate_position_conflict(
    db: &Database,
    loop_id: Option<&str>,
    exclude_spec_id: &str,
    position: i64,
) -> Result<(), String> {
    // A standalone spec (no loop yet) has no sibling positions to conflict
    // with — position only matters once it's ordered within a loop.
    let Some(loop_id) = loop_id else {
        return Ok(());
    };
    let conflict = db
        .list_loop_specs(loop_id)
        .map_err(|e| e.to_string())?
        .into_iter()
        .any(|s| s.id != exclude_spec_id && s.position == position);
    if conflict {
        Err(format!(
            "Loop '{loop_id}' already has a spec at position {position}."
        ))
    } else {
        Ok(())
    }
}

/// Refuse to delete a spec that's still bound to a loop, with an actionable
/// message pointing at the fix (detach it, or delete the loop instead).
///
/// Note: a spec that's a member of a [`Pool`] can still be deleted — the
/// `pool_members` row cascades away with it (see `pools` table). Pools are
/// just queues over specs that already exist; they don't own them the way a
/// loop owns its bound specs.
fn validate_spec_deletable(spec: &LoopSpec) -> Result<(), String> {
    if let Some(loop_id) = &spec.loop_id {
        return Err(format!(
            "Spec '{}' is bound to loop '{loop_id}'; remove it from the loop (or delete the loop) before deleting the spec.",
            spec.id
        ));
    }
    Ok(())
}

/// Check a node's sibling graph — its spec's nodes, or its loop's top-level
/// graph nodes — for a position conflict. `node` targets exactly one of
/// `spec_id`/`loop_id` (the DB layer enforces it), so exactly one branch runs.
fn validate_node_position_conflict(
    db: &Database,
    node: &LoopNode,
    exclude_node_id: &str,
    position: i64,
) -> Result<(), String> {
    let (siblings, owner) = if let Some(spec_id) = &node.spec_id {
        (
            db.list_loop_nodes(spec_id).map_err(|e| e.to_string())?,
            format!("Spec '{spec_id}'"),
        )
    } else if let Some(loop_id) = &node.loop_id {
        (
            db.list_loop_nodes_for_loop(loop_id)
                .map_err(|e| e.to_string())?,
            format!("Loop '{loop_id}'"),
        )
    } else {
        return Ok(());
    };
    let conflict = siblings
        .iter()
        .any(|n| n.id != exclude_node_id && n.position == position);
    if conflict {
        Err(format!(
            "{owner} already has a node at position {position}."
        ))
    } else {
        Ok(())
    }
}

fn validate_edge_condition(condition: &str) -> Result<LoopEdgeCondition, String> {
    LoopEdgeCondition::from_str(condition.trim())
        .ok_or_else(|| "Loop edge condition must be one of: pass, fail, always.".to_string())
}

fn validate_node_kind(kind: &str) -> Result<LoopNodeKind, String> {
    LoopNodeKind::from_str(kind.trim())
        .ok_or_else(|| "Loop node kind must be one of: agent, check, gate.".to_string())
}

/// `loop_add_node`/`loop_update_node`'s `kind: "join"` guard — a quorum node is
/// engine-managed and only ever created as part of `loop_add_ensemble`'s
/// one-call expansion, never directly.
fn validate_not_join_kind(kind: LoopNodeKind) -> Result<(), String> {
    if kind == LoopNodeKind::Join {
        return Err(
            "Loop node kind 'quorum' is engine-managed; it can only be created via loop_add_ensemble."
                .to_string(),
        );
    }
    Ok(())
}

/// Refuse to edit a node directly with `loop_update_node`/`loop_add_edge` if
/// it belongs to an ensemble (member or quorum) — F1's "individual member
/// overrides are NOT supported in v1": the ensemble is homogeneous by
/// design, so every edit to a member/quorum goes through
/// `loop_update_ensemble`, never a direct node/edge tool.
fn validate_node_not_ensemble_owned(db: &Database, node_id: &str) -> Result<(), String> {
    if let Some(details) = db
        .get_ensemble_by_member_node(node_id)
        .map_err(|e| e.to_string())?
    {
        return Err(format!(
            "Node '{node_id}' is a member of ensemble '{}' ('{}'); edit it via loop_update_ensemble instead.",
            details.ensemble.id, details.ensemble.name
        ));
    }
    if let Some(details) = db
        .get_ensemble_by_join_node(node_id)
        .map_err(|e| e.to_string())?
    {
        return Err(format!(
            "Node '{node_id}' is the quorum of ensemble '{}' ('{}'); edit it via loop_update_ensemble instead.",
            details.ensemble.id, details.ensemble.name
        ));
    }
    Ok(())
}

/// Unlike [`LoopSpecStatus::from_str`] (infallible, defaults to `Pending`
/// for callers that already trust the value came from the DB), a
/// `spec_list` status filter comes from the caller — an unrecognized value
/// should be rejected, not silently reinterpreted as "pending".
fn validate_spec_status(status: &str) -> Result<LoopSpecStatus, String> {
    match status.trim().to_lowercase().as_str() {
        "pending" => Ok(LoopSpecStatus::Pending),
        "running" => Ok(LoopSpecStatus::Running),
        "completed" => Ok(LoopSpecStatus::Completed),
        "failed" => Ok(LoopSpecStatus::Failed),
        "skipped" => Ok(LoopSpecStatus::Skipped),
        _ => Err(
            "Spec status must be one of: pending, running, completed, failed, skipped.".to_string(),
        ),
    }
}

fn validate_spec_set_status_target(status: &str) -> Result<LoopSpecStatus, String> {
    match status.trim().to_lowercase().as_str() {
        "pending" => Ok(LoopSpecStatus::Pending),
        "completed" => Ok(LoopSpecStatus::Completed),
        "skipped" => Ok(LoopSpecStatus::Skipped),
        _ => Err("Spec set_status target must be one of: pending, completed, skipped.".to_string()),
    }
}

fn json_value_kind_name(value: &serde_json::Value) -> &'static str {
    match value {
        serde_json::Value::Null => "null",
        serde_json::Value::Bool(_) => "a boolean",
        serde_json::Value::Number(_) => "a number",
        serde_json::Value::String(_) => "a JSON-encoded string",
        serde_json::Value::Array(_) => "an array",
        serde_json::Value::Object(_) => "an object",
    }
}

/// Validate that a loop node's config is a JSON object with the fields its
/// kind needs at execution time. This exists because a double-encoded config
/// (e.g. `"{\"platform\": \"mimo\"}"` instead of `{"platform": "mimo"}`) used
/// to be accepted at creation time and only surfaced as an engine crash
/// ("Agent node ... is missing a platform/cli") mid-run, long after the node
/// was saved.
fn validate_node_config(kind: LoopNodeKind, config: &serde_json::Value) -> Result<(), String> {
    let Some(map) = config.as_object() else {
        return Err(format!(
            "Loop node config must be a JSON object, not {}. Pass an object (e.g. {{\"platform\": \"claude\"}}) rather than a JSON-encoded string.",
            json_value_kind_name(config)
        ));
    };

    let has_non_empty_str = |field: &str| {
        map.get(field)
            .and_then(serde_json::Value::as_str)
            .is_some_and(|s| !s.trim().is_empty())
    };

    match kind {
        LoopNodeKind::Agent => {
            if !has_non_empty_str("platform") && !has_non_empty_str("cli") {
                return Err(
                    "Loop node config for kind 'agent' must include a non-empty 'platform' (or 'cli') field.".to_string(),
                );
            }
        }
        LoopNodeKind::Check => {
            if !has_non_empty_str("command") {
                return Err(
                    "Loop node config for kind 'check' must include a non-empty 'command' field."
                        .to_string(),
                );
            }
        }
        LoopNodeKind::Gate => {
            let evaluate = map
                .get("evaluate")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("output_contains");
            if evaluate == "output_contains" && !has_non_empty_str("value") {
                return Err(
                    "Loop node config for kind 'gate' must include a non-empty 'value' field when 'evaluate' is 'output_contains'.".to_string(),
                );
            }
        }
        // A join node's config is engine-managed (see `loop_add_ensemble`) —
        // there is nothing for a caller to validate, and callers can never
        // reach this arm anyway since `loop_add_node`/`loop_update_node`
        // refuse `kind: "join"` outright.
        LoopNodeKind::Join => {}
    }

    Ok(())
}

/// Look up a blueprint by name, or an actionable error listing every
/// available blueprint name (builtin and custom) so the caller can pick a
/// valid one without a separate `blueprint_list` round trip.
fn validate_blueprint_exists(db: &Database, name: &str) -> Result<Blueprint, String> {
    match db.get_blueprint_by_name(name).map_err(|e| e.to_string())? {
        Some(blueprint) => Ok(blueprint),
        None => {
            let available = db
                .list_blueprints()
                .map_err(|e| e.to_string())?
                .iter()
                .map(|b| b.name.clone())
                .collect::<Vec<_>>()
                .join(", ");
            Err(format!(
                "Unknown blueprint '{name}'. Available blueprints: {available}."
            ))
        }
    }
}

/// Resolve a `loop_add_node` call's kind/config, either from an explicit
/// `kind`+`config` pair or from a named blueprint (optionally shallow-merged
/// with `config_overrides`). Exactly one of `config`/`blueprint` must be
/// usable — this is the "blueprint as an alternative to a full config"
/// surface described in R7.
fn resolve_node_kind_and_config(
    db: &Database,
    kind: Option<&str>,
    config: Option<serde_json::Map<String, serde_json::Value>>,
    blueprint: Option<&str>,
    config_overrides: Option<serde_json::Map<String, serde_json::Value>>,
) -> Result<(LoopNodeKind, serde_json::Value), String> {
    let blueprint_name = blueprint.map(str::trim).filter(|s| !s.is_empty());
    let explicit_kind = kind.map(str::trim).filter(|s| !s.is_empty());

    match blueprint_name {
        Some(blueprint_name) => {
            let bp = validate_blueprint_exists(db, blueprint_name)?;
            let node_kind = match explicit_kind {
                Some(explicit) => validate_node_kind(explicit)?,
                None => bp.kind,
            };
            let overrides = config_overrides.map(serde_json::Value::Object);
            let merged = merge_blueprint_config(&bp.config, overrides.as_ref());
            Ok((node_kind, merged))
        }
        None => {
            let node_kind = validate_node_kind(
                explicit_kind
                    .ok_or_else(|| "Provide 'kind' when not using a blueprint.".to_string())?,
            )?;
            let config = config
                .map(serde_json::Value::Object)
                .ok_or_else(|| "Provide either 'config' or 'blueprint'.".to_string())?;
            Ok((node_kind, config))
        }
    }
}

fn validate_pool_exists(db: &Database, pool_id: &str) -> Result<Pool, String> {
    db.get_pool(pool_id)
        .map_err(|e| e.to_string())?
        .ok_or_else(|| format!("Queue '{pool_id}' not found."))
}

/// Refuse to start a pool run when one of the pool's specs is already
/// `running` under a different loop. A pool spec's own `loop_id` stays
/// `None` (pool membership never binds it), so ownership is read off the
/// spec's most recent `loop_runs` row instead — the loop that most recently
/// touched the spec is the only one that could have set it `running`.
///
/// This is a start-time check, not a lock: two `loop_run` calls issued in
/// the same instant, before either has run a single node, can still race.
fn validate_pool_not_consumed(
    db: &Database,
    pool_id: &str,
    requesting_loop_id: &str,
) -> Result<(), String> {
    for spec_id in db
        .list_pool_member_spec_ids(pool_id)
        .map_err(|e| e.to_string())?
    {
        let Some(spec) = db.get_loop_spec(&spec_id).map_err(|e| e.to_string())? else {
            continue;
        };
        if spec.status != LoopSpecStatus::Running {
            continue;
        }

        let runs = db
            .list_loop_runs_for_spec(&spec_id)
            .map_err(|e| e.to_string())?;
        let owner_loop_id = runs.last().map(|run| run.loop_id.clone());
        if owner_loop_id.as_deref() != Some(requesting_loop_id) {
            let owner = owner_loop_id.unwrap_or_else(|| "another loop".to_string());
            return Err(format!(
                "Queue '{pool_id}' spec '{spec_id}' is already running under loop '{owner}'; wait for it to finish, or pause that loop, before starting a new run against this queue."
            ));
        }
    }
    Ok(())
}

/// Validate that `spec_ids` is a total permutation of `current`: same
/// length, same set, no duplicates, no unknown ids. This rejects any partial
/// reorder (a subset, or a list with an unrecognized id) so the operation is
/// always "here is the whole new order," never a swap of two entries applied
/// on top of unknown existing state.
fn validate_pool_reorder(current: &[String], spec_ids: &[String]) -> Result<(), String> {
    if spec_ids.len() != current.len() {
        return Err(format!(
            "Reorder must list all {} queue spec(s) exactly once; got {}.",
            current.len(),
            spec_ids.len()
        ));
    }

    let mut seen = std::collections::HashSet::new();
    for id in spec_ids {
        if !seen.insert(id.as_str()) {
            return Err(format!("Reorder lists spec '{id}' more than once."));
        }
        if !current.iter().any(|existing| existing == id) {
            return Err(format!("Queue has no spec '{id}'."));
        }
    }
    Ok(())
}

/// Live pools (R6): refuse to remove a pool member that is currently
/// `running` — the tool-layer half of the same lock `pool_reorder` enforces
/// via [`validate_pool_reorder_locking`]. A spec that isn't a pool member at
/// all, or isn't found, is left for [`Database::remove_pool_member`]'s own
/// "no such spec" error — this only ever blocks a positive `running` match.
fn validate_pool_member_removable(
    db: &Database,
    pool_id: &str,
    spec_id: &str,
) -> Result<(), String> {
    if let Some(spec) = db.get_loop_spec(spec_id).map_err(|e| e.to_string())? {
        if spec.status == LoopSpecStatus::Running {
            return Err(format!(
                "Spec '{spec_id}' is currently running and cannot be removed from queue '{pool_id}'; wait for it to finish, or pause the loop, first."
            ));
        }
    }
    Ok(())
}

/// Live pools (R6): the currently running spec and every already-executed
/// one (`completed`/`failed`/`skipped`) are immutable in the pool's order —
/// only `pending` members may move. Call after [`validate_pool_reorder`] has
/// already confirmed `spec_ids` is a total permutation of `current`: under
/// that guarantee, a locked member "doesn't move" iff it sits at the same
/// index in both slices, since moving it necessarily displaces whatever now
/// occupies its old slot.
fn validate_pool_reorder_locking(
    db: &Database,
    current: &[String],
    spec_ids: &[String],
) -> Result<(), String> {
    for (index, spec_id) in current.iter().enumerate() {
        let status = db
            .get_loop_spec(spec_id)
            .map_err(|e| e.to_string())?
            .map(|spec| spec.status)
            .unwrap_or(LoopSpecStatus::Pending);
        if status == LoopSpecStatus::Pending {
            continue;
        }
        if spec_ids.get(index) != Some(spec_id) {
            return Err(format!(
                "Spec '{spec_id}' is {} and cannot be moved by a reorder; only pending members may be reordered.",
                status.as_str()
            ));
        }
    }
    Ok(())
}

fn pool_details_json(details: &PoolDetails) -> serde_json::Value {
    serde_json::json!({
        "id": details.pool.id,
        "name": details.pool.name,
        "members": details
            .members
            .iter()
            .enumerate()
            .map(|(index, spec)| {
                let mut value = spec_summary_json(spec);
                value["queue_position"] = serde_json::json!(index + 1);
                if let Some(Some(group)) = details.member_groups.get(&spec.id) {
                    value["group"] = serde_json::json!(group);
                }
                value
            })
            .collect::<Vec<_>>(),
    })
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

fn build_loop_update_response(loop_id: &str) -> CallToolResult {
    success_result(&format!("Loop '{loop_id}' updated."))
}

/// Whether `loop_run` accepts relaunching a loop in `status`, factored out as
/// a pure function so the guard (and its wording) can be unit-tested without
/// building a full `TaskTriggerHandler`.
///
/// `loop_run` refuses `failed`/`completed` loops outright — `loop_reset` (or,
/// for a `failed` loop, `loop_schedule_autorun`'s auto-reset-and-resume) is
/// the sanctioned way out, mirroring [`Database::reset_loop`].
fn loop_run_status_guard(loop_id: &str, status: LoopStatus) -> Result<(), String> {
    match status {
        LoopStatus::Running => Err(format!("Loop '{loop_id}' is already running.")),
        LoopStatus::Completed | LoopStatus::Failed => Err(
            "Completed or failed loops cannot be resumed directly; call loop_reset first, \
             then loop_run — or, for a failed loop, loop_schedule_autorun to have it \
             auto-reset and resume once its schedule fires."
                .to_string(),
        ),
        LoopStatus::Draft | LoopStatus::Paused => Ok(()),
    }
}

/// Validate every ensemble (F1) reachable by a `loop_run` call — the loop's
/// own top-level graph, plus the own graph of every spec that could actually
/// run (the loop's bound specs, and a pool's members when `pool_id` is
/// given). A spec with no nodes of its own falls back to the loop-level
/// graph at execution time (see `LoopEngine::run_spec`), so it's skipped
/// here rather than double-validated.
fn validate_loop_ensembles_for_run(
    db: &Database,
    loop_id: &str,
    pool_id: Option<&str>,
) -> Result<(), String> {
    let graph_nodes = db
        .list_loop_nodes_for_loop(loop_id)
        .map_err(|e| e.to_string())?;
    let graph_edges = db
        .list_loop_edges_for_loop(loop_id)
        .map_err(|e| e.to_string())?;
    let graph_ensembles = db
        .list_ensembles_for_loop(loop_id)
        .map_err(|e| e.to_string())?;
    crate::domain::validation::validate_ensembles_in_graph(
        &graph_ensembles,
        &graph_nodes,
        &graph_edges,
    )?;

    let mut spec_ids: Vec<String> = db
        .list_loop_specs(loop_id)
        .map_err(|e| e.to_string())?
        .into_iter()
        .map(|spec| spec.id)
        .collect();
    if let Some(pool_id) = pool_id {
        spec_ids.extend(
            db.list_pool_member_spec_ids(pool_id)
                .map_err(|e| e.to_string())?,
        );
    }

    for spec_id in spec_ids {
        let nodes = db.list_loop_nodes(&spec_id).map_err(|e| e.to_string())?;
        if nodes.is_empty() {
            continue;
        }
        let edges = db.list_loop_edges(&spec_id).map_err(|e| e.to_string())?;
        let ensembles = db
            .list_ensembles_for_spec(&spec_id)
            .map_err(|e| e.to_string())?;
        crate::domain::validation::validate_ensembles_in_graph(&ensembles, &nodes, &edges)?;
    }

    Ok(())
}

/// Core logic for `loop_reset`, factored out of the tool method so it only
/// needs `&Database` (no engine/executor) and can be unit-tested directly.
///
/// Delegates the actual state transition to [`Database::reset_loop`] — the
/// scheduler's auto-reset-and-resume of a `failed` loop on autorun goes
/// through the same function, so this tool and that background path can
/// never drift apart.
fn perform_loop_reset(
    db: &Database,
    loop_id: &str,
    specs: Option<&[String]>,
) -> Result<CallToolResult, McpError> {
    let spec_count = match db.reset_loop(loop_id, specs).map_err(internal_error)? {
        LoopResetOutcome::NotFound => {
            return Ok(error_result(&format!("Loop '{loop_id}' not found.")));
        }
        LoopResetOutcome::Running => {
            return Ok(error_result(&format!(
                "Loop '{loop_id}' is running; call loop_pause first, then loop_reset."
            )));
        }
        LoopResetOutcome::InvalidSpec(id) => {
            return Ok(error_result(&format!(
                "Spec '{id}' does not belong to loop '{loop_id}'."
            )));
        }
        LoopResetOutcome::Reset { spec_count } => spec_count,
    };

    Ok(success_result(&format!(
        "Loop '{loop_id}' reset to pending; {spec_count} spec(s) reset."
    )))
}

/// Resolve the exact node run a `loop_complete_node`/`loop_report_blocker`
/// report belongs to (B12). A node can be retried, so more than one run can
/// exist for the same `node_id` over a spec's lifetime — matching on
/// `node_id` alone (as this used to) means a report arriving late from a
/// killed/superseded attempt (e.g. a timed-out or paused agent that ignores
/// its own termination and calls the tool anyway) would get silently applied
/// to whatever newer run is now active for that node. Requiring the exact
/// `run_id` and verifying it's both still `running` and actually for the
/// claimed `node_id` closes that gap: anything else is rejected outright
/// rather than guessed at.
///
/// Returns `Err(McpError)` only for a genuine DB failure; a stale/malformed
/// report is a normal `Ok(Err(CallToolResult))` — the tool call succeeded at
/// the protocol level, it's just telling the caller its report didn't stick.
fn resolve_reported_run(
    db: &Database,
    run_id: &str,
    node_id: &str,
) -> Result<Result<LoopNodeRun, CallToolResult>, McpError> {
    let run = db.get_loop_run(run_id).map_err(internal_error)?;
    let Some(run) = run else {
        return Ok(Err(error_result(&format!(
            "No loop run found with id '{run_id}'."
        ))));
    };
    if run.node_id != node_id {
        return Ok(Err(error_result(&format!(
            "Run '{run_id}' belongs to node '{}', not '{node_id}'.",
            run.node_id
        ))));
    }
    if run.status != LoopRunStatus::Running {
        return Ok(Err(error_result(&format!(
            "Run '{run_id}' for node '{node_id}' is no longer active (status: {}); this report \
             is stale — the run was already finalized (timed out, paused, reset, superseded by \
             a retry, or already reported) — and was rejected.",
            run.status.as_str()
        ))));
    }
    Ok(Ok(run))
}

fn build_spec_update_response(spec_id: &str) -> CallToolResult {
    success_result(&format!("Loop spec '{spec_id}' updated."))
}

/// Summary JSON for `spec_list` — no run/blocker info, since a standalone
/// (unassigned) spec has never run. Compare [`loop_spec_details_json`],
/// which adds that runtime detail for specs already inside a loop.
fn spec_summary_json(spec: &LoopSpec) -> serde_json::Value {
    let mut obj = serde_json::json!({
        "id": spec.id,
        "loop_id": spec.loop_id,
        "name": spec.name,
        "description": spec.description,
        "workdir": spec.workdir,
        "position": spec.position,
        "parallelizable": spec.parallelizable,
        "status": spec.status.as_str(),
    });
    if let Some(via) = &spec.completed_via {
        obj["completed_via"] = serde_json::json!(via);
    }
    obj
}

fn blueprint_json(blueprint: &Blueprint) -> serde_json::Value {
    serde_json::json!({
        "id": blueprint.id,
        "name": blueprint.name,
        "kind": blueprint.kind.as_str(),
        "config": blueprint.config,
        "builtin": blueprint.builtin,
    })
}

fn build_node_update_response(node_id: &str) -> CallToolResult {
    success_result(&format!("Loop node '{node_id}' updated."))
}

fn build_id_result(id: &str, key: &str) -> CallToolResult {
    let mut map = serde_json::Map::new();
    map.insert(key.to_string(), serde_json::json!(id));
    CallToolResult::success(vec![Content::text(
        serde_json::to_string_pretty(&serde_json::Value::Object(map)).unwrap_or_default(),
    )])
}

/// Return an arbitrary JSON value as a tool result — used by the copy tools
/// (`loop_copy_node`/`loop_copy_ensemble`) whose response carries an explicit
/// old→new id mapping and a wiring report, not just a single id.
fn build_json_result(value: &serde_json::Value) -> CallToolResult {
    CallToolResult::success(vec![Content::text(
        serde_json::to_string_pretty(value).unwrap_or_default(),
    )])
}

/// A graph target split into `(spec_id, loop_id, existing_nodes)` — exactly one
/// of the ids is set. Returned by [`graph_target_parts`].
type GraphParts = (Option<String>, Option<String>, Vec<LoopNode>);

/// Resolve the target graph for a copy: an explicit `spec_id`/`loop_id`, or —
/// when both are absent — the source's own graph. Cross-graph copies are
/// allowed; the source graph is only a default, never a constraint.
fn resolve_copy_target(
    db: &Database,
    spec_id: Option<&str>,
    loop_id: Option<&str>,
    source_spec_id: &Option<String>,
    source_loop_id: &Option<String>,
) -> Result<GraphTarget, String> {
    let spec_id = spec_id.map(str::trim).filter(|s| !s.is_empty());
    let loop_id = loop_id.map(str::trim).filter(|s| !s.is_empty());
    match (spec_id, loop_id) {
        (Some(_), Some(_)) => Err("Provide at most one of spec_id or loop_id.".to_string()),
        (Some(spec_id), None) => {
            validate_spec_exists(db, spec_id)?;
            Ok(GraphTarget::Spec(spec_id.to_string()))
        }
        (None, Some(loop_id)) => {
            validate_loop_exists(db, loop_id)?;
            Ok(GraphTarget::Loop(loop_id.to_string()))
        }
        (None, None) => {
            if let Some(spec_id) = source_spec_id {
                Ok(GraphTarget::Spec(spec_id.clone()))
            } else if let Some(loop_id) = source_loop_id {
                Ok(GraphTarget::Loop(loop_id.clone()))
            } else {
                Err(
                    "Source belongs to no graph and no target spec_id/loop_id was given."
                        .to_string(),
                )
            }
        }
    }
}

/// Split a [`GraphTarget`] into `(spec_id, loop_id)` (exactly one set) plus the
/// target graph's current nodes — used by the copy planners to place the copy
/// at the next free position and to validate wiring targets against that graph.
fn graph_target_parts(db: &Database, target: &GraphTarget) -> Result<GraphParts, String> {
    match target {
        GraphTarget::Spec(spec_id) => {
            let nodes = db.list_loop_nodes(spec_id).map_err(|e| e.to_string())?;
            Ok((Some(spec_id.clone()), None, nodes))
        }
        GraphTarget::Loop(loop_id) => {
            let nodes = db
                .list_loop_nodes_for_loop(loop_id)
                .map_err(|e| e.to_string())?;
            Ok((None, Some(loop_id.clone()), nodes))
        }
    }
}

/// A planned single-node copy: the new node, its optional wiring edges, and a
/// report of the wiring actually applied. Pure of side effects — the caller
/// persists it with [`Database::insert_node_with_edges`]. Split out from
/// `loop_copy_node` so the copy/override/wiring logic is unit-testable with
/// just a `Database`.
#[derive(Debug)]
struct NodeCopyPlan {
    source_id: String,
    node: LoopNode,
    edges: Vec<LoopEdge>,
    wiring: serde_json::Map<String, serde_json::Value>,
}

fn plan_node_copy(db: &Database, params: &LoopCopyNodeParams) -> Result<NodeCopyPlan, String> {
    let source_id = params.source_node_id.trim();
    let source = validate_node_exists(db, source_id)?;
    if source.kind == LoopNodeKind::Join {
        return Err(
            "Cannot copy a quorum node directly; copy its ensemble with loop_copy_ensemble."
                .to_string(),
        );
    }
    if let Err(e) = validate_node_not_ensemble_owned(db, source_id) {
        return Err(format!(
            "Cannot copy an ensemble-owned node directly; copy the whole ensemble with loop_copy_ensemble instead. {e}"
        ));
    }

    let target = resolve_copy_target(
        db,
        params.spec_id.as_deref(),
        params.loop_id.as_deref(),
        &source.spec_id,
        &source.loop_id,
    )?;
    let (spec_id, loop_id, existing_nodes) = graph_target_parts(db, &target)?;

    // Copy the source config, then shallow-merge any override keys over it.
    let mut config = source.config.clone();
    if let Some(overrides) = &params.config_overrides {
        match config.as_object_mut() {
            Some(map) => {
                for (key, value) in overrides {
                    map.insert(key.clone(), value.clone());
                }
            }
            None => config = serde_json::Value::Object(overrides.clone()),
        }
    }
    validate_node_config(source.kind, &config)?;

    let name = params
        .name
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or(source.name.as_str())
        .to_string();

    let new_id = uuid::Uuid::new_v4().to_string();
    let next_position = existing_nodes
        .last()
        .map(|node| node.position + 1)
        .unwrap_or(1);
    let node = LoopNode {
        id: new_id.clone(),
        spec_id: spec_id.clone(),
        loop_id: loop_id.clone(),
        name,
        kind: source.kind,
        config,
        position: next_position,
        created_at: chrono::Utc::now(),
    };

    let node_exists = |id: &str| existing_nodes.iter().any(|n| n.id == id);
    let mut edges = Vec::new();
    let mut wiring = serde_json::Map::new();

    if let Some(from) = params
        .entry_from_node
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        if !node_exists(from) {
            return Err(format!(
                "entry_from_node '{from}' not found in the target graph."
            ));
        }
        let condition = match params.entry_condition.as_deref() {
            Some(value) => validate_edge_condition(value)?,
            None => LoopEdgeCondition::Always,
        };
        edges.push(LoopEdge {
            id: uuid::Uuid::new_v4().to_string(),
            spec_id: spec_id.clone(),
            loop_id: loop_id.clone(),
            from_node: from.to_string(),
            to_node: new_id.clone(),
            condition,
        });
        wiring.insert("entry_from_node".into(), serde_json::json!(from));
        wiring.insert(
            "entry_condition".into(),
            serde_json::json!(condition.as_str()),
        );
    }
    for (field, target_node, cond) in [
        ("on_pass_to", &params.on_pass_to, LoopEdgeCondition::Pass),
        ("on_fail_to", &params.on_fail_to, LoopEdgeCondition::Fail),
    ] {
        if let Some(to) = target_node
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            if !node_exists(to) {
                return Err(format!("{field} '{to}' not found in the target graph."));
            }
            edges.push(LoopEdge {
                id: uuid::Uuid::new_v4().to_string(),
                spec_id: spec_id.clone(),
                loop_id: loop_id.clone(),
                from_node: new_id.clone(),
                to_node: to.to_string(),
                condition: cond,
            });
            wiring.insert(field.to_string(), serde_json::json!(to));
        }
    }

    Ok(NodeCopyPlan {
        source_id: source_id.to_string(),
        node,
        edges,
        wiring,
    })
}

/// The `note` line a `loop_copy_node` response carries. An unwired copy is a
/// valid outcome, but callers must never assume edges exist — so the note says
/// so explicitly and points at how to wire it.
fn node_copy_note(source_id: &str, new_id: &str, wired: bool) -> String {
    if wired {
        format!("Copied node config from '{source_id}' into a new node '{new_id}'.")
    } else {
        format!(
            "Unwired copy of '{source_id}': the new node '{new_id}' has NO incoming or \
             outgoing edges yet. Wire it with loop_add_edge, or pass \
             entry_from_node/on_pass_to/on_fail_to."
        )
    }
}

/// A planned ensemble copy: the fully-built new unit plus the metadata needed
/// to report the old→new id mapping and the applied wiring. Pure of side
/// effects — the caller persists `built` with [`Database::insert_ensemble_unit`].
#[derive(Debug)]
struct EnsembleCopyPlan {
    source_ensemble_id: String,
    source_join_node_id: String,
    /// Source member node ids in position order — paired with the new member
    /// nodes for the mapping when members were not replaced.
    source_member_node_ids: Vec<String>,
    members_replaced: bool,
    built: BuiltEnsembleUnit,
    entry_from_node: String,
    entry_condition: LoopEdgeCondition,
    on_pass_to: String,
    on_fail_to: Option<String>,
}

fn plan_ensemble_copy(
    db: &Database,
    params: &LoopCopyEnsembleParams,
) -> Result<EnsembleCopyPlan, String> {
    let source_id = params.source_ensemble_id.trim();
    let details = db
        .get_ensemble_details(source_id)
        .map_err(|e| e.to_string())?
        .ok_or_else(|| format!("Ensemble '{source_id}' not found."))?;
    let source = &details.ensemble;

    let target = resolve_copy_target(
        db,
        params.spec_id.as_deref(),
        params.loop_id.as_deref(),
        &source.spec_id,
        &source.loop_id,
    )?;
    let (spec_id, loop_id, existing_nodes) = graph_target_parts(db, &target)?;
    let node_exists = |id: &str| existing_nodes.iter().any(|n| n.id == id);

    let name = params
        .name
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| format!("{} (copy)", source.name));
    let prompt_template = params
        .prompt_template
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or(source.prompt_template.as_str())
        .to_string();

    let members_replaced = params.members.is_some();
    let members: Vec<(String, Option<String>)> = match &params.members {
        Some(explicit) => validate_ensemble_members(explicit)?,
        None => details
            .members
            .iter()
            .map(|m| (m.platform.clone(), m.model.clone()))
            .collect(),
    };

    let entry_from_node = params
        .from_node
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or(source.entry_from_node.as_str())
        .to_string();
    let entry_condition = match params
        .condition
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        Some(value) => validate_edge_condition(value)?,
        None => source.entry_condition,
    };
    let on_pass_to = params
        .on_pass_to
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or(source.on_pass_to.as_str())
        .to_string();
    let on_fail_to = match params
        .on_fail_to
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        Some(value) => Some(value.to_string()),
        None => source.on_fail_to.clone(),
    };

    // Wiring targets must exist in the TARGET graph (respecting the target
    // loop's own validation for a cross-loop copy) and must not be
    // ensemble-owned — the same rules loop_add_ensemble enforces.
    for (label, id) in [("from_node", &entry_from_node), ("on_pass_to", &on_pass_to)] {
        if !node_exists(id) {
            return Err(format!(
                "Wiring node '{id}' ({label}) not found in the target graph. Pass {label} \
                 that exists there (required for a cross-graph copy)."
            ));
        }
        if let Err(e) = validate_node_not_ensemble_owned(db, id) {
            return Err(format!(
                "Cannot wire an ensemble to an ensemble-owned node ({label}): {e}"
            ));
        }
    }
    if let Some(fail) = &on_fail_to {
        if !node_exists(fail) {
            return Err(format!(
                "Wiring node '{fail}' (on_fail_to) not found in the target graph."
            ));
        }
        if let Err(e) = validate_node_not_ensemble_owned(db, fail) {
            return Err(format!(
                "Cannot wire an ensemble to an ensemble-owned node (on_fail_to): {e}"
            ));
        }
    }

    let min_pass = params.min_pass.unwrap_or(source.min_pass);
    if min_pass < 1 || min_pass > members.len() as i64 {
        return Err(format!(
            "min_pass must be between 1 and {} (the member count), got {min_pass}.",
            members.len()
        ));
    }
    let timeout_minutes = params.timeout_minutes.unwrap_or(source.timeout_minutes);
    if timeout_minutes < 0 {
        return Err("timeout_minutes must not be negative.".to_string());
    }
    let straggler_timeout_minutes = params
        .straggler_timeout_minutes
        .or(source.straggler_timeout_minutes);
    if let Some(straggler) = straggler_timeout_minutes {
        if straggler < 0 {
            return Err("straggler_timeout_minutes must not be negative.".to_string());
        }
    }

    let start_position = existing_nodes
        .last()
        .map(|node| node.position + 1)
        .unwrap_or(1);
    let built = build_ensemble_unit(&EnsembleUnitSpec {
        spec_id,
        loop_id,
        name: &name,
        prompt_template: &prompt_template,
        members: &members,
        entry_from_node: &entry_from_node,
        entry_condition,
        on_pass_to: &on_pass_to,
        on_fail_to: on_fail_to.as_deref(),
        min_pass,
        timeout_minutes,
        straggler_timeout_minutes,
        start_position,
    });

    Ok(EnsembleCopyPlan {
        source_ensemble_id: source.id.clone(),
        source_join_node_id: source.join_node_id.clone(),
        source_member_node_ids: details.members.iter().map(|m| m.node_id.clone()).collect(),
        members_replaced,
        built,
        entry_from_node,
        entry_condition,
        on_pass_to,
        on_fail_to,
    })
}

struct SpecRunInfo {
    name: String,
    current_node: Option<String>,
    blocker: Option<String>,
}

fn build_spec_run_info(db: &Database, spec: &LoopSpec) -> Result<SpecRunInfo, McpError> {
    let runs = db
        .list_loop_runs_for_spec(&spec.id)
        .map_err(internal_error)?;
    let current_node = runs
        .iter()
        .rev()
        .find(|run| run.status == LoopRunStatus::Running)
        .or_else(|| runs.last())
        .map(|run| run.node_id.clone());
    let blocker = runs.last().and_then(loop_run_blocker);
    Ok(SpecRunInfo {
        name: spec.name.clone(),
        current_node,
        blocker,
    })
}

fn build_loop_summary_json(db: &Database, lp: &Loop) -> Result<serde_json::Value, McpError> {
    let specs = db.list_loop_specs(&lp.id).map_err(internal_error)?;
    let current_spec = specs
        .into_iter()
        .find(|spec| {
            matches!(
                spec.status,
                LoopSpecStatus::Running | LoopSpecStatus::Pending
            )
        })
        .map(|spec| build_spec_run_info(db, &spec))
        .transpose()?;

    Ok(serde_json::json!({
        "id": lp.id,
        "name": lp.name,
        "status": lp.status.as_str(),
        "trigger": loop_trigger_json(lp),
        "current_spec": current_spec.as_ref().map(|v| &v.name),
        "current_node": current_spec.as_ref().and_then(|v| v.current_node.as_ref()),
        "blocked": current_spec.as_ref().is_some_and(|v| v.blocker.is_some()),
        "blocker": current_spec.and_then(|v| v.blocker),
        "created_at": lp.created_at.to_rfc3339(),
        "workdir": lp.workdir,
    }))
}

/// Serialize a loop's trigger for MCP responses: always a `type` label, plus
/// the cron schedule or watch path/events when applicable.
fn loop_trigger_json(lp: &Loop) -> serde_json::Value {
    let mut out = serde_json::json!({ "type": lp.trigger_type_label() });
    if let Some(schedule) = lp.schedule_expr() {
        out["schedule"] = serde_json::json!(schedule);
    }
    if let Some(path) = lp.watch_path() {
        out["path"] = serde_json::json!(path);
        if let Some(events) = lp.watch_events() {
            out["events"] =
                serde_json::json!(events.iter().map(|e| e.to_string()).collect::<Vec<_>>());
        }
    }
    out
}

fn build_loop_list_json(db: &Database, loops: &[Loop]) -> Result<Vec<serde_json::Value>, McpError> {
    loops
        .iter()
        .map(|lp| build_loop_summary_json(db, lp))
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
                "sync_declare_intent — announce your mission (impact: low/high/breaking)"
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
                "Follow the action-risk table: low=execute and broadcast if notable, high=declare+execute+report, breaking=same as high with impact=breaking.",
                "Always non-blocking — act on last-known state, never wait for responses.",
                "Communicate intent not implementation — missions explain what and why, not how."
            ],
            "tools": [
                "sync_get_context — check active missions and workspace vibe (call first)",
                "sync_declare_intent — announce mission (impact: low/high/breaking)",
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
    pub loop_engine: Arc<LoopEngine>,
    pub notification_service: Arc<dyn NotificationService>,
    pub sync_manager: Arc<SyncManager>,
    /// Shared RAG ingestion manager: rag_search must acquire its embedding
    /// client through this cache (B22) so queries reuse the loaded model and
    /// keep the persisted model-loaded status truthful.
    pub ingestion: Arc<crate::rag::ingestion::IngestionManager>,
    /// Rate limiters for rag_search (10 calls/min). Keyed by agent_id.
    pub rag_limiters: Arc<tokio::sync::Mutex<std::collections::HashMap<String, RateLimiter>>>,
    /// Backing store for `skill_list`/`skill_get` (`~/.canopy/skills/`).
    /// Git operations block, so callers run it via `spawn_blocking`.
    pub dynamic_skills: Arc<crate::dynamic_skills::SkillStore>,
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
        loop_engine: Arc<LoopEngine>,
        notification_service: Arc<dyn NotificationService>,
        sync_manager: Arc<SyncManager>,
        ingestion: Arc<crate::rag::ingestion::IngestionManager>,
        dynamic_skills: Arc<crate::dynamic_skills::SkillStore>,
        port: u16,
    ) -> Self {
        Self {
            db,
            executor,
            watcher_engine,
            scheduler_notify,
            loop_engine,
            notification_service,
            sync_manager,
            ingestion,
            rag_limiters: Arc::new(tokio::sync::Mutex::new(std::collections::HashMap::new())),
            dynamic_skills,
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
        let corrupt = self
            .db
            .list_corrupt_agents()
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;

        if agents.is_empty() && corrupt.is_empty() {
            return Ok(success_result("No agents registered."));
        }

        let mut lines = vec![format!(
            "Found {} agent(s){}:\n",
            agents.len(),
            if corrupt.is_empty() {
                String::new()
            } else {
                format!(" ({} corrupt)", corrupt.len())
            }
        )];

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

        for c in &corrupt {
            lines.push(format!(
                "- **{}** [corrupt config] — trigger_config failed to parse: {}\n",
                c.id, c.error
            ));
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
        let _ = self.watcher_engine.stop_watcher(&id).await;

        // Deletes by id without parsing the stored row, so a corrupt row
        // (e.g. malformed trigger_config) can always be removed — it must
        // never block its own repair.
        let deleted = self
            .db
            .delete_agent(&id)
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;

        if !deleted {
            return Ok(error_result(&format!("No agent found with ID '{}'", id)));
        }

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

    /// Schedule a one-shot future enable for a disabled agent.
    #[tool(
        name = "agent_schedule_enable",
        description = "Schedule a one-shot enable for an agent at a future ISO 8601 time — the agent stays disabled until then, when the scheduler enables it and clears the schedule"
    )]
    async fn task_schedule_enable(
        &self,
        Parameters(AgentScheduleEnableParams { id, at }): Parameters<AgentScheduleEnableParams>,
    ) -> Result<CallToolResult, McpError> {
        let Some(_existing) = self
            .db
            .get_agent(&id)
            .map_err(|e| McpError::internal_error(e.to_string(), None))?
        else {
            return Ok(error_result(&format!("No agent found with ID '{}'", id)));
        };

        let at = match chrono::DateTime::parse_from_rfc3339(&at) {
            Ok(dt) => dt.with_timezone(&chrono::Utc),
            Err(e) => {
                return Ok(error_result(&format!(
                    "Invalid ISO 8601 timestamp '{}': {}",
                    at, e
                )));
            }
        };

        self.db
            .schedule_agent_enable(&id, at)
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;

        self.scheduler_notify.notify_one();

        Ok(success_result(&format!(
            "Agent '{}' scheduled to enable at {}",
            id,
            at.to_rfc3339()
        )))
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
            notify_run_result(&notification_service, &agent_id, result);
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
        description = "List AI models available for use with agents. Pass an optional `platform` (e.g. \"opencode\") to filter to the models that platform can reach; pass `refresh: true` to force a fresh fetch. Every returned model id is the literal string that platform's CLI accepts for its model field — for a universal gateway that is the provider-prefixed form (opencode/big-pickle), for claude the bare form (claude-opus-4-8) — so an id can be copied verbatim into the model field of agent_add or agent_watch. Includes cache provenance (source: cache|live|stale, fetched_at)."
    )]
    async fn task_models(
        &self,
        Parameters(params): Parameters<TaskModelsParams>,
    ) -> Result<CallToolResult, McpError> {
        let force_refresh = params.refresh.unwrap_or(false);

        // Optional platform filter, validated against the platforms actually
        // configured in canopy (registry-driven) when that config is present.
        let platform = params
            .platform
            .as_deref()
            .map(str::trim)
            .filter(|p| !p.is_empty());

        if let Some(platform) = platform {
            if let Some(err) = validate_platform_configured(platform) {
                return Ok(error_result(&err));
            }
            // Registry-driven: a platform that can enumerate its own models is
            // the authoritative source of passable ids (each line is the literal
            // string its model flag accepts, prefix and all). models.dev cannot
            // know a gateway's provider-prefixed form or its private catalog, so
            // when the registry gives us an enumeration command we use it and
            // skip models.dev entirely — never requiring it to be reachable.
            if let Some((binary, args)) = platform_enumeration_cmd(platform) {
                return Ok(native_models_result(platform, binary, args, force_refresh).await);
            }
        }

        // models.dev-derived path: the all-providers listing, or a platform
        // without native enumeration (e.g. claude, whose bare ids are correct).
        let load = tokio::task::spawn_blocking(move || {
            crate::domain::models_db::load_catalog_with_source(force_refresh)
        })
        .await
        .ok()
        .flatten();

        let Some(load) = load else {
            return Ok(error_result(
                "Model catalog unavailable: could not reach models.dev and no local \
                 cache exists at ~/.canopy/models_cache.json. Omit the model field to \
                 use the CLI's default, or retry once network access is restored.",
            ));
        };
        let crate::domain::models_db::CatalogLoad { catalog, source } = load;

        let listing = match platform {
            Some(platform) => {
                let providers = crate::domain::models_db::providers_for_cli(platform);
                if providers.is_empty() {
                    return Ok(error_result(&format!(
                        "No known model providers are mapped for platform '{platform}'. \
                         Omit `platform` to list all providers.",
                    )));
                }
                format!(
                    "Models available to platform '{platform}' (providers: {}):\n{}",
                    providers.join(", "),
                    format_platform_models(&catalog, providers)
                )
            }
            None => format!(
                "Available models (use the model id as the model field):\n{}",
                format_catalog_models(&catalog)
            ),
        };

        Ok(CallToolResult::success(vec![Content::text(
            model_result_footer(&listing, source, catalog.fetched_at),
        )]))
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

        if let Some(new_id) = params.new_id.as_deref() {
            if new_id != params.id {
                if let Err(e) = validate_id(new_id) {
                    return Ok(error_result(&e));
                }
                let new_log_path = make_log_path(new_id)?;

                if agent.is_watch() {
                    let _ = self.watcher_engine.stop_watcher(&params.id).await;
                }

                if let Err(e) = self.db.rename_agent(&params.id, new_id, &new_log_path) {
                    return Ok(error_result(&e.to_string()));
                }

                agent.id = new_id.to_string();
                agent.log_path = new_log_path;
            }
        }

        if let Err(e) = apply_scalar_updates(&mut agent, &params) {
            return Ok(error_result(&e));
        }
        if let Err(e) = apply_trigger_updates(&mut agent, &params, &validate_cron) {
            return Ok(error_result(&e));
        }

        self.db.upsert_agent(&agent).map_err(internal_error)?;
        // Success-notification opt-in (B27) lives outside the agent row's
        // upserted columns, so set it separately when the caller provided it.
        if let Some(notify_on_success) = params.notify_on_success {
            self.db
                .set_agent_notify_on_success(&agent.id, notify_on_success)
                .map_err(internal_error)?;
        }
        if agent.is_cron() {
            self.scheduler_notify.notify_one();
        }

        if let Some(result) = self.restart_updated_watcher(&params, &agent).await? {
            return Ok(result);
        }

        Ok(success_result(&format!(
            "Agent '{}' updated successfully.",
            agent.id
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
        name = "skill_list",
        description = "List skills available to this agent from the dynamic skill store — the \
         union of skills already fetched into ~/.canopy/skills/ and the catalogs of every \
         git source configured under [skills] in ~/.canopy/config.toml. Each entry reports \
         name, one-line description, source URL, and whether it is already installed \
         (fetched locally, served instantly) or merely available (fetch it on demand with \
         skill_get, which clones it into the store transparently). Call this first to \
         discover what's available before deciding which skill_get to call — this system \
         works across every harness, not just one platform's local skills folder."
    )]
    async fn skill_list(&self) -> Result<CallToolResult, McpError> {
        let store = Arc::clone(&self.dynamic_skills);
        let entries = tokio::task::spawn_blocking(move || store.list())
            .await
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;
        Ok(CallToolResult::success(vec![Content::text(
            serde_json::to_string_pretty(&entries).unwrap_or_default(),
        )]))
    }

    #[tool(
        name = "skill_get",
        description = "Fetch a skill's full instructions by name from the dynamic skill \
         store. If the skill isn't in the local store yet, clones it on demand from its \
         configured git source; if it is present but its TTL has expired, transparently \
         checks the source for updates and refreshes the local copy first (network failures \
         during that check are non-fatal — the last-known-good copy is served). Returns the \
         SKILL.md/INSTRUCTIONS.md instructions plus any reference files (inlined when small, \
         otherwise listed by store path). Call skill_list first to find valid skill names."
    )]
    async fn skill_get(
        &self,
        Parameters(params): Parameters<SkillGetParams>,
    ) -> Result<CallToolResult, McpError> {
        let store = Arc::clone(&self.dynamic_skills);
        let content = tokio::task::spawn_blocking(move || store.get(&params.name))
            .await
            .map_err(|e| McpError::internal_error(e.to_string(), None))?
            .map_err(|e| McpError::invalid_params(e.to_string(), None))?;
        Ok(CallToolResult::success(vec![Content::text(
            serde_json::to_string_pretty(&content).unwrap_or_default(),
        )]))
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
        name = "loop_create",
        description = "Create a loop container for a background graph of specs and nodes."
    )]
    async fn loop_create(
        &self,
        Parameters(params): Parameters<LoopCreateParams>,
    ) -> Result<CallToolResult, McpError> {
        let name = params.name.trim();
        if let Err(e) = validate_non_empty(name, "Loop name") {
            return Ok(error_result(&e));
        }
        let workdir = params.workdir.trim();
        if let Err(e) = validate_non_empty(workdir, "Loop workdir") {
            return Ok(error_result(&e));
        }
        if let Err(e) = validate_absolute_dir(workdir) {
            return Ok(error_result(&e));
        }

        let trigger = match build_loop_trigger(&params.trigger) {
            Ok(trigger) => trigger,
            Err(e) => return Ok(error_result(&e)),
        };

        let lp = Loop {
            id: uuid::Uuid::new_v4().to_string(),
            name: name.to_string(),
            description: params.description.filter(|value| !value.trim().is_empty()),
            workdir: workdir.to_string(),
            status: LoopStatus::Draft,
            trigger,
            created_at: chrono::Utc::now(),
            started_at: None,
            completed_at: None,
            autorun_at: None,
            auto_continue_at: None,
            auto_continue_action: None,
            active_run_pool_id: None,
            on_completed: None,
        };

        self.db.insert_loop(&lp).map_err(internal_error)?;
        if let Err(error) = self.db.register_project_path(std::path::Path::new(workdir)) {
            tracing::debug!("Could not register loop project at {workdir}: {error}");
        }
        self.activate_loop_trigger(&lp).await;

        Ok(build_id_result(&lp.id, "loop_id"))
    }

    #[tool(
        name = "loop_update",
        description = "Update loop metadata such as name, description, or workdir."
    )]
    async fn loop_update(
        &self,
        Parameters(params): Parameters<LoopUpdateParams>,
    ) -> Result<CallToolResult, McpError> {
        let loop_id = params.loop_id.trim();
        if let Err(e) = validate_non_empty(loop_id, "Loop ID") {
            return Ok(error_result(&e));
        }
        if let Err(e) = validate_loop_exists(&self.db, loop_id) {
            return Ok(error_result(&e));
        }

        let name = match params.name.as_deref().map(str::trim) {
            Some("") => return Ok(error_result("Loop name must not be empty.")),
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
            Some("") => return Ok(error_result("Loop workdir must not be empty.")),
            Some(value) => {
                if let Err(e) = validate_absolute_dir(value) {
                    return Ok(error_result(&e));
                }
                Some(value)
            }
            None => None,
        };

        // A provided `trigger` param (even kind = "manual") counts as an update.
        let new_trigger = if params.trigger.is_some() {
            match build_loop_trigger(&params.trigger) {
                Ok(trigger) => Some(trigger),
                Err(e) => return Ok(error_result(&e)),
            }
        } else {
            None
        };

        // A provided `on_completed` param (even `null`, to clear it) counts
        // as an update — same `Option<Option<_>>` shape as `description`.
        let new_completion_hook = match &params.on_completed {
            None => None,
            Some(None) => Some(None),
            Some(Some(hook_params)) => match build_loop_completion_hook(hook_params) {
                Ok(hook) => Some(Some(hook)),
                Err(e) => return Ok(error_result(&e)),
            },
        };

        if let Err(e) = validate_at_least_one_bool(
            &[
                name.is_some(),
                description.is_some(),
                workdir.is_some(),
                new_trigger.is_some(),
                new_completion_hook.is_some(),
            ],
            "loop_update",
        ) {
            return Ok(error_result(&e));
        }

        self.db
            .update_loop_details(loop_id, name, description, workdir)
            .map_err(internal_error)?;

        if let Some(trigger) = new_trigger {
            self.db
                .update_loop_trigger(loop_id, trigger.as_ref())
                .map_err(internal_error)?;
            // Tear down any live watcher, then re-activate from the new trigger.
            let _ = self.watcher_engine.stop_loop_watcher(loop_id).await;
            if let Ok(Some(lp)) = self.db.get_loop(loop_id) {
                self.activate_loop_trigger(&lp).await;
            }
        }

        if let Some(hook) = new_completion_hook {
            self.db
                .update_loop_completion_hook(loop_id, hook.as_ref())
                .map_err(internal_error)?;
        }

        Ok(build_loop_update_response(loop_id))
    }

    /// Reconcile a loop's live trigger wiring after create/update: wake the
    /// cron scheduler for cron loops, and start the file watcher for watch
    /// loops. Manual loops need no wiring.
    async fn activate_loop_trigger(&self, lp: &Loop) {
        if lp.is_cron() {
            self.scheduler_notify.notify_one();
        } else if lp.is_watch() {
            if let Err(e) = self.watcher_engine.start_loop_watcher(lp).await {
                tracing::warn!("Loop '{}' saved but watcher failed to start: {}", lp.id, e);
            }
        }
    }

    #[tool(
        name = "loop_add_spec",
        description = "Add an ordered spec to an existing loop."
    )]
    async fn loop_add_spec(
        &self,
        Parameters(params): Parameters<LoopAddSpecParams>,
    ) -> Result<CallToolResult, McpError> {
        let name = params.name.trim();
        if let Err(e) = validate_non_empty(name, "Loop spec name") {
            return Ok(error_result(&e));
        }
        let loop_id = params.loop_id.trim();
        if let Err(e) = validate_loop_exists(&self.db, loop_id) {
            return Ok(error_result(&e));
        }

        let existing_specs = self.db.list_loop_specs(loop_id).map_err(internal_error)?;
        if existing_specs
            .iter()
            .any(|spec| spec.position == params.position)
        {
            return Ok(error_result(&format!(
                "Loop '{loop_id}' already has a spec at position {}.",
                params.position
            )));
        }
        let Some(description) = params.description.as_deref().map(str::trim) else {
            return Ok(error_result(
                "Loop spec description is required and must follow the minimum template.",
            ));
        };
        if let Err(error) = validate_spec_description_template(description) {
            return Ok(error_result(&error));
        }

        let spec = LoopSpec {
            id: uuid::Uuid::new_v4().to_string(),
            loop_id: Some(loop_id.to_string()),
            name: name.to_string(),
            description: Some(description.to_string()),
            position: params.position,
            parallelizable: params.parallelizable,
            status: LoopSpecStatus::Pending,
            started_at: None,
            completed_at: None,
            spec_start_head: None,
            workdir: None,
            completed_via: None,
            completed_via_reason: None,
            completed_via_at: None,
        };
        self.db.insert_loop_spec(&spec).map_err(internal_error)?;

        Ok(build_id_result(&spec.id, "spec_id"))
    }

    #[tool(
        name = "loop_update_spec",
        description = "Update an existing loop spec."
    )]
    async fn loop_update_spec(
        &self,
        Parameters(params): Parameters<LoopUpdateSpecParams>,
    ) -> Result<CallToolResult, McpError> {
        let spec_id = params.spec_id.trim();
        let spec = match validate_spec_exists(&self.db, spec_id) {
            Ok(spec) => spec,
            Err(e) => return Ok(error_result(&e)),
        };

        let name = match params.name.as_deref().map(str::trim) {
            Some("") => return Ok(error_result("Loop spec name must not be empty.")),
            Some(value) => Some(value),
            None => None,
        };
        let description = match params.description.as_deref().map(str::trim) {
            Some("") => {
                return Ok(error_result(
                    "Loop spec description must not be empty and must follow the template.",
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
                validate_position_conflict(&self.db, spec.loop_id.as_deref(), spec_id, position)
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
            "loop_update_spec",
        ) {
            return Ok(error_result(&e));
        }

        self.db
            .update_loop_spec_details(
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
        name = "spec_create",
        description = "Create a standalone spec (a backlog item) not yet assigned to any loop. Optionally tag it to a workdir for later filtering."
    )]
    async fn spec_create(
        &self,
        Parameters(params): Parameters<SpecCreateParams>,
    ) -> Result<CallToolResult, McpError> {
        let name = params.name.trim();
        if let Err(e) = validate_non_empty(name, "Spec name") {
            return Ok(error_result(&e));
        }
        let description = params.description.trim();
        if let Err(e) = validate_non_empty(description, "Spec description") {
            return Ok(error_result(&e));
        }
        if let Err(error) = validate_spec_description_template(description) {
            return Ok(error_result(&error));
        }
        let workdir = match params.workdir.as_deref().map(str::trim) {
            Some("") | None => None,
            Some(value) => {
                if let Err(e) = validate_spec_workdir(value) {
                    return Ok(error_result(&e));
                }
                Some(value.to_string())
            }
        };

        let spec = LoopSpec {
            id: uuid::Uuid::new_v4().to_string(),
            loop_id: None,
            name: name.to_string(),
            description: Some(description.to_string()),
            position: 0,
            parallelizable: false,
            status: LoopSpecStatus::Pending,
            started_at: None,
            completed_at: None,
            spec_start_head: None,
            workdir,
            completed_via: None,
            completed_via_reason: None,
            completed_via_at: None,
        };
        self.db.insert_loop_spec(&spec).map_err(internal_error)?;

        Ok(build_id_result(&spec.id, "spec_id"))
    }

    #[tool(
        name = "spec_list",
        description = "List standalone/backlog specs, optionally filtered by workdir tag, status, or unassigned-only."
    )]
    async fn spec_list(
        &self,
        Parameters(params): Parameters<SpecListParams>,
    ) -> Result<CallToolResult, McpError> {
        let status = match params.status.as_deref().map(str::trim) {
            Some(value) => match validate_spec_status(value) {
                Ok(status) => Some(status),
                Err(e) => return Ok(error_result(&e)),
            },
            None => None,
        };

        let specs = self
            .db
            .list_specs(
                params.workdir.as_deref(),
                status,
                params.unassigned_only.unwrap_or(false),
            )
            .map_err(internal_error)?;

        Ok(CallToolResult::success(vec![Content::text(
            serde_json::to_string_pretty(&serde_json::json!({
                "specs": specs.iter().map(spec_summary_json).collect::<Vec<_>>(),
            }))
            .unwrap_or_default(),
        )]))
    }

    #[tool(
        name = "spec_update",
        description = "Update a standalone spec's name, description, and/or workdir tag."
    )]
    async fn spec_update(
        &self,
        Parameters(params): Parameters<SpecUpdateParams>,
    ) -> Result<CallToolResult, McpError> {
        let spec_id = params.spec_id.trim();
        if let Err(e) = validate_spec_exists(&self.db, spec_id) {
            return Ok(error_result(&e));
        }

        let name = match params.name.as_deref().map(str::trim) {
            Some("") => return Ok(error_result("Spec name must not be empty.")),
            Some(value) => Some(value),
            None => None,
        };
        let description = match params.description.as_deref().map(str::trim) {
            Some("") => {
                return Ok(error_result(
                    "Spec description must not be empty and must follow the template.",
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
        let workdir = match &params.workdir {
            Some(Some(value)) => {
                let trimmed = value.trim();
                if let Err(e) = validate_spec_workdir(trimmed) {
                    return Ok(error_result(&e));
                }
                Some(Some(trimmed))
            }
            Some(None) => Some(None),
            None => None,
        };

        if let Err(e) = validate_at_least_one_bool(
            &[name.is_some(), description.is_some(), workdir.is_some()],
            "spec_update",
        ) {
            return Ok(error_result(&e));
        }

        self.db
            .update_spec_tag_details(spec_id, name, description, workdir)
            .map_err(internal_error)?;

        Ok(build_spec_update_response(spec_id))
    }

    #[tool(
        name = "spec_set_status",
        description = "Administratively transition a standalone spec's status (completed, skipped, or pending). Rejects if the spec is bound to a loop or has an active run."
    )]
    async fn spec_set_status(
        &self,
        Parameters(params): Parameters<SpecSetStatusParams>,
    ) -> Result<CallToolResult, McpError> {
        let spec_id = params.spec_id.trim();
        if let Err(e) = validate_spec_exists(&self.db, spec_id) {
            return Ok(error_result(&e));
        }

        let status = match validate_spec_set_status_target(params.status.as_str()) {
            Ok(s) => s,
            Err(e) => return Ok(error_result(&e)),
        };

        let reason = params.reason.trim();
        if reason.is_empty() {
            return Ok(error_result("Reason must not be empty."));
        }

        match self.db.set_spec_admin_status(spec_id, status, reason)
            .map_err(internal_error)? {
            SpecAdminStatusOutcome::Success => {
                Ok(success_result(&format!(
                    "Spec '{spec_id}' set to '{}' (admin): {reason}",
                    status.as_str()
                )))
            },
            SpecAdminStatusOutcome::NotFound => {
                Ok(error_result(&format!("Spec '{spec_id}' not found.")))
            },
            SpecAdminStatusOutcome::NotStandalone(loop_id) => {
                Ok(error_result(&format!(
                    "Spec '{spec_id}' is bound to loop '{loop_id}'; spec_set_status only \
                     administers standalone specs."
                )))
            },
            SpecAdminStatusOutcome::ActiveRun { loop_id, run_id } => {
                Ok(error_result(&format!(
                    "Spec '{spec_id}' is attached to an active run (loop '{loop_id}', run '{run_id}'); \
                     it cannot be administratively transitioned while running."
                )))
            },
        }
    }

    #[tool(
        name = "spec_delete",
        description = "Delete a standalone spec. Refuses to delete a spec that's bound to a loop."
    )]
    async fn spec_delete(
        &self,
        Parameters(params): Parameters<SpecDeleteParams>,
    ) -> Result<CallToolResult, McpError> {
        let spec_id = params.spec_id.trim();
        let spec = match validate_spec_exists(&self.db, spec_id) {
            Ok(spec) => spec,
            Err(e) => return Ok(error_result(&e)),
        };
        if let Err(e) = validate_spec_deletable(&spec) {
            return Ok(error_result(&e));
        }

        self.db.delete_loop_spec(spec_id).map_err(internal_error)?;

        Ok(success_result(&format!("Spec '{spec_id}' deleted.")))
    }

    #[tool(
        name = "blueprint_list",
        description = "List every node blueprint (builtin and custom) — predesigned, reusable node templates for loop_add_node."
    )]
    async fn blueprint_list(&self) -> Result<CallToolResult, McpError> {
        let blueprints = self.db.list_blueprints().map_err(internal_error)?;

        Ok(CallToolResult::success(vec![Content::text(
            serde_json::to_string_pretty(&serde_json::json!({
                "blueprints": blueprints.iter().map(blueprint_json).collect::<Vec<_>>(),
            }))
            .unwrap_or_default(),
        )]))
    }

    #[tool(
        name = "blueprint_create",
        description = "Create a custom node blueprint: a reusable {name, kind, config template} that loop_add_node can reference by name."
    )]
    async fn blueprint_create(
        &self,
        Parameters(params): Parameters<BlueprintCreateParams>,
    ) -> Result<CallToolResult, McpError> {
        let name = params.name.trim();
        if let Err(e) = validate_non_empty(name, "Blueprint name") {
            return Ok(error_result(&e));
        }
        if self
            .db
            .get_blueprint_by_name(name)
            .map_err(internal_error)?
            .is_some()
        {
            return Ok(error_result(&format!(
                "Blueprint '{name}' already exists; choose a different name."
            )));
        }

        let kind = match validate_node_kind(params.kind.trim()) {
            Ok(kind) => kind,
            Err(e) => return Ok(error_result(&e)),
        };
        let config = serde_json::Value::Object(params.config);
        if let Err(e) = validate_node_config(kind, &config) {
            return Ok(error_result(&e));
        }

        let blueprint = Blueprint {
            id: uuid::Uuid::new_v4().to_string(),
            name: name.to_string(),
            kind,
            config,
            builtin: false,
            created_at: chrono::Utc::now(),
        };
        self.db
            .insert_blueprint(&blueprint)
            .map_err(internal_error)?;

        Ok(build_id_result(&blueprint.id, "blueprint_id"))
    }

    #[tool(
        name = "blueprint_delete",
        description = "Delete a custom node blueprint. Refuses to delete a builtin."
    )]
    async fn blueprint_delete(
        &self,
        Parameters(params): Parameters<BlueprintDeleteParams>,
    ) -> Result<CallToolResult, McpError> {
        let name = params.name.trim();
        let Some(blueprint) = self
            .db
            .get_blueprint_by_name(name)
            .map_err(internal_error)?
        else {
            return Ok(error_result(&format!("Blueprint '{name}' not found.")));
        };
        if let Err(e) = validate_blueprint_deletable(&blueprint) {
            return Ok(error_result(&e));
        }

        self.db
            .delete_blueprint_by_name(name)
            .map_err(internal_error)?;

        Ok(success_result(&format!("Blueprint '{name}' deleted.")))
    }

    #[tool(
        name = "loop_add_node",
        description = "Add a graph node to an existing loop spec, or (via loop_id instead of spec_id) to the loop's top-level graph. Provide either a full 'config' (with 'kind'), or a 'blueprint' name (see blueprint_list) optionally shallow-merged with 'config_overrides'."
    )]
    async fn loop_add_node(
        &self,
        Parameters(params): Parameters<LoopAddNodeParams>,
    ) -> Result<CallToolResult, McpError> {
        let name = params.name.trim();
        if let Err(e) = validate_non_empty(name, "Loop node name") {
            return Ok(error_result(&e));
        }
        let target = match resolve_graph_target(
            &self.db,
            params.spec_id.as_deref(),
            params.loop_id.as_deref(),
        ) {
            Ok(target) => target,
            Err(e) => return Ok(error_result(&e)),
        };

        let (kind, config) = match resolve_node_kind_and_config(
            &self.db,
            params.kind.as_deref(),
            params.config,
            params.blueprint.as_deref(),
            params.config_overrides,
        ) {
            Ok(result) => result,
            Err(e) => return Ok(error_result(&e)),
        };
        if let Err(e) = validate_not_join_kind(kind) {
            return Ok(error_result(&e));
        }
        if let Err(e) = validate_node_config(kind, &config) {
            return Ok(error_result(&e));
        }

        let (spec_id, loop_id, next_position) = match &target {
            GraphTarget::Spec(spec_id) => {
                let next_position = self
                    .db
                    .list_loop_nodes(spec_id)
                    .map_err(internal_error)?
                    .last()
                    .map(|node| node.position + 1)
                    .unwrap_or(1);
                (Some(spec_id.clone()), None, next_position)
            }
            GraphTarget::Loop(loop_id) => {
                let next_position = self
                    .db
                    .list_loop_nodes_for_loop(loop_id)
                    .map_err(internal_error)?
                    .last()
                    .map(|node| node.position + 1)
                    .unwrap_or(1);
                (None, Some(loop_id.clone()), next_position)
            }
        };

        let node = LoopNode {
            id: uuid::Uuid::new_v4().to_string(),
            spec_id,
            loop_id,
            name: name.to_string(),
            kind,
            config,
            position: next_position,
            created_at: chrono::Utc::now(),
        };
        self.db.insert_loop_node(&node).map_err(internal_error)?;

        Ok(build_id_result(&node.id, "node_id"))
    }

    #[tool(
        name = "loop_update_node",
        description = "Update an existing loop node."
    )]
    async fn loop_update_node(
        &self,
        Parameters(params): Parameters<LoopUpdateNodeParams>,
    ) -> Result<CallToolResult, McpError> {
        let node_id = params.node_id.trim();
        let node = match validate_node_exists(&self.db, node_id) {
            Ok(node) => node,
            Err(e) => return Ok(error_result(&e)),
        };
        if let Err(e) = validate_node_not_ensemble_owned(&self.db, node_id) {
            return Ok(error_result(&e));
        }

        let name = match params.name.as_deref().map(str::trim) {
            Some("") => return Ok(error_result("Loop node name must not be empty.")),
            Some(value) => Some(value),
            None => None,
        };
        let kind = match params.kind.as_deref().map(str::trim) {
            Some("") => return Ok(error_result("Loop node kind must not be empty.")),
            Some(value) => match validate_node_kind(value) {
                Ok(kind) => Some(kind),
                Err(e) => return Ok(error_result(&e)),
            },
            None => None,
        };
        if let Some(kind) = kind {
            if let Err(e) = validate_not_join_kind(kind) {
                return Ok(error_result(&e));
            }
        }

        if let Some(position) = params.position {
            if let Err(e) = validate_node_position_conflict(&self.db, &node, node_id, position) {
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
            "loop_update_node",
        ) {
            return Ok(error_result(&e));
        }

        let config = params.config.map(serde_json::Value::Object);
        if kind.is_some() || config.is_some() {
            let effective_kind = kind.unwrap_or(node.kind);
            let effective_config = config.as_ref().unwrap_or(&node.config);
            if let Err(e) = validate_node_config(effective_kind, effective_config) {
                return Ok(error_result(&e));
            }
        }

        self.db
            .update_loop_node_details(node_id, name, kind, config.as_ref(), params.position)
            .map_err(internal_error)?;

        Ok(build_node_update_response(node_id))
    }

    #[tool(
        name = "loop_add_edge",
        description = "Connect two nodes with a routing condition, inside a loop spec's graph or (via loop_id instead of spec_id) the loop's top-level graph."
    )]
    async fn loop_add_edge(
        &self,
        Parameters(params): Parameters<LoopAddEdgeParams>,
    ) -> Result<CallToolResult, McpError> {
        let condition = match validate_edge_condition(params.condition.trim()) {
            Ok(c) => c,
            Err(e) => return Ok(error_result(&e)),
        };
        let target = match resolve_graph_target(
            &self.db,
            params.spec_id.as_deref(),
            params.loop_id.as_deref(),
        ) {
            Ok(target) => target,
            Err(e) => return Ok(error_result(&e)),
        };

        let nodes = match &target {
            GraphTarget::Spec(spec_id) => {
                self.db.list_loop_nodes(spec_id).map_err(internal_error)?
            }
            GraphTarget::Loop(loop_id) => self
                .db
                .list_loop_nodes_for_loop(loop_id)
                .map_err(internal_error)?,
        };
        let has_from = nodes.iter().any(|node| node.id == params.from_node);
        let has_to = nodes.iter().any(|node| node.id == params.to_node);
        if !has_from || !has_to {
            return Ok(error_result(
                "Both loop edge endpoints must belong to the same spec or loop graph as the edge.",
            ));
        }
        if let Err(e) = validate_node_not_ensemble_owned(&self.db, &params.from_node) {
            return Ok(error_result(&e));
        }
        if let Err(e) = validate_node_not_ensemble_owned(&self.db, &params.to_node) {
            return Ok(error_result(&e));
        }

        let (spec_id, loop_id) = match target {
            GraphTarget::Spec(spec_id) => (Some(spec_id), None),
            GraphTarget::Loop(loop_id) => (None, Some(loop_id)),
        };
        let edge = LoopEdge {
            id: uuid::Uuid::new_v4().to_string(),
            spec_id,
            loop_id,
            from_node: params.from_node,
            to_node: params.to_node,
            condition,
        };
        self.db.insert_loop_edge(&edge).map_err(internal_error)?;

        Ok(CallToolResult::success(vec![Content::text(
            serde_json::to_string_pretty(&serde_json::json!({ "ok": true })).unwrap_or_default(),
        )]))
    }

    #[tool(
        name = "loop_update_edge",
        description = "Update the routing condition of an existing loop edge."
    )]
    async fn loop_update_edge(
        &self,
        Parameters(params): Parameters<LoopUpdateEdgeParams>,
    ) -> Result<CallToolResult, McpError> {
        let edge = match validate_edge_exists(&self.db, params.edge_id.trim()) {
            Ok(e) => e,
            Err(e) => return Ok(error_result(&e)),
        };
        if let Err(e) = validate_node_not_ensemble_owned(&self.db, &edge.from_node) {
            return Ok(error_result(&e));
        }
        if let Err(e) = validate_node_not_ensemble_owned(&self.db, &edge.to_node) {
            return Ok(error_result(&e));
        }
        let condition = match validate_edge_condition(params.condition.trim()) {
            Ok(c) => c,
            Err(e) => return Ok(error_result(&e)),
        };

        if edge.condition == condition {
            return Ok(success_result(&format!(
                "Loop edge '{}' already uses condition '{}'.",
                edge.id,
                condition.as_str()
            )));
        }

        self.db
            .update_loop_edge_condition(&edge.id, condition)
            .map_err(internal_error)?;

        Ok(success_result(&format!("Loop edge '{}' updated.", edge.id)))
    }

    #[tool(
        name = "loop_add_ensemble",
        description = "Create an ensemble in ONE call: N (2-8) parallel agent-node members sharing one prompt, plus the quorum that waits for all of them, consolidates their outputs, and routes onward. Members differ only by platform/model. (Formerly called 'fusion' — retired to avoid colliding with OpenRouter's fusion technology.)"
    )]
    async fn loop_add_ensemble(
        &self,
        Parameters(params): Parameters<LoopAddEnsembleParams>,
    ) -> Result<CallToolResult, McpError> {
        let name = params.name.trim();
        if let Err(e) = validate_non_empty(name, "Ensemble name") {
            return Ok(error_result(&e));
        }

        let blueprint_name = params
            .blueprint
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty());
        let blueprint = match blueprint_name {
            Some(blueprint_name) => match self
                .db
                .get_ensemble_blueprint_by_name(blueprint_name)
                .map_err(internal_error)?
            {
                Some(bp) => Some(bp),
                None => {
                    return Ok(error_result(&format!(
                        "Unknown ensemble blueprint '{blueprint_name}'."
                    )))
                }
            },
            None => None,
        };

        let prompt_template: String = match params
            .prompt_template
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
        {
            Some(explicit) => explicit.to_string(),
            None => match &blueprint {
                Some(bp) => bp.prompt_template.clone(),
                None => {
                    return Ok(error_result(
                        "Provide prompt_template, or a blueprint that supplies one.",
                    ))
                }
            },
        };
        let prompt_template = prompt_template.as_str();

        let members: Vec<(String, Option<String>)> = match &params.members {
            Some(explicit) => match validate_ensemble_members(explicit) {
                Ok(members) => members,
                Err(e) => return Ok(error_result(&e)),
            },
            None => match &blueprint {
                Some(bp) => bp.members.clone(),
                None => {
                    return Ok(error_result(
                        "Provide members, or a blueprint that supplies them.",
                    ))
                }
            },
        };
        let condition = match validate_edge_condition(params.condition.trim()) {
            Ok(c) => c,
            Err(e) => return Ok(error_result(&e)),
        };

        let target = match resolve_graph_target(
            &self.db,
            params.spec_id.as_deref(),
            params.loop_id.as_deref(),
        ) {
            Ok(target) => target,
            Err(e) => return Ok(error_result(&e)),
        };
        let existing_nodes = match &target {
            GraphTarget::Spec(spec_id) => {
                self.db.list_loop_nodes(spec_id).map_err(internal_error)?
            }
            GraphTarget::Loop(loop_id) => self
                .db
                .list_loop_nodes_for_loop(loop_id)
                .map_err(internal_error)?,
        };
        let node_exists = |id: &str| existing_nodes.iter().any(|node| node.id == id);

        let from_node = params.from_node.trim();
        if !node_exists(from_node) {
            return Ok(error_result(&format!(
                "Loop node '{from_node}' not found in the target graph."
            )));
        }
        if let Err(e) = validate_node_not_ensemble_owned(&self.db, from_node) {
            return Ok(error_result(&format!(
                "Cannot wire an ensemble's entry from an ensemble-owned node (nested ensembles are not supported): {e}"
            )));
        }

        let on_pass_to = params.on_pass_to.trim();
        if let Err(e) = validate_non_empty(on_pass_to, "on_pass_to") {
            return Ok(error_result(&e));
        }
        if !node_exists(on_pass_to) {
            return Ok(error_result(&format!(
                "Loop node '{on_pass_to}' not found in the target graph."
            )));
        }
        if let Err(e) = validate_node_not_ensemble_owned(&self.db, on_pass_to) {
            return Ok(error_result(&format!(
                "Cannot wire an ensemble's exit into another ensemble's members/quorum (nested ensembles are not supported): {e}"
            )));
        }

        let on_fail_to = params
            .on_fail_to
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty());
        if let Some(on_fail_to) = on_fail_to {
            if !node_exists(on_fail_to) {
                return Ok(error_result(&format!(
                    "Loop node '{on_fail_to}' not found in the target graph."
                )));
            }
            if let Err(e) = validate_node_not_ensemble_owned(&self.db, on_fail_to) {
                return Ok(error_result(&format!(
                "Cannot wire an ensemble's exit into another ensemble's members/quorum (nested ensembles are not supported): {e}"
                )));
            }
        }

        let min_pass = params
            .min_pass
            .or_else(|| blueprint.as_ref().and_then(|bp| bp.min_pass))
            .unwrap_or(members.len() as i64);
        if min_pass < 1 || min_pass > members.len() as i64 {
            return Ok(error_result(&format!(
                "min_pass must be between 1 and {} (the member count), got {min_pass}.",
                members.len()
            )));
        }
        let timeout_minutes = params
            .timeout_minutes
            .unwrap_or(DEFAULT_ENSEMBLE_MEMBER_TIMEOUT_MINUTES);
        if timeout_minutes < 0 {
            return Ok(error_result("timeout_minutes must not be negative."));
        }
        if let Some(straggler) = params.straggler_timeout_minutes {
            if straggler < 0 {
                return Ok(error_result(
                    "straggler_timeout_minutes must not be negative.",
                ));
            }
        }

        let (spec_id, loop_id) = match &target {
            GraphTarget::Spec(spec_id) => (Some(spec_id.clone()), None),
            GraphTarget::Loop(loop_id) => (None, Some(loop_id.clone())),
        };
        let start_position = existing_nodes
            .last()
            .map(|node| node.position + 1)
            .unwrap_or(1);

        let built = build_ensemble_unit(&EnsembleUnitSpec {
            spec_id,
            loop_id,
            name,
            prompt_template,
            members: &members,
            entry_from_node: from_node,
            entry_condition: condition,
            on_pass_to,
            on_fail_to,
            min_pass,
            timeout_minutes,
            straggler_timeout_minutes: params.straggler_timeout_minutes,
            start_position,
        });

        self.db
            .insert_ensemble_unit(
                &built.ensemble,
                &built.members,
                &built.member_nodes,
                &built.join_node,
                &built.edges,
            )
            .map_err(internal_error)?;

        Ok(build_id_result(&built.ensemble.id, "ensemble_id"))
    }

    #[tool(
        name = "loop_copy_node",
        description = "Duplicate a loop node's CONFIG (never its runtime state) into a graph — the source's own by default, or a different spec/loop for a cross-loop copy. Optional overrides: name, and config_overrides shallow-merged over the copied config (e.g. swap prompt_template, platform, model, timeout_minutes). Optional wiring: entry_from_node/entry_condition (incoming edge) and on_pass_to/on_fail_to (outgoing edges). All ids are new. An unwired copy is valid and is reported as such — no edges are assumed."
    )]
    async fn loop_copy_node(
        &self,
        Parameters(params): Parameters<LoopCopyNodeParams>,
    ) -> Result<CallToolResult, McpError> {
        let plan = match plan_node_copy(&self.db, &params) {
            Ok(plan) => plan,
            Err(e) => return Ok(error_result(&e)),
        };

        self.db
            .insert_node_with_edges(&plan.node, &plan.edges)
            .map_err(internal_error)?;

        let wired = !plan.edges.is_empty();
        let mut mapping = serde_json::Map::new();
        mapping.insert(plan.source_id.clone(), serde_json::json!(plan.node.id));
        let note = node_copy_note(&plan.source_id, &plan.node.id, wired);
        Ok(build_json_result(&serde_json::json!({
            "node_id": plan.node.id,
            "mapping": mapping,
            "wired": wired,
            "wiring": plan.wiring,
            "copied_runtime_state": false,
            "note": note,
        })))
    }

    #[tool(
        name = "loop_copy_ensemble",
        description = "Duplicate a whole ensemble unit (members + quorum + shared prompt) — CONFIG only, never runtime state — in one call. Optional overrides: name, prompt_template (e.g. swap a proposer prompt for a review prompt), members (2-8 replacement), min_pass, timeout_minutes, straggler_timeout_minutes, and wiring (from_node/condition entry, on_pass_to/on_fail_to exit). Wiring defaults to the source's; for a cross-loop copy pass wiring that exists in the target graph. Every id is new; the response returns the full old→new id mapping and the wiring actually applied."
    )]
    async fn loop_copy_ensemble(
        &self,
        Parameters(params): Parameters<LoopCopyEnsembleParams>,
    ) -> Result<CallToolResult, McpError> {
        let plan = match plan_ensemble_copy(&self.db, &params) {
            Ok(plan) => plan,
            Err(e) => return Ok(error_result(&e)),
        };
        let built = &plan.built;

        self.db
            .insert_ensemble_unit(
                &built.ensemble,
                &built.members,
                &built.member_nodes,
                &built.join_node,
                &built.edges,
            )
            .map_err(internal_error)?;

        // Full old→new id mapping. Member nodes map by position only when the
        // members weren't replaced (otherwise there's no 1:1 correspondence).
        let mut mapping = serde_json::Map::new();
        mapping.insert(
            plan.source_ensemble_id.clone(),
            serde_json::json!(built.ensemble.id),
        );
        mapping.insert(
            plan.source_join_node_id.clone(),
            serde_json::json!(built.join_node.id),
        );
        if !plan.members_replaced {
            for (old_id, new_node) in plan
                .source_member_node_ids
                .iter()
                .zip(built.member_nodes.iter())
            {
                mapping.insert(old_id.clone(), serde_json::json!(new_node.id));
            }
        }
        let new_member_node_ids: Vec<&str> = built
            .member_nodes
            .iter()
            .map(|node| node.id.as_str())
            .collect();

        Ok(build_json_result(&serde_json::json!({
            "ensemble_id": built.ensemble.id,
            "join_node_id": built.join_node.id,
            "mapping": mapping,
            "members_replaced": plan.members_replaced,
            "member_node_ids": new_member_node_ids,
            "wiring": {
                "entry_from_node": plan.entry_from_node,
                "entry_condition": plan.entry_condition.as_str(),
                "on_pass_to": plan.on_pass_to,
                "on_fail_to": plan.on_fail_to,
            },
            "copied_runtime_state": false,
            "note": format!(
                "Copied ensemble '{}' as '{}' — wired from '{}' to '{}'{}.",
                plan.source_ensemble_id,
                built.ensemble.id,
                plan.entry_from_node,
                plan.on_pass_to,
                plan.on_fail_to.as_deref().map(|f| format!(" (fail → '{f}')")).unwrap_or_default(),
            ),
        })))
    }

    #[tool(
        name = "loop_update_ensemble",
        description = "Update an ensemble's shared prompt (propagated to every member), member list (platform/model — added/removed/replaced by position), quorum config (min_pass, straggler_timeout_minutes, timeout_minutes), and/or exit wiring (on_pass_to/on_fail_to) — all in one call, without touching individual member nodes directly."
    )]
    async fn loop_update_ensemble(
        &self,
        Parameters(params): Parameters<LoopUpdateEnsembleParams>,
    ) -> Result<CallToolResult, McpError> {
        let ensemble_id = params.ensemble_id.trim();
        let Some(mut details) = self
            .db
            .get_ensemble_details(ensemble_id)
            .map_err(internal_error)?
        else {
            return Ok(error_result(&format!(
                "Ensemble '{ensemble_id}' not found."
            )));
        };

        if let Err(e) = validate_at_least_one_bool(
            &[
                params.prompt_template.is_some(),
                params.members.is_some(),
                params.min_pass.is_some(),
                params.straggler_timeout_minutes.is_some(),
                params.timeout_minutes.is_some(),
                params.on_pass_to.is_some(),
                params.on_fail_to.is_some(),
            ],
            "loop_update_ensemble",
        ) {
            return Ok(error_result(&e));
        }

        if let Some(prompt_template) = &params.prompt_template {
            if let Err(e) = validate_non_empty(prompt_template.trim(), "Ensemble prompt_template") {
                return Ok(error_result(&e));
            }
        }

        let owner_nodes = match (&details.ensemble.spec_id, &details.ensemble.loop_id) {
            (Some(spec_id), None) => self.db.list_loop_nodes(spec_id).map_err(internal_error)?,
            (None, Some(loop_id)) => self
                .db
                .list_loop_nodes_for_loop(loop_id)
                .map_err(internal_error)?,
            _ => Vec::new(),
        };

        // ── member list resize/replace (add/remove/replace by position) ──
        if let Some(new_members) = &params.members {
            let members = match validate_ensemble_members(new_members) {
                Ok(members) => members,
                Err(e) => return Ok(error_result(&e)),
            };
            let prompt_template = params
                .prompt_template
                .as_deref()
                .unwrap_or(&details.ensemble.prompt_template);
            let timeout_minutes = params
                .timeout_minutes
                .unwrap_or(details.ensemble.timeout_minutes);

            let old_members = details.members.clone();
            let old_len = old_members.len();
            let new_len = members.len();

            for (index, (platform, model)) in members.iter().enumerate().take(old_len.min(new_len))
            {
                let existing = &old_members[index];
                self.db
                    .update_ensemble_member_platform(
                        ensemble_id,
                        &existing.node_id,
                        platform,
                        model.as_deref(),
                    )
                    .map_err(internal_error)?;
                let config = member_node_config(
                    platform,
                    model.as_deref(),
                    prompt_template,
                    timeout_minutes,
                );
                self.db
                    .update_loop_node_details(&existing.node_id, None, None, Some(&config), None)
                    .map_err(internal_error)?;
            }

            if new_len > old_len {
                let start_position = owner_nodes
                    .last()
                    .map(|node| node.position + 1)
                    .unwrap_or(1);
                for (i, (platform, model)) in members[old_len..new_len].iter().enumerate() {
                    let next_position = start_position + i as i64;
                    let next_member_position = old_len as i64 + i as i64;
                    let node_id = uuid::Uuid::new_v4().to_string();
                    let node = LoopNode {
                        id: node_id.clone(),
                        spec_id: details.ensemble.spec_id.clone(),
                        loop_id: details.ensemble.loop_id.clone(),
                        name: format!("{} [{}]", details.ensemble.name, next_member_position + 1),
                        kind: LoopNodeKind::Agent,
                        config: member_node_config(
                            platform,
                            model.as_deref(),
                            prompt_template,
                            timeout_minutes,
                        ),
                        position: next_position,
                        created_at: chrono::Utc::now(),
                    };
                    let entry_edge = LoopEdge {
                        id: uuid::Uuid::new_v4().to_string(),
                        spec_id: details.ensemble.spec_id.clone(),
                        loop_id: details.ensemble.loop_id.clone(),
                        from_node: details.ensemble.entry_from_node.clone(),
                        to_node: node_id.clone(),
                        condition: details.ensemble.entry_condition,
                    };
                    let join_edge = LoopEdge {
                        id: uuid::Uuid::new_v4().to_string(),
                        spec_id: details.ensemble.spec_id.clone(),
                        loop_id: details.ensemble.loop_id.clone(),
                        from_node: node_id.clone(),
                        to_node: details.ensemble.join_node_id.clone(),
                        condition: LoopEdgeCondition::Always,
                    };
                    let member = EnsembleMember {
                        ensemble_id: ensemble_id.to_string(),
                        node_id,
                        position: next_member_position,
                        platform: platform.clone(),
                        model: model.clone(),
                    };
                    self.db
                        .add_ensemble_member(&member, &node, &entry_edge, &join_edge)
                        .map_err(internal_error)?;
                }
            } else if new_len < old_len {
                for existing in &old_members[new_len..old_len] {
                    self.db
                        .remove_ensemble_member(&existing.node_id)
                        .map_err(internal_error)?;
                }
            }

            details = self
                .db
                .get_ensemble_details(ensemble_id)
                .map_err(internal_error)?
                .ok_or_else(|| {
                    internal_error(format!("Ensemble '{ensemble_id}' vanished mid-update."))
                })?;
        } else if params.prompt_template.is_some() || params.timeout_minutes.is_some() {
            // Prompt and/or shared timeout changed without a member-list
            // resize: propagate onto every existing member's config as-is.
            let prompt_template = params
                .prompt_template
                .as_deref()
                .unwrap_or(&details.ensemble.prompt_template);
            let timeout_minutes = params
                .timeout_minutes
                .unwrap_or(details.ensemble.timeout_minutes);
            for member in &details.members {
                let config = member_node_config(
                    &member.platform,
                    member.model.as_deref(),
                    prompt_template,
                    timeout_minutes,
                );
                self.db
                    .update_loop_node_details(&member.node_id, None, None, Some(&config), None)
                    .map_err(internal_error)?;
            }
        }

        if let Some(prompt_template) = params.prompt_template.as_deref() {
            self.db
                .update_ensemble_prompt(ensemble_id, prompt_template.trim())
                .map_err(internal_error)?;
        }

        // ── join config: min_pass / straggler_timeout_minutes / timeout_minutes ──
        if params.min_pass.is_some()
            || params.timeout_minutes.is_some()
            || params.straggler_timeout_minutes.is_some()
        {
            let member_count = details.members.len() as i64;
            if let Some(min_pass) = params.min_pass {
                if min_pass < 1 || min_pass > member_count {
                    return Ok(error_result(&format!(
                        "min_pass must be between 1 and {member_count} (the member count), got {min_pass}."
                    )));
                }
            }
            if let Some(Some(straggler)) = params.straggler_timeout_minutes {
                if straggler < 0 {
                    return Ok(error_result(
                        "straggler_timeout_minutes must not be negative.",
                    ));
                }
            }
            if let Some(timeout_minutes) = params.timeout_minutes {
                if timeout_minutes < 0 {
                    return Ok(error_result("timeout_minutes must not be negative."));
                }
            }
            self.db
                .update_ensemble_join_config(
                    ensemble_id,
                    params.min_pass,
                    params.straggler_timeout_minutes,
                    params.timeout_minutes,
                )
                .map_err(internal_error)?;
        }

        // ── exit wiring: on_pass_to / on_fail_to ──────────────────────
        if params.on_pass_to.is_some() || params.on_fail_to.is_some() {
            let owner_nodes = match (&details.ensemble.spec_id, &details.ensemble.loop_id) {
                (Some(spec_id), None) => {
                    self.db.list_loop_nodes(spec_id).map_err(internal_error)?
                }
                (None, Some(loop_id)) => self
                    .db
                    .list_loop_nodes_for_loop(loop_id)
                    .map_err(internal_error)?,
                _ => Vec::new(),
            };
            let node_exists = |id: &str| owner_nodes.iter().any(|node| node.id == id);

            if let Some(on_pass_to) = params.on_pass_to.as_deref().map(str::trim) {
                if let Err(e) = validate_non_empty(on_pass_to, "on_pass_to") {
                    return Ok(error_result(&e));
                }
                if !node_exists(on_pass_to) {
                    return Ok(error_result(&format!(
                        "Loop node '{on_pass_to}' not found in the ensemble's graph."
                    )));
                }
                if let Err(e) = validate_node_not_ensemble_owned(&self.db, on_pass_to) {
                    return Ok(error_result(&format!(
                        "Cannot wire an ensemble's exit into another ensemble's members/quorum: {e}"
                    )));
                }
                self.db
                    .delete_loop_edges_from_node_with_condition(
                        &details.ensemble.join_node_id,
                        LoopEdgeCondition::Pass,
                    )
                    .map_err(internal_error)?;
                self.db
                    .insert_loop_edge(&LoopEdge {
                        id: uuid::Uuid::new_v4().to_string(),
                        spec_id: details.ensemble.spec_id.clone(),
                        loop_id: details.ensemble.loop_id.clone(),
                        from_node: details.ensemble.join_node_id.clone(),
                        to_node: on_pass_to.to_string(),
                        condition: LoopEdgeCondition::Pass,
                    })
                    .map_err(internal_error)?;
            }

            if let Some(on_fail_to) = &params.on_fail_to {
                self.db
                    .delete_loop_edges_from_node_with_condition(
                        &details.ensemble.join_node_id,
                        LoopEdgeCondition::Fail,
                    )
                    .map_err(internal_error)?;
                if let Some(target) = on_fail_to
                    .as_deref()
                    .map(str::trim)
                    .filter(|v| !v.is_empty())
                {
                    if !node_exists(target) {
                        return Ok(error_result(&format!(
                            "Loop node '{target}' not found in the ensemble's graph."
                        )));
                    }
                    if let Err(e) = validate_node_not_ensemble_owned(&self.db, target) {
                        return Ok(error_result(&format!(
                            "Cannot wire an ensemble's exit into another ensemble's members/quorum: {e}"
                        )));
                    }
                    self.db
                        .insert_loop_edge(&LoopEdge {
                            id: uuid::Uuid::new_v4().to_string(),
                            spec_id: details.ensemble.spec_id.clone(),
                            loop_id: details.ensemble.loop_id.clone(),
                            from_node: details.ensemble.join_node_id,
                            to_node: target.to_string(),
                            condition: LoopEdgeCondition::Fail,
                        })
                        .map_err(internal_error)?;
                }
            }

            self.db
                .update_ensemble_exit_wiring(
                    ensemble_id,
                    params.on_pass_to.as_deref(),
                    params.on_fail_to.as_ref().map(|value| value.as_deref()),
                )
                .map_err(internal_error)?;
        }

        Ok(success_result(&format!(
            "Ensemble '{ensemble_id}' updated."
        )))
    }

    // ---- Queue tools (Q1) --------------------------------------------------
    //
    // A "queue" is an ordered list of existing specs, decoupled from any one
    // loop. The `queue_*` tools below are the primary surface; the `pool_*`
    // tools further down are DEPRECATED thin aliases kept for back-compat.
    // Both call the shared `do_queue_*` helpers so there is exactly one
    // implementation and one place that routes: alias in, one handler out.
    //
    // NOTE: the DB/engine layer still speaks "pool" internally (the `pools`
    // table, `insert_pool`, `list_pool_member_spec_ids`, `validate_pool_*`,
    // etc.). That is deliberate — Q1 renames only the MCP/user surface, never
    // the storage layer. The helpers keep `pool`-named DB calls unchanged.

    async fn do_queue_create(&self, name: &str) -> Result<CallToolResult, McpError> {
        let name = name.trim();
        if let Err(e) = validate_non_empty(name, "Queue name") {
            return Ok(error_result(&e));
        }

        let pool = Pool {
            id: uuid::Uuid::new_v4().to_string(),
            name: name.to_string(),
            created_at: chrono::Utc::now(),
        };
        self.db.insert_pool(&pool).map_err(internal_error)?;

        Ok(build_id_result(&pool.id, "queue_id"))
    }

    async fn do_queue_add_spec(
        &self,
        queue_id: &str,
        spec_id: &str,
        group: Option<&str>,
    ) -> Result<CallToolResult, McpError> {
        let queue_id = queue_id.trim();
        if let Err(e) = validate_pool_exists(&self.db, queue_id) {
            return Ok(error_result(&e));
        }
        let spec_id = spec_id.trim();
        if let Err(e) = validate_spec_exists(&self.db, spec_id) {
            return Ok(error_result(&e));
        }
        let already_member = self
            .db
            .pool_has_member(queue_id, spec_id)
            .map_err(internal_error)?;
        if already_member {
            return Ok(error_result(&format!(
                "Spec '{spec_id}' is already in queue '{queue_id}'."
            )));
        }

        // An all-whitespace or empty `group` is treated as ungrouped.
        let group = group.map(str::trim).filter(|g| !g.is_empty());

        self.db
            .append_pool_member(queue_id, spec_id, group)
            .map_err(internal_error)?;

        let group_note = group
            .map(|g| format!(" in group '{g}'"))
            .unwrap_or_default();
        Ok(success_result(&format!(
            "Spec '{spec_id}' added to queue '{queue_id}'{group_note}."
        )))
    }

    async fn do_queue_list(&self, queue_id: Option<&str>) -> Result<CallToolResult, McpError> {
        let queue_id = queue_id.map(str::trim).filter(|s| !s.is_empty());

        let body = match queue_id {
            Some(queue_id) => {
                let details = match self.db.get_pool_details(queue_id) {
                    Ok(Some(details)) => details,
                    Ok(None) => return Ok(error_result(&format!("Queue '{queue_id}' not found."))),
                    Err(e) => return Err(internal_error(e.to_string())),
                };
                serde_json::json!({ "queue": pool_details_json(&details) })
            }
            None => {
                let pools = self.db.list_pools().map_err(internal_error)?;
                serde_json::json!({
                    "queues": pools
                        .iter()
                        .map(|pool| serde_json::json!({
                            "id": pool.id,
                            "name": pool.name,
                        }))
                        .collect::<Vec<_>>(),
                })
            }
        };

        Ok(CallToolResult::success(vec![Content::text(
            serde_json::to_string_pretty(&body).unwrap_or_default(),
        )]))
    }

    async fn do_queue_remove_spec(
        &self,
        queue_id: &str,
        spec_id: &str,
    ) -> Result<CallToolResult, McpError> {
        let queue_id = queue_id.trim();
        if let Err(e) = validate_pool_exists(&self.db, queue_id) {
            return Ok(error_result(&e));
        }
        let spec_id = spec_id.trim();
        if let Err(e) = validate_pool_member_removable(&self.db, queue_id, spec_id) {
            return Ok(error_result(&e));
        }
        let removed = self
            .db
            .remove_pool_member(queue_id, spec_id)
            .map_err(internal_error)?;
        if !removed {
            return Ok(error_result(&format!(
                "Queue '{queue_id}' has no spec '{spec_id}'."
            )));
        }

        Ok(success_result(&format!(
            "Spec '{spec_id}' removed from queue '{queue_id}'."
        )))
    }

    async fn do_queue_reorder(
        &self,
        queue_id: &str,
        spec_ids: &[String],
    ) -> Result<CallToolResult, McpError> {
        let queue_id = queue_id.trim();
        if let Err(e) = validate_pool_exists(&self.db, queue_id) {
            return Ok(error_result(&e));
        }
        let current = self
            .db
            .list_pool_member_spec_ids(queue_id)
            .map_err(internal_error)?;
        if let Err(e) = validate_pool_reorder(&current, spec_ids) {
            return Ok(error_result(&e));
        }
        if let Err(e) = validate_pool_reorder_locking(&self.db, &current, spec_ids) {
            return Ok(error_result(&e));
        }

        self.db
            .reorder_pool_members(queue_id, spec_ids)
            .map_err(internal_error)?;

        Ok(success_result(&format!("Queue '{queue_id}' reordered.")))
    }

    #[tool(
        name = "queue_create",
        description = "Create a queue: an ordered list of existing specs, decoupled from any one loop."
    )]
    async fn queue_create(
        &self,
        Parameters(params): Parameters<QueueCreateParams>,
    ) -> Result<CallToolResult, McpError> {
        self.do_queue_create(&params.name).await
    }

    #[tool(
        name = "queue_add_spec",
        description = "Append an existing spec to the end of a queue."
    )]
    async fn queue_add_spec(
        &self,
        Parameters(params): Parameters<QueueAddSpecParams>,
    ) -> Result<CallToolResult, McpError> {
        self.do_queue_add_spec(&params.queue_id, &params.spec_id, params.group.as_deref())
            .await
    }

    #[tool(
        name = "queue_list",
        description = "List a queue's ordered members, or every queue (summary only) if queue_id is omitted."
    )]
    async fn queue_list(
        &self,
        Parameters(params): Parameters<QueueListParams>,
    ) -> Result<CallToolResult, McpError> {
        self.do_queue_list(params.queue_id.as_deref()).await
    }

    #[tool(
        name = "queue_remove_spec",
        description = "Remove a spec from a queue."
    )]
    async fn queue_remove_spec(
        &self,
        Parameters(params): Parameters<QueueRemoveSpecParams>,
    ) -> Result<CallToolResult, McpError> {
        self.do_queue_remove_spec(&params.queue_id, &params.spec_id)
            .await
    }

    #[tool(
        name = "queue_reorder",
        description = "Reorder a queue. `spec_ids` must list every queue member exactly once, in the desired order — a total replacement, not a partial swap."
    )]
    async fn queue_reorder(
        &self,
        Parameters(params): Parameters<QueueReorderParams>,
    ) -> Result<CallToolResult, McpError> {
        self.do_queue_reorder(&params.queue_id, &params.spec_ids)
            .await
    }

    #[tool(
        name = "pool_create",
        description = "DEPRECATED: use queue_create instead. Create a queue: an ordered list of existing specs, decoupled from any one loop."
    )]
    async fn pool_create(
        &self,
        Parameters(params): Parameters<PoolCreateParams>,
    ) -> Result<CallToolResult, McpError> {
        self.do_queue_create(&params.name).await
    }

    #[tool(
        name = "pool_add_spec",
        description = "DEPRECATED: use queue_add_spec instead. Append an existing spec to the end of a queue."
    )]
    async fn pool_add_spec(
        &self,
        Parameters(params): Parameters<PoolAddSpecParams>,
    ) -> Result<CallToolResult, McpError> {
        self.do_queue_add_spec(&params.pool_id, &params.spec_id, params.group.as_deref())
            .await
    }

    #[tool(
        name = "pool_list",
        description = "DEPRECATED: use queue_list instead. List a queue's ordered members, or every queue (summary only) if pool_id is omitted."
    )]
    async fn pool_list(
        &self,
        Parameters(params): Parameters<PoolListParams>,
    ) -> Result<CallToolResult, McpError> {
        self.do_queue_list(params.pool_id.as_deref()).await
    }

    #[tool(
        name = "pool_remove_spec",
        description = "DEPRECATED: use queue_remove_spec instead. Remove a spec from a queue."
    )]
    async fn pool_remove_spec(
        &self,
        Parameters(params): Parameters<PoolRemoveSpecParams>,
    ) -> Result<CallToolResult, McpError> {
        self.do_queue_remove_spec(&params.pool_id, &params.spec_id)
            .await
    }

    #[tool(
        name = "pool_reorder",
        description = "DEPRECATED: use queue_reorder instead. Reorder a queue. `spec_ids` must list every queue member exactly once, in the desired order — a total replacement, not a partial swap."
    )]
    async fn pool_reorder(
        &self,
        Parameters(params): Parameters<PoolReorderParams>,
    ) -> Result<CallToolResult, McpError> {
        self.do_queue_reorder(&params.pool_id, &params.spec_ids)
            .await
    }

    #[tool(
        name = "loop_get",
        description = "Return a loop with its ordered specs, nodes, and edges."
    )]
    async fn loop_get(
        &self,
        Parameters(params): Parameters<LoopGetParams>,
    ) -> Result<CallToolResult, McpError> {
        let lp = match self.db.get_loop_details(&params.loop_id) {
            Ok(Some(w)) => w,
            Ok(None) => {
                return Ok(error_result(&format!(
                    "Loop '{}' not found.",
                    params.loop_id
                )))
            }
            Err(e) => return Err(internal_error(e.to_string())),
        };

        Ok(CallToolResult::success(vec![Content::text(
            serde_json::to_string_pretty(
                &loop_details_json(&self.db, &lp).map_err(internal_error)?,
            )
            .unwrap_or_default(),
        )]))
    }

    #[tool(
        name = "loop_list",
        description = "List loops, optionally filtered by workdir."
    )]
    async fn loop_list(
        &self,
        Parameters(params): Parameters<LoopListParams>,
    ) -> Result<CallToolResult, McpError> {
        let loops = self
            .db
            .list_loops(params.workdir.as_deref())
            .map_err(internal_error)?;

        let out = build_loop_list_json(&self.db, &loops)?;

        Ok(CallToolResult::success(vec![Content::text(
            serde_json::to_string_pretty(&out).unwrap_or_default(),
        )]))
    }

    #[tool(
        name = "loop_run",
        description = "Run a loop in the background, spec by spec. With `queue_id`, runs the queue's pending specs (in queue order) through the loop's graph instead of the loop's own bound specs. (`pool_id` is a deprecated alias for `queue_id`; `queue_id` wins if both are set.) `workdir` overrides the loop's workdir for this run only."
    )]
    async fn loop_run(
        &self,
        Parameters(params): Parameters<LoopRunParams>,
    ) -> Result<CallToolResult, McpError> {
        let lp = match self.db.get_loop(&params.loop_id) {
            Ok(Some(w)) => w,
            Ok(None) => {
                return Ok(error_result(&format!(
                    "Loop '{}' not found.",
                    params.loop_id
                )))
            }
            Err(e) => return Err(internal_error(e.to_string())),
        };

        if let Err(message) = loop_run_status_guard(&params.loop_id, lp.status) {
            return Ok(error_result(&message));
        }

        // `queue_id` is the current surface name; `pool_id` is the deprecated
        // alias. Prefer `queue_id`, fall back to `pool_id`. Everything
        // downstream (engine, db) keeps its internal `pool_id` naming.
        let pool_id = params
            .queue_id
            .as_deref()
            .or(params.pool_id.as_deref())
            .map(str::trim)
            .filter(|value| !value.is_empty());
        if let Some(pool_id) = pool_id {
            if let Err(e) = validate_pool_exists(&self.db, pool_id) {
                return Ok(error_result(&e));
            }
            if let Err(e) = validate_pool_not_consumed(&self.db, pool_id, &params.loop_id) {
                return Ok(error_result(&e));
            }
        }

        let workdir = params
            .workdir
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty());
        if let Some(workdir) = workdir {
            if let Err(e) = validate_absolute_dir(workdir) {
                return Ok(error_result(&e));
            }
        }

        if let Err(e) = validate_loop_ensembles_for_run(&self.db, &params.loop_id, pool_id) {
            return Ok(error_result(&e));
        }

        // (B17) An empty effective spec set is a launch error, not a
        // successful no-op run — check it here, synchronously, so the caller
        // (human or an LLM recovery agent) gets the actionable message back
        // directly instead of only via a log line once the fire-and-forget
        // background dispatch below refuses to launch. `run_loop_dispatch`
        // re-runs this identical check right before flipping the loop to
        // `Running`, so every other launch path inherits it too.
        match self
            .loop_engine
            .empty_launch_check(&params.loop_id, pool_id)
        {
            Ok(Some(message)) => return Ok(error_result(&message)),
            Ok(None) => {}
            Err(e) => return Err(internal_error(e.to_string())),
        }

        Arc::clone(&self.loop_engine).start_background_run(
            params.loop_id.clone(),
            pool_id.map(str::to_string),
            workdir.map(str::to_string),
        );
        Ok(success_result(&format!(
            "Loop '{}' launched in background.",
            params.loop_id
        )))
    }

    /// Reset a `completed`/`failed` (or otherwise stalled) loop back to
    /// `pending` so it can be relaunched via `loop_run`, which otherwise
    /// refuses to resume a completed or failed loop.
    ///
    /// This is also the exact state transition the scheduler uses to
    /// auto-reset a `failed` loop when its `loop_schedule_autorun` fires —
    /// both paths call [`crate::db::Database::reset_loop`], so a human
    /// calling this tool and the scheduler resuming a failed loop on its own
    /// can never diverge in behavior.
    #[tool(
        name = "loop_reset",
        description = "Reset a completed/failed loop back to pending so loop_run can relaunch it. Without `specs`, resets every non-completed spec, leaving already-completed ones untouched so loop_run resumes at the first pending spec. With `specs`, resets exactly those spec IDs, even if they were completed. If the loop's last run was against a pool, its pool members are what get reset (same semantics), since a pool run's own bound specs are typically empty. Rejects a `running` loop — call loop_pause first. Note: a `failed` loop with a pending loop_schedule_autorun resets and resumes itself automatically when the schedule fires — call this manually only to reset sooner, reset a `completed` loop, or reset specific spec IDs."
    )]
    async fn loop_reset(
        &self,
        Parameters(params): Parameters<LoopResetParams>,
    ) -> Result<CallToolResult, McpError> {
        perform_loop_reset(&self.db, &params.loop_id, params.specs.as_deref())
    }

    /// Schedule a one-shot future resume for a loop (e.g. a loop that failed
    /// on a quota can reschedule itself at the exact reset time instead of
    /// relying on a blindly polling cron). The scheduler fires it once the
    /// time is reached and the loop is fireable, then clears the schedule.
    ///
    /// Firing on a `failed` loop performs an explicit, logged
    /// auto-reset-and-resume: it runs the same transition as `loop_reset`
    /// (see [`crate::db::Database::reset_loop`]) and then resumes execution —
    /// this is the sanctioned way a quota-failed loop revives itself
    /// unattended. Firing on a `completed` loop does not re-run it: that is
    /// a human decision, made via `loop_reset` + `loop_run`.
    #[tool(
        name = "loop_schedule_autorun",
        description = "Schedule a one-shot resume for a loop at a future ISO 8601 time, or cancel a pending one. When the scheduler reaches that time: a `failed` loop is auto-reset (same transition as loop_reset) and resumed — useful for a loop that failed on a quota to reschedule its own resumption at the exact reset time; a `completed` loop is left alone (the schedule is cleared but the loop is not re-run — use loop_reset + loop_run to re-run a finished loop); any other fireable status launches normally. If the loop's last run was against a pool, the resume targets that same pool (its pending members, in queue order) instead of the loop's own bound specs. The schedule always clears after firing (one-shot). Omit both `at` and `quota_reset_message` to cancel any pending autorun instead of scheduling one — valid regardless of the loop's current status, and a no-op (not an error) if nothing was scheduled. After a quota failure, prefer `quota_reset_message` (the raw CLI text, e.g. \"resets 1pm (America/Bogota)\") over computing `at` yourself — the engine parses the stated local time/timezone and converts it deterministically, avoiding scheduling errors from doing that arithmetic by hand."
    )]
    async fn loop_schedule_autorun(
        &self,
        Parameters(LoopScheduleAutorunParams {
            loop_id,
            at,
            quota_reset_message,
        }): Parameters<LoopScheduleAutorunParams>,
    ) -> Result<CallToolResult, McpError> {
        let Some(existing) = self
            .db
            .get_loop(&loop_id)
            .map_err(|e| McpError::internal_error(e.to_string(), None))?
        else {
            return Ok(error_result(&format!(
                "No loop found with ID '{}'",
                loop_id
            )));
        };

        if at.is_some() && quota_reset_message.is_some() {
            return Ok(error_result(
                "Pass either `at` or `quota_reset_message`, not both.",
            ));
        }

        // B — deterministic, engine-side conversion: a raw CLI quota message
        // ("resets 1pm (America/Bogota)") is parsed and converted to UTC
        // here, in code, rather than trusting the calling model's own
        // local-time arithmetic (the source of a +2h scheduling error on
        // 2026-07-24 — see `domain::quota_reset`).
        if let Some(message) = quota_reset_message {
            let at = match crate::domain::quota_reset::parse_quota_reset_instant(
                &message,
                chrono::Utc::now(),
            ) {
                Ok(at) => at,
                Err(e) => {
                    return Ok(error_result(&format!(
                        "Could not compute a reset instant from '{}': {}",
                        message, e
                    )));
                }
            };
            return self.commit_loop_autorun(&loop_id, at);
        }

        let Some(at) = at else {
            let previously_scheduled = existing.autorun_at;
            self.db
                .clear_loop_autorun(&loop_id)
                .map_err(|e| McpError::internal_error(e.to_string(), None))?;
            return Ok(success_result(&match previously_scheduled {
                Some(previous_at) => format!(
                    "Loop '{}' autorun scheduled for {} was cancelled.",
                    loop_id,
                    previous_at.to_rfc3339()
                ),
                None => format!("Loop '{}' had no pending autorun to cancel.", loop_id),
            }));
        };

        let at = match chrono::DateTime::parse_from_rfc3339(&at) {
            Ok(dt) => dt.with_timezone(&chrono::Utc),
            Err(e) => {
                return Ok(error_result(&format!(
                    "Invalid ISO 8601 timestamp '{}': {}",
                    at, e
                )));
            }
        };

        self.commit_loop_autorun(&loop_id, at)
    }

    /// Shared tail of `loop_schedule_autorun`'s two entry points (`at` and
    /// `quota_reset_message`): persist the schedule (idempotent — a retry
    /// with the same `loop_id`/`at` after a lost ack just re-overwrites the
    /// same single-column schedule, never creating a second pending
    /// schedule) and wake the scheduler.
    fn commit_loop_autorun(
        &self,
        loop_id: &str,
        at: chrono::DateTime<chrono::Utc>,
    ) -> Result<CallToolResult, McpError> {
        self.db
            .schedule_loop_autorun(loop_id, at)
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;

        self.scheduler_notify.notify_one();

        Ok(success_result(&format!(
            "Loop '{}' scheduled to autorun at {}",
            loop_id,
            at.to_rfc3339()
        )))
    }

    /// Schedule a one-shot deferred resume for a loop that is (or will be)
    /// `Paused` — the counterpart to `loop_schedule_autorun` for a loop a
    /// user paused on purpose (e.g. to stop burning quota right now) rather
    /// than one that failed. When the scheduler reaches `at` and the loop is
    /// still `Paused`, it fires the exact `loop_continue` action requested
    /// here — preserving the paused cursor/context — instead of
    /// `autorun_at`'s reset-and-relaunch. If the loop is no longer `Paused`
    /// by then (resumed manually, failed, completed, running), the schedule
    /// is cleared without firing; it never double-runs. Always one-shot.
    #[tool(
        name = "loop_schedule_continue",
        description = "Schedule a one-shot deferred loop_continue for a paused loop at a future ISO 8601 time, or cancel a pending one. When the scheduler reaches that time and the loop is still `paused`, it fires loop_continue with `action` (retry_current_node by default, or skip_next_spec) — resuming with the paused cursor/context intact, never resetting or relaunching. If the loop is no longer paused by then (already continued manually, failed, completed, running), the schedule is cleared without firing. The schedule always clears after firing (one-shot). Omit `at` (or pass null) to cancel any pending auto-continue instead of scheduling one — a no-op (not an error) if nothing was scheduled. Use this instead of loop_schedule_autorun to defer resuming a loop you paused on purpose, e.g. to stop burning quota now and pick back up automatically at a later time."
    )]
    async fn loop_schedule_continue(
        &self,
        Parameters(LoopScheduleContinueParams {
            loop_id,
            at,
            action,
        }): Parameters<LoopScheduleContinueParams>,
    ) -> Result<CallToolResult, McpError> {
        let Some(existing) = self
            .db
            .get_loop(&loop_id)
            .map_err(|e| McpError::internal_error(e.to_string(), None))?
        else {
            return Ok(error_result(&format!(
                "No loop found with ID '{}'",
                loop_id
            )));
        };

        let Some(at) = at else {
            let previously_scheduled = existing.auto_continue_at;
            self.db
                .clear_loop_auto_continue(&loop_id)
                .map_err(|e| McpError::internal_error(e.to_string(), None))?;
            return Ok(success_result(&match previously_scheduled {
                Some(previous_at) => format!(
                    "Loop '{}' auto-continue scheduled for {} was cancelled.",
                    loop_id,
                    previous_at.to_rfc3339()
                ),
                None => format!("Loop '{}' had no pending auto-continue to cancel.", loop_id),
            }));
        };

        let action = action.unwrap_or_else(|| "retry_current_node".to_string());
        if action != "retry_current_node" && action != "skip_next_spec" {
            return Ok(error_result(
                "loop_schedule_continue action must be retry_current_node or skip_next_spec.",
            ));
        }

        let at = match chrono::DateTime::parse_from_rfc3339(&at) {
            Ok(dt) => dt.with_timezone(&chrono::Utc),
            Err(e) => {
                return Ok(error_result(&format!(
                    "Invalid ISO 8601 timestamp '{}': {}",
                    at, e
                )));
            }
        };

        self.db
            .schedule_loop_auto_continue(&loop_id, at, Some(&action))
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;

        self.scheduler_notify.notify_one();

        Ok(success_result(&format!(
            "Loop '{}' scheduled to auto-continue ({}) at {}",
            loop_id,
            action,
            at.to_rfc3339()
        )))
    }

    #[tool(
        name = "loop_pause",
        description = "Pause a running loop after the current node finishes."
    )]
    async fn loop_pause(
        &self,
        Parameters(params): Parameters<LoopPauseParams>,
    ) -> Result<CallToolResult, McpError> {
        let paused = self
            .loop_engine
            .request_pause(&params.loop_id)
            .map_err(internal_error)?;
        if paused {
            Ok(success_result(&format!(
                "Loop '{}' marked to pause.",
                params.loop_id
            )))
        } else {
            Ok(error_result(&format!(
                "Loop '{}' is not running or does not exist.",
                params.loop_id
            )))
        }
    }

    #[tool(
        name = "loop_continue",
        description = "Continue a paused loop by retrying the current node or skipping to the next spec."
    )]
    async fn loop_continue(
        &self,
        Parameters(params): Parameters<LoopContinueParams>,
    ) -> Result<CallToolResult, McpError> {
        let lp = match self.db.get_loop(&params.loop_id) {
            Ok(Some(w)) => w,
            Ok(None) => {
                return Ok(error_result(&format!(
                    "Loop '{}' not found.",
                    params.loop_id
                )))
            }
            Err(e) => return Err(internal_error(e.to_string())),
        };
        if lp.status != LoopStatus::Paused {
            return Ok(error_result(&format!(
                "Loop '{}' is not paused.",
                params.loop_id
            )));
        }

        match params.action.trim() {
            "retry_current_node" => {
                handle_retry_current_node(&self.db, &params.loop_id)?;
            }
            "skip_next_spec" => handle_skip_next_spec(&self.db, &params.loop_id)?,
            _ => {
                return Ok(error_result(
                    "loop_continue action must be retry_current_node or skip_next_spec.",
                ));
            }
        }

        // Resume with the loop's persisted run context — a paused pool run
        // must pick the same pool back up, not the loop's own bound specs.
        // The flip to `Running` is NOT done here: the dispatch's own atomic
        // loop claim (B42) owns that transition, so this resume and any other
        // launch racing it converge on one guarded entry point instead of each
        // pre-flipping the status and then both dispatching.
        Arc::clone(&self.loop_engine).resume_background(params.loop_id.clone());

        Ok(success_result(&format!(
            "Loop '{}' resumed with action '{}'.",
            params.loop_id, params.action
        )))
    }

    #[tool(
        name = "loop_complete_node",
        description = "Mark the active run for a loop node as pass or fail and attach its output."
    )]
    async fn loop_complete_node(
        &self,
        Parameters(params): Parameters<LoopCompleteNodeParams>,
    ) -> Result<CallToolResult, McpError> {
        let status = match params.status.trim() {
            "pass" => Some(LoopRunStatus::Pass),
            "fail" => Some(LoopRunStatus::Fail),
            _ => None,
        };
        let Some(status) = status else {
            return Ok(error_result(
                "loop_complete_node status must be pass or fail.",
            ));
        };
        let run = match resolve_reported_run(&self.db, &params.run_id, &params.node_id)? {
            Ok(run) => run,
            Err(result) => return Ok(result),
        };

        self.db
            .update_loop_run_result(
                &run.id,
                status,
                Some(&serde_json::json!({
                    "reported_output": params.output,
                    "summary": params.summary,
                })),
                Some(chrono::Utc::now()),
            )
            .map_err(internal_error)?;

        Ok(success_result("Loop node result recorded."))
    }

    #[tool(
        name = "loop_report_blocker",
        description = "Pause a loop because the active node is blocked and needs human intervention."
    )]
    async fn loop_report_blocker(
        &self,
        Parameters(params): Parameters<LoopReportBlockerParams>,
    ) -> Result<CallToolResult, McpError> {
        let run = match resolve_reported_run(&self.db, &params.run_id, &params.node_id)? {
            Ok(run) => run,
            Err(result) => return Ok(result),
        };

        self.db
            .update_loop_run_result(
                &run.id,
                LoopRunStatus::Fail,
                Some(&serde_json::json!({
                    "blocker": params.description,
                })),
                Some(chrono::Utc::now()),
            )
            .map_err(internal_error)?;
        self.db
            .update_loop_status(&run.loop_id, LoopStatus::Paused, None, None)
            .map_err(internal_error)?;
        self.loop_engine
            .notify_blocked(&run.loop_id, &params.description)
            .map_err(internal_error)?;

        Ok(success_result("Loop blocker recorded and loop paused."))
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

        // B22: go through the shared ingestion cache instead of building a
        // throwaway client — queries reuse the already-loaded model (no
        // reload per search) and the persisted model-loaded status flips to
        // "ready" for the CLI/TUI, exactly like an indexing pass does.
        let embedding_client = match self.ingestion.get_embedding_client(&config).await {
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

        self.record_rag_search_missions(&results);

        let out: Vec<serde_json::Value> = results.iter().map(rag_result_json).collect();

        Ok(CallToolResult::success(vec![Content::text(
            serde_json::to_string_pretty(&out).unwrap_or_default(),
        )]))
    }

    /// Flag one-shot RAG missions for the TUI to pick up via shared state,
    /// mirroring the `gamification:identity_evolved` mechanism.
    fn record_rag_search_missions(&self, results: &[crate::rag::vector_store::SearchResult]) {
        if results.iter().any(|r| r.distance.is_some_and(|d| d < 0.2)) {
            let _ = self.db.set_state("gamification:deep_rag_search", "1");
        }

        let month_ago = chrono::Utc::now().timestamp() - 30 * 24 * 3600;
        if results.iter().any(|r| r.created_at < month_ago) {
            let _ = self.db.set_state("gamification:digital_archeologist", "1");
        }
    }
}

/// Validate a requested `agent_models` platform against the CLIs actually
/// configured in canopy (registry-driven, from `~/.canopy/config.toml`).
/// Returns `Some(error_message)` if config exists and the platform isn't among
/// the configured CLIs; `None` when the platform is configured, or when no
/// config is present yet (stay lenient rather than rejecting everything).
fn validate_platform_configured(platform: &str) -> Option<String> {
    let home = dirs::home_dir()?;
    let config = crate::domain::canopy_config::CanopyConfig::load(&home.join(".canopy"));
    let configured = config.cli_names();
    if configured.is_empty() || configured.contains(&platform) {
        return None;
    }
    Some(format!(
        "Platform '{platform}' is not configured in canopy. Configured platforms: {}. \
         Omit `platform` to list all providers.",
        configured.join(", ")
    ))
}

/// The `(binary, enumeration args)` for a platform that can list its own
/// models, read straight from its registry-driven `CliConfig` — `None` when the
/// platform has no such command configured (so nothing is inferred from the CLI
/// name). This is the seam that decides opencode enumerates via `opencode
/// models` while claude does not.
fn platform_enumeration_cmd(platform: &str) -> Option<(String, String)> {
    let home = dirs::home_dir()?;
    let config = crate::domain::canopy_config::CanopyConfig::load(&home.join(".canopy"));
    let cli = config.get_cli(platform)?;
    let args = cli.models_list_cmd.as_deref()?.trim();
    if args.is_empty() || cli.binary.is_empty() {
        return None;
    }
    Some((cli.binary.clone(), args.to_string()))
}

/// Build the `agent_models` result from a platform's native enumeration. The
/// CLI run happens off the async executor (it may spawn a process); a fresh
/// cache returns with no CLI call at all.
async fn native_models_result(
    platform: &str,
    binary: String,
    args: String,
    force_refresh: bool,
) -> CallToolResult {
    let platform_owned = platform.to_string();
    let load = tokio::task::spawn_blocking(move || {
        crate::domain::models_db::load_native_models(&platform_owned, &binary, &args, force_refresh)
    })
    .await
    .ok()
    .flatten();

    let Some(load) = load else {
        return error_result(&format!(
            "Could not enumerate '{platform}' models: running its model-list command \
             failed and no cached enumeration exists. Retry once the CLI is reachable."
        ));
    };
    let crate::domain::models_db::NativeLoad { catalog, source } = load;

    let listing = format!(
        "Models available to platform '{platform}' (enumerated from the CLI — ids are \
         passable verbatim):\n{}",
        format_native_models(&catalog.ids)
    );
    CallToolResult::success(vec![Content::text(model_result_footer(
        &listing,
        source,
        catalog.fetched_at,
    ))])
}

/// The shared provenance/footer block for `agent_models`, used by both the
/// models.dev and native-enumeration paths.
fn model_result_footer(
    listing: &str,
    source: crate::domain::models_db::CatalogSource,
    fetched_at: std::time::SystemTime,
) -> String {
    let stale_hint = if source == crate::domain::models_db::CatalogSource::Stale {
        " (the source was unreachable — this cache may be out of date; retry with refresh: true)"
    } else {
        ""
    };
    format!(
        "{listing}\n\n\
         Source: {}{stale_hint} · fetched_at: {}\n\
         Note: model availability also depends on the CLI's configured API keys. \
         If model is omitted, the CLI uses its own default.",
        source.as_str(),
        format_system_time(fetched_at),
    )
}

/// Format a `SystemTime` as an RFC 3339 / ISO 8601 UTC timestamp for the
/// `agent_models` cache metadata.
fn format_system_time(time: std::time::SystemTime) -> String {
    chrono::DateTime::<chrono::Utc>::from(time).to_rfc3339()
}

/// Find and skip `loop_id`'s currently `running` spec — its own bound spec,
/// or, for a pool-driven run, the pool member currently in flight. A pool
/// member's `loop_id` column stays `None` (pool membership never binds it),
/// so `list_loop_specs(loop_id)` alone can't see it (B18): the loop's
/// persisted `active_run_pool_id` is what names the pool to look in instead.
pub(crate) fn handle_skip_next_spec(db: &Database, loop_id: &str) -> Result<(), McpError> {
    let bound_running = db
        .list_loop_specs(loop_id)
        .map_err(internal_error)?
        .into_iter()
        .find(|spec| spec.status == LoopSpecStatus::Running);

    let current_spec = match bound_running {
        Some(spec) => spec,
        None => pool_running_spec(db, loop_id)?.ok_or_else(|| {
            McpError::invalid_params("No running spec found to skip from this paused loop.", None)
        })?,
    };

    db.update_loop_spec_status(
        &current_spec.id,
        LoopSpecStatus::Skipped,
        None,
        Some(chrono::Utc::now()),
    )
    .map_err(internal_error)?;
    Ok(())
}

/// Validate that a running spec exists for `retry_current_node` — the same
/// lookup shape as [`handle_skip_next_spec`] but only checks, never mutates.
pub(crate) fn handle_retry_current_node(db: &Database, loop_id: &str) -> Result<(), McpError> {
    let bound_running = db
        .list_loop_specs(loop_id)
        .map_err(internal_error)?
        .into_iter()
        .find(|spec| spec.status == LoopSpecStatus::Running);
    if bound_running.is_none() && pool_running_spec(db, loop_id)?.is_none() {
        return Err(McpError::invalid_params(
            "No running spec found to retry from this paused loop.",
            None,
        ));
    }
    Ok(())
}

/// The `running` member of `loop_id`'s currently active pool run, if any —
/// `None` if the loop isn't drawing from a pool, or no member is `running`.
fn pool_running_spec(db: &Database, loop_id: &str) -> Result<Option<LoopSpec>, McpError> {
    let Some(pool_id) = db
        .get_loop(loop_id)
        .map_err(internal_error)?
        .and_then(|lp| lp.active_run_pool_id)
    else {
        return Ok(None);
    };
    for spec_id in db
        .list_pool_member_spec_ids(&pool_id)
        .map_err(internal_error)?
    {
        if let Some(spec) = db.get_loop_spec(&spec_id).map_err(internal_error)? {
            if spec.status == LoopSpecStatus::Running {
                return Ok(Some(spec));
            }
        }
    }
    Ok(None)
}

impl TaskTriggerHandler {
    async fn restart_updated_watcher(
        &self,
        params: &TaskUpdateParams,
        agent: &Agent,
    ) -> Result<Option<CallToolResult>, McpError> {
        // A rename always needs the watcher to end up running under the new
        // id, even if no other watch-related field changed.
        let renamed = agent.id != params.id;
        if !agent.is_watch() || (!watcher_restart_needed(params) && !renamed) {
            return Ok(None);
        }

        // The watcher for a renamed agent may already have been stopped
        // under the old id before the rename; stopping it again here is a
        // harmless no-op.
        let _ = self.watcher_engine.stop_watcher(&params.id).await;
        if !agent.enabled {
            return Ok(None);
        }

        let Err(e) = self.watcher_engine.start_watcher(agent).await else {
            return Ok(Some(success_result(&format!(
                "Agent '{}' updated successfully. Watcher restarted with new configuration.",
                agent.id
            ))));
        };

        Ok(Some(CallToolResult::success(vec![Content::text(format!(
            "Agent '{}' updated but watcher failed to restart: {}. It will be retried on daemon restart.",
            agent.id, e
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

fn loop_details_json(db: &Database, lp: &LoopDetails) -> anyhow::Result<serde_json::Value> {
    let specs = lp
        .specs
        .iter()
        .map(|spec| loop_spec_details_json(db, spec, lp.lp.status))
        .collect::<anyhow::Result<Vec<_>>>()?;
    let ensembles = db
        .list_ensembles_for_loop(&lp.lp.id)?
        .iter()
        .map(ensemble_details_json)
        .collect::<Vec<_>>();

    Ok(serde_json::json!({
        "id": lp.lp.id,
        "name": lp.lp.name,
        "description": lp.lp.description,
        "workdir": lp.lp.workdir,
        "status": lp.lp.status.as_str(),
        "trigger": loop_trigger_json(&lp.lp),
        "created_at": lp.lp.created_at.to_rfc3339(),
        "started_at": lp.lp.started_at.map(|value| value.to_rfc3339()),
        "completed_at": lp.lp.completed_at.map(|value| value.to_rfc3339()),
        "autorun_at": lp.lp.autorun_at.map(|value| value.to_rfc3339()),
        "graph": {
            "nodes": lp.graph_nodes.iter().map(loop_node_json).collect::<Vec<_>>(),
            "edges": lp.graph_edges.iter().map(loop_edge_json).collect::<Vec<_>>(),
            "ensembles": ensembles,
        },
        "specs": specs,
        "on_completed": lp.lp.on_completed.as_ref().map(loop_completion_hook_json),
        "completion_hook_runs": lp
            .completion_hook_runs
            .iter()
            .map(loop_completion_hook_run_json)
            .collect::<Vec<_>>(),
    }))
}

/// Serialize an ensemble (F1) as the one unit `loop_get`/`loop_update_ensemble`
/// address — members in position order alongside the join's own config, so a
/// client can render/edit it without reconstructing it from the underlying
/// nodes/edges itself.
fn ensemble_details_json(details: &crate::domain::loops::EnsembleDetails) -> serde_json::Value {
    let ensemble = &details.ensemble;
    serde_json::json!({
        "id": ensemble.id,
        "name": ensemble.name,
        "prompt_template": ensemble.prompt_template,
        "join_node_id": ensemble.join_node_id,
        "entry_from_node": ensemble.entry_from_node,
        "entry_condition": ensemble.entry_condition.as_str(),
        "min_pass": ensemble.min_pass,
        "straggler_timeout_minutes": ensemble.straggler_timeout_minutes,
        "effective_straggler_timeout_minutes": ensemble.effective_straggler_timeout_minutes(),
        "timeout_minutes": ensemble.timeout_minutes,
        "on_pass_to": ensemble.on_pass_to,
        "on_fail_to": ensemble.on_fail_to,
        "created_at": ensemble.created_at.to_rfc3339(),
        "members": details.members.iter().map(|member| serde_json::json!({
            "node_id": member.node_id,
            "position": member.position,
            "platform": member.platform,
            "model": member.model,
        })).collect::<Vec<_>>(),
    })
}

fn loop_completion_hook_json(hook: &crate::domain::loops::LoopCompletionHook) -> serde_json::Value {
    serde_json::json!({
        "platform": hook.platform,
        "model": hook.model,
        "prompt": hook.prompt,
        "timeout_minutes": hook.timeout_minutes,
    })
}

fn loop_completion_hook_run_json(
    run: &crate::domain::loops::LoopCompletionHookRun,
) -> serde_json::Value {
    serde_json::json!({
        "id": run.id,
        "loop_id": run.loop_id,
        "status": run.status.as_str(),
        "output": run.output,
        "summary": run.summary,
        "started_at": run.started_at.to_rfc3339(),
        "completed_at": run.completed_at.map(|value| value.to_rfc3339()),
    })
}

fn loop_spec_details_json(
    db: &Database,
    spec: &crate::domain::loops::LoopSpecDetails,
    loop_status: LoopStatus,
) -> anyhow::Result<serde_json::Value> {
    let runs = db.list_loop_runs_for_spec(&spec.spec.id)?;
    let current_run = runs
        .iter()
        .rev()
        .find(|run| run.status == LoopRunStatus::Running)
        .or_else(|| runs.last());
    let blocker = runs.last().and_then(loop_run_blocker);
    let resume_actions =
        if loop_status == LoopStatus::Paused && spec.spec.status == LoopSpecStatus::Running {
            vec!["retry_current_node", "skip_next_spec"]
        } else {
            Vec::new()
        };
    let runs = runs.iter().map(loop_run_json).collect::<Vec<_>>();
    let ensembles = db
        .list_ensembles_for_spec(&spec.spec.id)?
        .iter()
        .map(ensemble_details_json)
        .collect::<Vec<_>>();

    Ok(serde_json::json!({
        "id": spec.spec.id,
        "loop_id": spec.spec.loop_id,
        "name": spec.spec.name,
        "description": spec.spec.description,
        "position": spec.spec.position,
        "parallelizable": spec.spec.parallelizable,
        "status": spec.spec.status.as_str(),
        "current_node": current_run.map(|run| run.node_id.clone()),
        "blocked": blocker.is_some(),
        "blocker": blocker,
        "resume_actions": resume_actions,
        // The baseline `{{spec_start_head}}` resolved to for this spec's
        // current attempt (B10) — lets debugging see exactly which HEAD a
        // check node's commit-detection compared against.
        "spec_start_head": spec.spec.spec_start_head,
        "started_at": spec.spec.started_at.map(|value| value.to_rfc3339()),
        "completed_at": spec.spec.completed_at.map(|value| value.to_rfc3339()),
        "nodes": spec.nodes.iter().map(loop_node_json).collect::<Vec<_>>(),
        "edges": spec.edges.iter().map(loop_edge_json).collect::<Vec<_>>(),
        "ensembles": ensembles,
        "runs": runs,
    }))
}

fn loop_node_json(node: &LoopNode) -> serde_json::Value {
    serde_json::json!({
        "id": node.id,
        "spec_id": node.spec_id,
        "loop_id": node.loop_id,
        "name": node.name,
        "kind": node.kind.display_str(),
        "config": node.config,
        "position": node.position,
        "created_at": node.created_at.to_rfc3339(),
    })
}

fn loop_edge_json(edge: &LoopEdge) -> serde_json::Value {
    serde_json::json!({
        "id": edge.id,
        "spec_id": edge.spec_id,
        "loop_id": edge.loop_id,
        "from_node": edge.from_node,
        "to_node": edge.to_node,
        "condition": edge.condition.as_str(),
    })
}

fn loop_run_json(run: &crate::domain::loops::LoopNodeRun) -> serde_json::Value {
    serde_json::json!({
        "id": run.id,
        "loop_id": run.loop_id,
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

fn loop_run_blocker(run: &crate::domain::loops::LoopNodeRun) -> Option<String> {
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
    use super::{
        build_ensemble_unit, build_get_tools_response, build_id_result, build_json_result,
        build_loop_completion_hook, build_loop_trigger, build_loop_update_response,
        build_node_update_response, build_spec_update_response, handle_retry_current_node,
        handle_skip_next_spec, header_str, json_value_kind_name, loop_details_json,
        loop_run_status_guard, loop_trigger_json, member_node_config, missing_sync_identity_error,
        node_copy_note, perform_loop_reset, plan_ensemble_copy, plan_node_copy, rag_result_json,
        resolve_graph_target, resolve_node_kind_and_config, resolve_reported_run,
        spec_summary_json, validate_absolute_dir, validate_at_least_one_bool,
        validate_blueprint_exists, validate_edge_condition, validate_ensemble_members,
        validate_node_config, validate_node_kind, validate_node_not_ensemble_owned,
        validate_non_empty, validate_not_join_kind, validate_pool_exists,
        validate_pool_member_removable, validate_pool_not_consumed, validate_pool_reorder,
        validate_pool_reorder_locking, validate_spec_deletable, validate_spec_exists,
        validate_spec_set_status_target, validate_spec_status, validate_spec_workdir,
        BuiltEnsembleUnit, EnsembleMemberParams, EnsembleUnitSpec, TaskTriggerHandler,
        MISSING_SYNC_IDENTITY_MESSAGE,
    };
    use crate::daemon::params::{
        LoopCompletionHookParams, LoopCopyEnsembleParams, LoopCopyNodeParams, LoopRunParams,
        LoopScheduleAutorunParams, LoopScheduleContinueParams, LoopTriggerParams,
        PoolAddSpecParams, PoolCreateParams, PoolListParams, QueueAddSpecParams, QueueCreateParams,
        QueueListParams,
    };
    use crate::db::Database;
    use crate::domain::blueprints::Blueprint;
    use crate::domain::loops::{
        Ensemble, EnsembleMember, Loop, LoopEdge, LoopEdgeCondition, LoopNode, LoopNodeKind,
        LoopNodeRun, LoopRunStatus, LoopSpec, LoopSpecStatus, LoopStatus,
    };
    use crate::domain::models::Trigger;
    use crate::domain::pools::Pool;
    use crate::shared::sync_identity::CANOPY_AGENT_ID_HEADER;
    use tempfile::tempdir;

    fn ensemble_member_params(platform: &str) -> EnsembleMemberParams {
        EnsembleMemberParams {
            platform: platform.to_string(),
            model: None,
        }
    }

    #[test]
    fn validate_ensemble_members_rejects_below_minimum() {
        let members = vec![ensemble_member_params("claude")];
        let err = validate_ensemble_members(&members).unwrap_err();
        assert!(err.contains("2-8 members"), "{err}");
    }

    #[test]
    fn validate_ensemble_members_rejects_above_maximum() {
        let members: Vec<_> = (0..9).map(|_| ensemble_member_params("claude")).collect();
        let err = validate_ensemble_members(&members).unwrap_err();
        assert!(err.contains("2-8 members"), "{err}");
    }

    #[test]
    fn validate_ensemble_members_accepts_boundary_counts() {
        let two: Vec<_> = (0..2).map(|_| ensemble_member_params("claude")).collect();
        assert!(validate_ensemble_members(&two).is_ok());
        let eight: Vec<_> = (0..8).map(|_| ensemble_member_params("claude")).collect();
        assert!(validate_ensemble_members(&eight).is_ok());
    }

    #[test]
    fn validate_ensemble_members_rejects_empty_platform() {
        let members = vec![
            ensemble_member_params("claude"),
            ensemble_member_params("  "),
        ];
        let err = validate_ensemble_members(&members).unwrap_err();
        assert!(err.contains("platform"), "{err}");
    }

    /// F1's "no nested ensembles" rule: wiring into a node that already
    /// belongs to another ensemble (as a member or as its join) must be
    /// rejected, not silently accepted.
    #[test]
    fn validate_node_not_ensemble_owned_rejects_member_and_join_nodes() {
        let dir = tempdir().unwrap();
        let db = Database::new(&dir.path().join("test.db")).unwrap();
        db.insert_loop_spec(&LoopSpec {
            id: "spec-1".to_string(),
            loop_id: None,
            name: "spec-1".to_string(),
            description: None,
            position: 1,
            parallelizable: false,
            status: LoopSpecStatus::Pending,
            started_at: None,
            completed_at: None,
            spec_start_head: None,
            workdir: None,
            completed_via: None,
            completed_via_reason: None,
            completed_via_at: None,
        })
        .unwrap();
        let now = chrono::Utc::now();
        db.insert_loop_node(&LoopNode {
            id: "kickoff".to_string(),
            spec_id: Some("spec-1".to_string()),
            loop_id: None,
            name: "kickoff".to_string(),
            kind: LoopNodeKind::Check,
            config: serde_json::json!({"command": "true", "success_condition": "exit_code_0"}),
            position: 1,
            created_at: now,
        })
        .unwrap();
        db.insert_loop_node(&LoopNode {
            id: "arbiter".to_string(),
            spec_id: Some("spec-1".to_string()),
            loop_id: None,
            name: "arbiter".to_string(),
            kind: LoopNodeKind::Agent,
            config: serde_json::json!({}),
            position: 10,
            created_at: now,
        })
        .unwrap();
        let member_nodes = vec![
            LoopNode {
                id: "m1".to_string(),
                spec_id: Some("spec-1".to_string()),
                loop_id: None,
                name: "member-1".to_string(),
                kind: LoopNodeKind::Agent,
                config: serde_json::json!({"platform": "claude"}),
                position: 2,
                created_at: now,
            },
            LoopNode {
                id: "m2".to_string(),
                spec_id: Some("spec-1".to_string()),
                loop_id: None,
                name: "member-2".to_string(),
                kind: LoopNodeKind::Agent,
                config: serde_json::json!({"platform": "codex"}),
                position: 3,
                created_at: now,
            },
        ];
        let join_node = LoopNode {
            id: "join1".to_string(),
            spec_id: Some("spec-1".to_string()),
            loop_id: None,
            name: "quorum".to_string(),
            kind: LoopNodeKind::Join,
            config: serde_json::json!({"ensemble_id": "ens1"}),
            position: 4,
            created_at: now,
        };
        let edges = vec![
            LoopEdge {
                id: "kickoff->m1".to_string(),
                spec_id: Some("spec-1".to_string()),
                loop_id: None,
                from_node: "kickoff".to_string(),
                to_node: "m1".to_string(),
                condition: LoopEdgeCondition::Always,
            },
            LoopEdge {
                id: "kickoff->m2".to_string(),
                spec_id: Some("spec-1".to_string()),
                loop_id: None,
                from_node: "kickoff".to_string(),
                to_node: "m2".to_string(),
                condition: LoopEdgeCondition::Always,
            },
            LoopEdge {
                id: "m1->join1".to_string(),
                spec_id: Some("spec-1".to_string()),
                loop_id: None,
                from_node: "m1".to_string(),
                to_node: "join1".to_string(),
                condition: LoopEdgeCondition::Always,
            },
            LoopEdge {
                id: "m2->join1".to_string(),
                spec_id: Some("spec-1".to_string()),
                loop_id: None,
                from_node: "m2".to_string(),
                to_node: "join1".to_string(),
                condition: LoopEdgeCondition::Always,
            },
            LoopEdge {
                id: "join1->arbiter".to_string(),
                spec_id: Some("spec-1".to_string()),
                loop_id: None,
                from_node: "join1".to_string(),
                to_node: "arbiter".to_string(),
                condition: LoopEdgeCondition::Pass,
            },
        ];
        let ensemble = Ensemble {
            id: "ens1".to_string(),
            spec_id: Some("spec-1".to_string()),
            loop_id: None,
            name: "Proposers".to_string(),
            prompt_template: "draft it".to_string(),
            join_node_id: "join1".to_string(),
            entry_from_node: "kickoff".to_string(),
            entry_condition: LoopEdgeCondition::Always,
            min_pass: 2,
            straggler_timeout_minutes: None,
            timeout_minutes: 30,
            on_pass_to: "arbiter".to_string(),
            on_fail_to: None,
            created_at: now,
        };
        let members = vec![
            EnsembleMember {
                ensemble_id: "ens1".to_string(),
                node_id: "m1".to_string(),
                position: 0,
                platform: "claude".to_string(),
                model: None,
            },
            EnsembleMember {
                ensemble_id: "ens1".to_string(),
                node_id: "m2".to_string(),
                position: 1,
                platform: "codex".to_string(),
                model: None,
            },
        ];
        db.insert_ensemble_unit(&ensemble, &members, &member_nodes, &join_node, &edges)
            .unwrap();

        assert!(validate_node_not_ensemble_owned(&db, "m1").is_err());
        assert!(validate_node_not_ensemble_owned(&db, "join1").is_err());
        assert!(validate_node_not_ensemble_owned(&db, "kickoff").is_ok());
        assert!(validate_node_not_ensemble_owned(&db, "arbiter").is_ok());
    }

    fn spec_with_status(
        loop_id: &str,
        id: &str,
        position: i64,
        status: LoopSpecStatus,
    ) -> LoopSpec {
        LoopSpec {
            id: id.to_string(),
            loop_id: Some(loop_id.to_string()),
            name: id.to_string(),
            description: None,
            position,
            parallelizable: false,
            status,
            started_at: None,
            completed_at: Some(chrono::Utc::now()),
            spec_start_head: None,
            workdir: None,
            completed_via: None,
            completed_via_reason: None,
            completed_via_at: None,
        }
    }

    #[test]
    fn spec_delete_refuses_loop_bound_spec_with_actionable_error() {
        let spec = spec_with_status("loop-1", "spec-1", 1, LoopSpecStatus::Pending);

        let error = validate_spec_deletable(&spec).unwrap_err();

        assert!(
            error.contains("spec-1"),
            "error should name the spec: {error}"
        );
        assert!(
            error.contains("loop-1"),
            "error should name the loop: {error}"
        );
        assert!(
            error.contains("remove it from the loop") || error.contains("delete the loop"),
            "error should be actionable: {error}"
        );
    }

    #[test]
    fn spec_delete_allows_standalone_spec() {
        let mut spec = spec_with_status("loop-1", "spec-1", 1, LoopSpecStatus::Pending);
        spec.loop_id = None;

        assert!(validate_spec_deletable(&spec).is_ok());
    }

    fn loop_reset_fixture(status: LoopStatus) -> (tempfile::TempDir, Database, String) {
        let dir = tempdir().unwrap();
        let db = Database::new(&dir.path().join("test.db")).unwrap();
        let loop_id = "loop-reset-test".to_string();
        db.insert_loop(&Loop {
            id: loop_id.clone(),
            name: "Loop".to_string(),
            description: None,
            workdir: dir.path().to_string_lossy().to_string(),
            status,
            trigger: None,
            created_at: chrono::Utc::now(),
            started_at: None,
            completed_at: Some(chrono::Utc::now()),
            autorun_at: None,
            auto_continue_at: None,
            auto_continue_action: None,
            active_run_pool_id: None,
            on_completed: None,
        })
        .unwrap();
        (dir, db, loop_id)
    }

    #[test]
    fn loop_reset_failed_loop_resets_incomplete_specs_to_pending() {
        let (_dir, db, loop_id) = loop_reset_fixture(LoopStatus::Failed);
        db.insert_loop_spec(&spec_with_status(
            &loop_id,
            "spec-done",
            1,
            LoopSpecStatus::Completed,
        ))
        .unwrap();
        db.insert_loop_spec(&spec_with_status(
            &loop_id,
            "spec-failed",
            2,
            LoopSpecStatus::Failed,
        ))
        .unwrap();

        let result = perform_loop_reset(&db, &loop_id, None).unwrap();
        assert!(!result.is_error.unwrap_or(false));

        let lp = db.get_loop(&loop_id).unwrap().unwrap();
        assert_eq!(lp.status, LoopStatus::Draft);
        assert!(lp.completed_at.is_none());

        let specs = db.list_loop_specs(&loop_id).unwrap();
        let done = specs.iter().find(|s| s.id == "spec-done").unwrap();
        let failed = specs.iter().find(|s| s.id == "spec-failed").unwrap();
        assert_eq!(failed.status, LoopSpecStatus::Pending);
        assert!(failed.completed_at.is_none());
        // Completed specs are preserved when `specs` isn't given explicitly.
        assert_eq!(done.status, LoopSpecStatus::Completed);
    }

    #[test]
    fn loop_reset_with_explicit_specs_resets_completed_spec_too() {
        let (_dir, db, loop_id) = loop_reset_fixture(LoopStatus::Failed);
        db.insert_loop_spec(&spec_with_status(
            &loop_id,
            "spec-done",
            1,
            LoopSpecStatus::Completed,
        ))
        .unwrap();

        let result = perform_loop_reset(&db, &loop_id, Some(&["spec-done".to_string()])).unwrap();
        assert!(!result.is_error.unwrap_or(false));

        let spec = db.get_loop_spec("spec-done").unwrap().unwrap();
        assert_eq!(spec.status, LoopSpecStatus::Pending);
        assert!(spec.completed_at.is_none());
    }

    /// A loop whose last run was against a pool has empty (or irrelevant)
    /// bound specs — the pool's *members* are what actually need resetting.
    /// `loop_reset` must find them via the loop's persisted
    /// `active_run_pool_id`, reset every non-completed one back to pending
    /// (completed members untouched), and report the real count — not "0
    /// spec(s) reset", the false report from the incident this spec fixes.
    #[test]
    fn loop_reset_pool_run_resets_pending_pool_members_and_reports_count() {
        let dir = tempdir().unwrap();
        let db = Database::new(&dir.path().join("test.db")).unwrap();
        let loop_id = "loop-pool-reset-test".to_string();
        db.insert_loop(&Loop {
            id: loop_id.clone(),
            name: "Loop".to_string(),
            description: None,
            workdir: dir.path().to_string_lossy().to_string(),
            status: LoopStatus::Failed,
            trigger: None,
            created_at: chrono::Utc::now(),
            started_at: None,
            completed_at: Some(chrono::Utc::now()),
            autorun_at: None,
            auto_continue_at: None,
            auto_continue_action: None,
            active_run_pool_id: Some("pool-1".to_string()),
            on_completed: None,
        })
        .unwrap();

        let standalone = |id: &str, position: i64, status: LoopSpecStatus| LoopSpec {
            id: id.to_string(),
            loop_id: None,
            name: id.to_string(),
            description: None,
            position,
            parallelizable: false,
            status,
            started_at: None,
            completed_at: Some(chrono::Utc::now()),
            spec_start_head: None,
            workdir: None,
            completed_via: None,
            completed_via_reason: None,
            completed_via_at: None,
        };
        db.insert_loop_spec(&standalone("pool-done", 1, LoopSpecStatus::Completed))
            .unwrap();
        db.insert_loop_spec(&standalone("pool-failed", 2, LoopSpecStatus::Failed))
            .unwrap();
        db.insert_loop_spec(&standalone("pool-pending", 3, LoopSpecStatus::Pending))
            .unwrap();
        db.insert_pool(&Pool {
            id: "pool-1".to_string(),
            name: "pool-1".to_string(),
            created_at: chrono::Utc::now(),
        })
        .unwrap();
        for spec_id in ["pool-done", "pool-failed", "pool-pending"] {
            db.append_pool_member("pool-1", spec_id, None).unwrap();
        }

        let result = perform_loop_reset(&db, &loop_id, None).unwrap();
        assert!(!result.is_error.unwrap_or(false));
        let text = format!("{:?}", result.content);
        assert!(text.contains("2 spec(s) reset"), "{text}");

        let lp = db.get_loop(&loop_id).unwrap().unwrap();
        assert_eq!(lp.status, LoopStatus::Draft);

        let done = db.get_loop_spec("pool-done").unwrap().unwrap();
        let failed = db.get_loop_spec("pool-failed").unwrap().unwrap();
        let pending = db.get_loop_spec("pool-pending").unwrap().unwrap();
        assert_eq!(
            done.status,
            LoopSpecStatus::Completed,
            "completed pool member must be left untouched"
        );
        assert_eq!(failed.status, LoopSpecStatus::Pending);
        assert!(failed.completed_at.is_none());
        assert_eq!(pending.status, LoopSpecStatus::Pending);
    }

    #[test]
    fn loop_reset_rejects_running_loop_with_actionable_error() {
        let (_dir, db, loop_id) = loop_reset_fixture(LoopStatus::Running);

        let result = perform_loop_reset(&db, &loop_id, None).unwrap();
        assert!(result.is_error.unwrap_or(false));
        let text = format!("{:?}", result.content);
        assert!(text.contains("loop_pause"), "{text}");

        // Status untouched.
        let lp = db.get_loop(&loop_id).unwrap().unwrap();
        assert_eq!(lp.status, LoopStatus::Running);
    }

    #[test]
    fn loop_reset_missing_loop_returns_not_found_error() {
        let dir = tempdir().unwrap();
        let db = Database::new(&dir.path().join("test.db")).unwrap();

        let result = perform_loop_reset(&db, "does-not-exist", None).unwrap();
        assert!(result.is_error.unwrap_or(false));
        let text = format!("{:?}", result.content);
        assert!(text.contains("not found"), "{text}");
    }

    /// `loop_run` must keep refusing a `failed` loop, and the refusal must
    /// point at both sanctioned ways out: `loop_reset` (manual) and
    /// `loop_schedule_autorun` (self-service, for a quota-failed loop).
    #[test]
    fn loop_run_status_guard_rejects_failed_loop_and_names_both_ways_out() {
        let err = loop_run_status_guard("loop-1", LoopStatus::Failed).unwrap_err();
        assert!(err.contains("loop_reset"), "{err}");
        assert!(err.contains("loop_schedule_autorun"), "{err}");
    }

    #[test]
    fn loop_run_status_guard_rejects_completed_loop() {
        let err = loop_run_status_guard("loop-1", LoopStatus::Completed).unwrap_err();
        assert!(err.contains("loop_reset"), "{err}");
    }

    #[test]
    fn loop_run_status_guard_rejects_running_loop_with_loop_id() {
        let err = loop_run_status_guard("loop-1", LoopStatus::Running).unwrap_err();
        assert!(err.contains("loop-1"), "{err}");
        assert!(err.contains("already running"), "{err}");
    }

    #[test]
    fn loop_run_status_guard_accepts_draft_and_paused_loops() {
        assert!(loop_run_status_guard("loop-1", LoopStatus::Draft).is_ok());
        assert!(loop_run_status_guard("loop-1", LoopStatus::Paused).is_ok());
    }

    #[test]
    fn validate_node_config_rejects_double_encoded_string() {
        let config = serde_json::json!("{\"platform\": \"mimo\"}");
        let error = validate_node_config(LoopNodeKind::Agent, &config).unwrap_err();
        assert!(error.contains("must be a JSON object"), "{error}");
    }

    #[test]
    fn validate_node_config_agent_requires_platform_or_cli() {
        let config = serde_json::json!({ "model": "opus" });
        let error = validate_node_config(LoopNodeKind::Agent, &config).unwrap_err();
        assert!(error.contains("'agent'"), "{error}");
        assert!(error.contains("platform"), "{error}");
    }

    #[test]
    fn validate_node_config_check_requires_command() {
        let config = serde_json::json!({ "timeout_seconds": 30 });
        let error = validate_node_config(LoopNodeKind::Check, &config).unwrap_err();
        assert!(error.contains("'check'"), "{error}");
        assert!(error.contains("command"), "{error}");
    }

    #[test]
    fn validate_node_config_gate_requires_value_for_output_contains() {
        let config = serde_json::json!({ "evaluate": "output_contains" });
        let error = validate_node_config(LoopNodeKind::Gate, &config).unwrap_err();
        assert!(error.contains("'gate'"), "{error}");
        assert!(error.contains("value"), "{error}");

        let config_without_evaluate = serde_json::json!({});
        let error = validate_node_config(LoopNodeKind::Gate, &config_without_evaluate).unwrap_err();
        assert!(error.contains("value"), "{error}");
    }

    #[test]
    fn validate_node_config_accepts_valid_configs() {
        assert!(validate_node_config(
            LoopNodeKind::Agent,
            &serde_json::json!({ "platform": "claude" })
        )
        .is_ok());
        assert!(
            validate_node_config(LoopNodeKind::Agent, &serde_json::json!({ "cli": "codex" }))
                .is_ok()
        );
        assert!(validate_node_config(
            LoopNodeKind::Check,
            &serde_json::json!({ "command": "cargo test" })
        )
        .is_ok());
        assert!(validate_node_config(
            LoopNodeKind::Gate,
            &serde_json::json!({ "evaluate": "output_contains", "value": "ok" })
        )
        .is_ok());
    }

    #[test]
    fn blueprints_are_listed_after_a_fresh_startup_and_reseeding_is_idempotent() {
        let dir = tempdir().unwrap();
        let db = Database::new(&dir.path().join("test.db")).unwrap();

        let names_after_first_start: Vec<String> = db
            .list_blueprints()
            .unwrap()
            .into_iter()
            .map(|b| b.name)
            .collect();
        for expected in [
            "implementer-claude",
            "cargo-gates",
            "reviewer-committer-mimo",
            "commit-check",
            "resilience-mimo",
        ] {
            assert!(
                names_after_first_start.contains(&expected.to_string()),
                "expected builtin '{expected}' after fresh startup, got {names_after_first_start:?}"
            );
        }

        // Simulate a second daemon startup against the same database.
        db.seed_builtin_blueprints().unwrap();
        let names_after_second_start: Vec<String> = db
            .list_blueprints()
            .unwrap()
            .into_iter()
            .map(|b| b.name)
            .collect();
        assert_eq!(
            names_after_first_start.len(),
            names_after_second_start.len(),
            "reseeding must not duplicate builtins"
        );
    }

    #[test]
    fn custom_blueprint_create_list_delete_round_trip_and_builtin_delete_refused() {
        let dir = tempdir().unwrap();
        let db = Database::new(&dir.path().join("test.db")).unwrap();

        let custom = Blueprint {
            id: uuid::Uuid::new_v4().to_string(),
            name: "tdd-implementer".to_string(),
            kind: LoopNodeKind::Agent,
            config: serde_json::json!({ "platform": "claude", "prompt": "write a failing test first" }),
            builtin: false,
            created_at: chrono::Utc::now(),
        };
        db.insert_blueprint(&custom).unwrap();

        let names: Vec<String> = db
            .list_blueprints()
            .unwrap()
            .into_iter()
            .map(|b| b.name)
            .collect();
        assert!(names.contains(&"tdd-implementer".to_string()));

        let removed = db.delete_blueprint_by_name("tdd-implementer").unwrap();
        assert!(removed);
        assert!(db
            .get_blueprint_by_name("tdd-implementer")
            .unwrap()
            .is_none());

        // Deleting a builtin is refused at the validation layer with an
        // actionable message, before ever touching the DB.
        let builtin = db
            .get_blueprint_by_name("implementer-claude")
            .unwrap()
            .expect("builtin should exist");
        let error = super::validate_blueprint_deletable(&builtin).unwrap_err();
        assert!(error.contains("implementer-claude"));
        assert!(error.contains("cannot be deleted"));
    }

    #[test]
    fn loop_add_node_from_blueprint_with_override_merges_config_and_override_wins() {
        let dir = tempdir().unwrap();
        let db = Database::new(&dir.path().join("test.db")).unwrap();

        let mut overrides = serde_json::Map::new();
        overrides.insert(
            "prompt".to_string(),
            serde_json::json!("custom overridden prompt"),
        );

        let (kind, config) = resolve_node_kind_and_config(
            &db,
            None,
            None,
            Some("implementer-claude"),
            Some(overrides),
        )
        .expect("blueprint resolution should succeed");

        assert_eq!(kind, LoopNodeKind::Agent);
        assert_eq!(config["prompt"], "custom overridden prompt");
        // Other templated keys (e.g. platform) survive the shallow merge.
        assert_eq!(config["platform"], "claude");
    }

    #[test]
    fn loop_add_node_with_unknown_blueprint_lists_available_names() {
        let dir = tempdir().unwrap();
        let db = Database::new(&dir.path().join("test.db")).unwrap();

        let error = validate_blueprint_exists(&db, "does-not-exist").unwrap_err();

        assert!(error.contains("does-not-exist"));
        assert!(error.contains("implementer-claude"));
        assert!(error.contains("cargo-gates"));
        assert!(error.contains("reviewer-committer-mimo"));
        assert!(error.contains("commit-check"));
        assert!(error.contains("resilience-mimo"));
    }

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

    fn standalone_spec(id: &str) -> LoopSpec {
        LoopSpec {
            id: id.to_string(),
            loop_id: None,
            name: id.to_string(),
            description: None,
            position: 0,
            parallelizable: false,
            status: LoopSpecStatus::Pending,
            started_at: None,
            completed_at: None,
            spec_start_head: None,
            workdir: None,
            completed_via: None,
            completed_via_reason: None,
            completed_via_at: None,
        }
    }

    fn pool_test_db() -> (tempfile::TempDir, Database) {
        let dir = tempdir().unwrap();
        let db = Database::new(&dir.path().join("test.db")).unwrap();
        (dir, db)
    }

    fn insert_pool(db: &Database, id: &str) {
        db.insert_pool(&Pool {
            id: id.to_string(),
            name: format!("{id}-name"),
            created_at: chrono::Utc::now(),
        })
        .unwrap();
    }

    /// Build a fully-wired `TaskTriggerHandler` over an in-memory-ish temp DB
    /// so the queue/pool `#[tool]` methods (and their shared `do_queue_*`
    /// helpers) can be exercised end-to-end.
    fn queue_test_handler() -> (
        tempfile::TempDir,
        std::sync::Arc<Database>,
        TaskTriggerHandler,
    ) {
        use crate::application::notification_service::{
            DefaultNotificationService, NotificationService,
        };
        use crate::executor::Executor;
        use crate::loop_engine::LoopEngine;
        use crate::rag::ingestion::IngestionManager;
        use crate::sync_manager::SyncManager;
        use crate::watchers::WatcherEngine;
        use std::sync::Arc;
        use tokio::sync::Notify;

        let dir = tempdir().unwrap();
        let db = Arc::new(Database::new(&dir.path().join("test.db")).unwrap());
        let notif: Arc<dyn NotificationService> = Arc::new(DefaultNotificationService);
        let executor = Arc::new(Executor::new(Arc::clone(&db), Arc::clone(&notif)));
        let sync_manager = Arc::new(SyncManager::new(Arc::clone(&db)));
        let loop_engine = Arc::new(LoopEngine::new(Arc::clone(&db), Arc::clone(&notif)));
        let watcher_engine = Arc::new(WatcherEngine::new(
            Arc::clone(&db),
            Arc::clone(&executor),
            Arc::clone(&loop_engine),
        ));
        let ingestion = Arc::new(IngestionManager::new(
            Arc::clone(&db),
            dir.path().to_path_buf(),
        ));
        let dynamic_skills = Arc::new(crate::dynamic_skills::SkillStore::new(
            dir.path().join("skills"),
            Vec::new(),
            15,
        ));
        let handler = TaskTriggerHandler::new(
            Arc::clone(&db),
            executor,
            watcher_engine,
            Arc::new(Notify::new()),
            loop_engine,
            notif,
            sync_manager,
            ingestion,
            dynamic_skills,
            0,
        );
        (dir, db, handler)
    }

    fn result_text(result: &rmcp::model::CallToolResult) -> String {
        format!("{:?}", result.content)
    }

    /// Q1: the primary `queue_create` and the deprecated `pool_create` alias
    /// must both route to the same `do_queue_create` helper, both emit the
    /// `queue_id` result key (never the old `pool_id`), and both persist a real
    /// queue row.
    #[tokio::test]
    async fn queue_create_and_pool_alias_route_to_same_handler() {
        use rmcp::handler::server::wrapper::Parameters;

        let (_dir, db, handler) = queue_test_handler();

        let via_queue = handler
            .queue_create(Parameters(QueueCreateParams {
                name: "Primary".to_string(),
            }))
            .await
            .unwrap();
        let via_pool = handler
            .pool_create(Parameters(PoolCreateParams {
                name: "Alias".to_string(),
            }))
            .await
            .unwrap();

        for text in [result_text(&via_queue), result_text(&via_pool)] {
            assert!(text.contains("queue_id"), "expected queue_id key: {text}");
            assert!(
                !text.contains("pool_id"),
                "result must not leak pool_id: {text}"
            );
        }

        let pools = db.list_pools().unwrap();
        assert!(pools.iter().any(|p| p.name == "Primary"));
        assert!(pools.iter().any(|p| p.name == "Alias"));
    }

    /// Q1: `queue_add_spec` and its `pool_add_spec` alias share one handler —
    /// adding via either path lands the spec in the same underlying queue and
    /// returns queue-worded confirmation.
    #[tokio::test]
    async fn queue_add_spec_and_pool_alias_are_equivalent() {
        use rmcp::handler::server::wrapper::Parameters;

        let (_dir, db, handler) = queue_test_handler();
        for id in ["spec-a", "spec-b"] {
            db.insert_loop_spec(&standalone_spec(id)).unwrap();
        }
        insert_pool(&db, "queue-1");

        let via_queue = handler
            .queue_add_spec(Parameters(QueueAddSpecParams {
                queue_id: "queue-1".to_string(),
                spec_id: "spec-a".to_string(),
                group: None,
            }))
            .await
            .unwrap();
        let via_pool = handler
            .pool_add_spec(Parameters(PoolAddSpecParams {
                pool_id: "queue-1".to_string(),
                spec_id: "spec-b".to_string(),
                group: None,
            }))
            .await
            .unwrap();

        assert!(result_text(&via_queue).contains("added to queue"));
        assert!(result_text(&via_pool).contains("added to queue"));
        assert_eq!(
            db.list_pool_member_spec_ids("queue-1").unwrap(),
            vec!["spec-a", "spec-b"]
        );
    }

    /// Q1: `queue_list` emits the queue-worded payload keys (`queue`/`queues`),
    /// and the `pool_list` alias produces the identical payload.
    #[tokio::test]
    async fn queue_list_and_pool_alias_use_queue_keys() {
        use rmcp::handler::server::wrapper::Parameters;

        let (_dir, db, handler) = queue_test_handler();
        db.insert_loop_spec(&standalone_spec("spec-a")).unwrap();
        insert_pool(&db, "queue-1");
        db.append_pool_member("queue-1", "spec-a", None).unwrap();

        let all_via_queue = handler
            .queue_list(Parameters(QueueListParams { queue_id: None }))
            .await
            .unwrap();
        let all_via_pool = handler
            .pool_list(Parameters(PoolListParams { pool_id: None }))
            .await
            .unwrap();
        assert_eq!(result_text(&all_via_queue), result_text(&all_via_pool));
        assert!(result_text(&all_via_queue).contains("queues"));

        let one = handler
            .queue_list(Parameters(QueueListParams {
                queue_id: Some("queue-1".to_string()),
            }))
            .await
            .unwrap();
        let text = result_text(&one);
        assert!(text.contains("queue"), "{text}");
        assert!(
            !text.contains("\\\"pool\\\""),
            "must not use pool key: {text}"
        );
    }

    /// Q1: `loop_run` accepts the deprecated `pool_id` as an alias for
    /// `queue_id`. Routing an empty queue through it surfaces the (queue-worded)
    /// empty-launch error naming that queue — proof the id resolved.
    #[tokio::test]
    async fn loop_run_accepts_pool_id_alias() {
        use rmcp::handler::server::wrapper::Parameters;

        let (dir, db, handler) = queue_test_handler();
        db.insert_loop(&Loop {
            id: "loop-1".to_string(),
            name: "loop-1".to_string(),
            description: None,
            workdir: dir.path().to_string_lossy().to_string(),
            status: LoopStatus::Draft,
            trigger: None,
            created_at: chrono::Utc::now(),
            started_at: None,
            completed_at: None,
            autorun_at: None,
            auto_continue_at: None,
            auto_continue_action: None,
            active_run_pool_id: None,
            on_completed: None,
        })
        .unwrap();
        insert_pool(&db, "queue-empty");

        let result = handler
            .loop_run(Parameters(LoopRunParams {
                loop_id: "loop-1".to_string(),
                queue_id: None,
                pool_id: Some("queue-empty".to_string()),
                workdir: None,
            }))
            .await
            .unwrap();
        let text = result_text(&result);
        assert!(text.contains("queue-empty"), "pool_id must resolve: {text}");
    }

    /// Q1: when both `queue_id` and `pool_id` are set, `queue_id` wins.
    #[tokio::test]
    async fn loop_run_prefers_queue_id_over_pool_id() {
        use rmcp::handler::server::wrapper::Parameters;

        let (dir, db, handler) = queue_test_handler();
        db.insert_loop(&Loop {
            id: "loop-1".to_string(),
            name: "loop-1".to_string(),
            description: None,
            workdir: dir.path().to_string_lossy().to_string(),
            status: LoopStatus::Draft,
            trigger: None,
            created_at: chrono::Utc::now(),
            started_at: None,
            completed_at: None,
            autorun_at: None,
            auto_continue_at: None,
            auto_continue_action: None,
            active_run_pool_id: None,
            on_completed: None,
        })
        .unwrap();
        insert_pool(&db, "queue-win");
        insert_pool(&db, "queue-lose");

        let result = handler
            .loop_run(Parameters(LoopRunParams {
                loop_id: "loop-1".to_string(),
                queue_id: Some("queue-win".to_string()),
                pool_id: Some("queue-lose".to_string()),
                workdir: None,
            }))
            .await
            .unwrap();
        let text = result_text(&result);
        assert!(text.contains("queue-win"), "queue_id must win: {text}");
        assert!(!text.contains("queue-lose"), "pool_id must lose: {text}");
    }

    #[test]
    fn pool_crud_and_ordering_round_trips() {
        let (_dir, db) = pool_test_db();
        for id in ["spec-a", "spec-b", "spec-c"] {
            db.insert_loop_spec(&standalone_spec(id)).unwrap();
        }
        insert_pool(&db, "pool-1");

        db.append_pool_member("pool-1", "spec-a", None).unwrap();
        db.append_pool_member("pool-1", "spec-b", None).unwrap();
        db.append_pool_member("pool-1", "spec-c", None).unwrap();

        assert_eq!(
            db.list_pool_member_spec_ids("pool-1").unwrap(),
            vec!["spec-a", "spec-b", "spec-c"]
        );

        let details = db.get_pool_details("pool-1").unwrap().unwrap();
        assert_eq!(details.pool.id, "pool-1");
        assert_eq!(
            details
                .members
                .iter()
                .map(|spec| spec.id.clone())
                .collect::<Vec<_>>(),
            vec!["spec-a", "spec-b", "spec-c"]
        );

        assert!(db.remove_pool_member("pool-1", "spec-b").unwrap());
        assert_eq!(
            db.list_pool_member_spec_ids("pool-1").unwrap(),
            vec!["spec-a", "spec-c"]
        );
        assert!(!db.remove_pool_member("pool-1", "spec-b").unwrap());

        assert!(db.list_pools().unwrap().iter().any(|p| p.id == "pool-1"));
    }

    #[test]
    fn pool_reorder_is_total_and_deterministic() {
        let (_dir, db) = pool_test_db();
        for id in ["spec-a", "spec-b", "spec-c"] {
            db.insert_loop_spec(&standalone_spec(id)).unwrap();
        }
        insert_pool(&db, "pool-1");
        for id in ["spec-a", "spec-b", "spec-c"] {
            db.append_pool_member("pool-1", id, None).unwrap();
        }

        let current = db.list_pool_member_spec_ids("pool-1").unwrap();
        let order = vec![
            "spec-c".to_string(),
            "spec-a".to_string(),
            "spec-b".to_string(),
        ];
        assert!(validate_pool_reorder(&current, &order).is_ok());

        db.reorder_pool_members("pool-1", &order).unwrap();
        assert_eq!(db.list_pool_member_spec_ids("pool-1").unwrap(), order);
    }

    #[test]
    fn pool_reorder_rejects_partial_list() {
        let current = vec![
            "spec-a".to_string(),
            "spec-b".to_string(),
            "spec-c".to_string(),
        ];
        let order = vec!["spec-a".to_string(), "spec-b".to_string()];

        let error = validate_pool_reorder(&current, &order).unwrap_err();
        assert!(error.contains("exactly once; got 2"), "{error}");
    }

    #[test]
    fn pool_reorder_rejects_unknown_spec() {
        let current = vec![
            "spec-a".to_string(),
            "spec-b".to_string(),
            "spec-c".to_string(),
        ];
        let order = vec![
            "spec-a".to_string(),
            "spec-b".to_string(),
            "ghost".to_string(),
        ];

        let error = validate_pool_reorder(&current, &order).unwrap_err();
        assert!(error.contains("no spec 'ghost'"), "{error}");
    }

    fn running_spec(id: &str) -> LoopSpec {
        let mut spec = standalone_spec(id);
        spec.status = LoopSpecStatus::Running;
        spec
    }

    /// A minimal loop row, needed only to satisfy `loop_runs.loop_id`'s FK.
    fn insert_test_loop(db: &Database, id: &str) {
        db.insert_loop(&Loop {
            id: id.to_string(),
            name: id.to_string(),
            description: None,
            workdir: "/tmp".to_string(),
            status: LoopStatus::Running,
            trigger: None,
            created_at: chrono::Utc::now(),
            started_at: None,
            completed_at: None,
            autorun_at: None,
            auto_continue_at: None,
            auto_continue_action: None,
            active_run_pool_id: None,
            on_completed: None,
        })
        .unwrap();
    }

    /// A minimal node row, needed only to satisfy `loop_runs.node_id`'s FK.
    fn insert_test_node(db: &Database, id: &str, spec_id: &str) {
        db.insert_loop_node(&LoopNode {
            id: id.to_string(),
            spec_id: Some(spec_id.to_string()),
            loop_id: None,
            name: id.to_string(),
            kind: LoopNodeKind::Check,
            config: serde_json::json!({"command": "true"}),
            position: 1,
            created_at: chrono::Utc::now(),
        })
        .unwrap();
    }

    fn loop_run_row(id: &str, loop_id: &str, spec_id: &str, status: LoopRunStatus) -> LoopNodeRun {
        LoopNodeRun {
            id: id.to_string(),
            loop_id: loop_id.to_string(),
            spec_id: spec_id.to_string(),
            node_id: "node-1".to_string(),
            status,
            input: None,
            output: None,
            started_at: chrono::Utc::now(),
            completed_at: (status != LoopRunStatus::Running).then(chrono::Utc::now),
            iteration: 1,
            pid: None,
            boot_id: None,
            session_id: None,
        }
    }

    // ── B12: stale loop_complete_node/loop_report_blocker reports ─────

    fn reported_run_fixture() -> (tempfile::TempDir, Database) {
        let dir = tempdir().unwrap();
        let db = Database::new(&dir.path().join("test.db")).unwrap();
        insert_test_loop(&db, "loop-1");
        db.insert_loop_spec(&standalone_spec("spec-a")).unwrap();
        insert_test_node(&db, "node-1", "spec-a");
        (dir, db)
    }

    #[test]
    fn resolve_reported_run_accepts_matching_active_run() {
        let (_dir, db) = reported_run_fixture();
        db.insert_loop_run(&loop_run_row(
            "run-1",
            "loop-1",
            "spec-a",
            LoopRunStatus::Running,
        ))
        .unwrap();

        let run = resolve_reported_run(&db, "run-1", "node-1")
            .unwrap()
            .expect("a running run for the claimed node_id must be accepted");
        assert_eq!(run.id, "run-1");
    }

    #[test]
    fn resolve_reported_run_rejects_unknown_run_id() {
        let (_dir, db) = reported_run_fixture();

        let result = resolve_reported_run(&db, "no-such-run", "node-1").unwrap();
        let error = result.expect_err("an unknown run_id must be rejected");
        assert!(error.is_error.unwrap_or(false));
    }

    #[test]
    fn resolve_reported_run_rejects_node_id_mismatch() {
        let (_dir, db) = reported_run_fixture();
        db.insert_loop_run(&loop_run_row(
            "run-1",
            "loop-1",
            "spec-a",
            LoopRunStatus::Running,
        ))
        .unwrap();

        // "run-1" really belongs to "node-1" (see loop_run_row) — a report
        // claiming a different node_id for the same run_id is malformed.
        let result = resolve_reported_run(&db, "run-1", "some-other-node").unwrap();
        let error = result.expect_err("a node_id that doesn't match the run must be rejected");
        assert!(error.is_error.unwrap_or(false));
    }

    /// The core B12 regression case: a node run that was already finalized
    /// (timed out, killed on pause/reset, or superseded by a retry — any of
    /// which flips its status away from `running`) must reject a late report
    /// naming its exact `run_id`, rather than that report silently landing
    /// on whatever's now active for the same `node_id`.
    #[test]
    fn resolve_reported_run_rejects_already_finalized_run() {
        let (_dir, db) = reported_run_fixture();
        // The stale run: already finalized (e.g. by the timeout/pause kill
        // path), simulating the orphaned agent's late self-report arriving
        // after the engine gave up on it.
        db.insert_loop_run(&loop_run_row(
            "run-stale",
            "loop-1",
            "spec-a",
            LoopRunStatus::Fail,
        ))
        .unwrap();
        // A newer attempt at the SAME node is now the genuinely active run —
        // exactly the run a naive node_id-only lookup would have
        // misattributed the stale report to.
        db.insert_loop_run(&loop_run_row(
            "run-current",
            "loop-1",
            "spec-a",
            LoopRunStatus::Running,
        ))
        .unwrap();

        let result = resolve_reported_run(&db, "run-stale", "node-1").unwrap();
        let error = result.expect_err("a finalized run must reject a late report");
        assert!(error.is_error.unwrap_or(false));

        // And the genuinely active run must be left completely untouched.
        let current = db.get_loop_run("run-current").unwrap().unwrap();
        assert_eq!(current.status, LoopRunStatus::Running);
    }

    #[test]
    fn pool_not_consumed_allows_start_when_every_member_is_pending() {
        let (_dir, db) = pool_test_db();
        db.insert_loop_spec(&standalone_spec("spec-a")).unwrap();
        db.insert_loop_spec(&standalone_spec("spec-b")).unwrap();
        insert_pool(&db, "pool-1");
        db.append_pool_member("pool-1", "spec-a", None).unwrap();
        db.append_pool_member("pool-1", "spec-b", None).unwrap();

        assert!(validate_pool_not_consumed(&db, "pool-1", "loop-requesting").is_ok());
    }

    #[test]
    fn pool_not_consumed_blocks_when_a_member_runs_under_another_loop() {
        let (_dir, db) = pool_test_db();
        db.insert_loop_spec(&running_spec("spec-a")).unwrap();
        insert_pool(&db, "pool-1");
        db.append_pool_member("pool-1", "spec-a", None).unwrap();
        insert_test_loop(&db, "loop-other");
        insert_test_node(&db, "node-1", "spec-a");
        db.insert_loop_run(&loop_run_row(
            "run-1",
            "loop-other",
            "spec-a",
            LoopRunStatus::Running,
        ))
        .unwrap();

        let error = validate_pool_not_consumed(&db, "pool-1", "loop-requesting").unwrap_err();

        assert!(error.contains("spec-a"), "{error}");
        assert!(error.contains("loop-other"), "{error}");
    }

    #[test]
    fn pool_not_consumed_allows_the_owning_loop_to_resume_its_own_running_spec() {
        // A paused pool run's active spec stays `running` between node
        // executions. Resuming the SAME loop against the SAME pool must not
        // be mistaken for a conflicting run.
        let (_dir, db) = pool_test_db();
        db.insert_loop_spec(&running_spec("spec-a")).unwrap();
        insert_pool(&db, "pool-1");
        db.append_pool_member("pool-1", "spec-a", None).unwrap();
        insert_test_loop(&db, "loop-owner");
        insert_test_node(&db, "node-1", "spec-a");
        db.insert_loop_run(&loop_run_row(
            "run-1",
            "loop-owner",
            "spec-a",
            LoopRunStatus::Pass,
        ))
        .unwrap();

        assert!(validate_pool_not_consumed(&db, "pool-1", "loop-owner").is_ok());
    }

    /// B18 (Requirement 2): `skip_next_spec` on a pool-driven paused loop
    /// must find its in-flight member through the loop's persisted
    /// `active_run_pool_id` — the member's own `loop_id` column stays `None`
    /// (pool membership never binds it), so `list_loop_specs(loop_id)` alone
    /// can't see it. Before this fix `handle_skip_next_spec` always errored
    /// "No running spec found" for a pool-driven pause.
    #[test]
    fn skip_next_spec_finds_and_skips_the_running_pool_member() {
        let (_dir, db) = pool_test_db();
        insert_test_loop(&db, "loop-owner");
        db.set_loop_active_run_pool("loop-owner", Some("pool-1"))
            .unwrap();

        db.insert_loop_spec(&running_spec("spec-a")).unwrap();
        db.insert_loop_spec(&standalone_spec("spec-b")).unwrap();
        insert_pool(&db, "pool-1");
        db.append_pool_member("pool-1", "spec-a", None).unwrap();
        db.append_pool_member("pool-1", "spec-b", None).unwrap();

        handle_skip_next_spec(&db, "loop-owner").unwrap();

        let spec_a = db.get_loop_spec("spec-a").unwrap().unwrap();
        let spec_b = db.get_loop_spec("spec-b").unwrap().unwrap();
        assert_eq!(spec_a.status, LoopSpecStatus::Skipped);
        assert_eq!(spec_b.status, LoopSpecStatus::Pending);
    }

    #[test]
    fn skip_next_spec_prefers_the_loop_bound_spec_over_pool_context() {
        // A loop with its own bound `running` spec must use that, even if a
        // stale `active_run_pool_id` is still sitting on the loop from an
        // earlier, unrelated pool run.
        let (_dir, db) = pool_test_db();
        insert_test_loop(&db, "loop-owner");
        db.set_loop_active_run_pool("loop-owner", Some("pool-1"))
            .unwrap();

        let mut bound = running_spec("spec-bound");
        bound.loop_id = Some("loop-owner".to_string());
        db.insert_loop_spec(&bound).unwrap();
        db.insert_loop_spec(&running_spec("spec-pool")).unwrap();
        insert_pool(&db, "pool-1");
        db.append_pool_member("pool-1", "spec-pool", None).unwrap();

        handle_skip_next_spec(&db, "loop-owner").unwrap();

        let bound_after = db.get_loop_spec("spec-bound").unwrap().unwrap();
        let pool_after = db.get_loop_spec("spec-pool").unwrap().unwrap();
        assert_eq!(bound_after.status, LoopSpecStatus::Skipped);
        assert_eq!(pool_after.status, LoopSpecStatus::Running);
    }

    #[test]
    fn skip_next_spec_errors_when_no_spec_is_running_anywhere() {
        let (_dir, db) = pool_test_db();
        insert_test_loop(&db, "loop-owner");

        let error = handle_skip_next_spec(&db, "loop-owner").unwrap_err();
        assert!(error.message.contains("No running spec found"));
    }

    /// B35: `retry_current_node` must error when no spec is running in
    /// either the loop's bound specs or its pool — same validation shape as
    /// `skip_next_spec`.
    #[test]
    fn retry_current_node_errors_when_no_running_spec() {
        let (_dir, db) = pool_test_db();
        insert_test_loop(&db, "loop-owner");
        let error = handle_retry_current_node(&db, "loop-owner").unwrap_err();
        assert!(error.message.contains("No running spec found"));
    }

    /// B35: `retry_current_node` must find the running spec through the
    /// loop's persisted `active_run_pool_id` (pool member has `loop_id: None`).
    #[test]
    fn retry_current_node_finds_running_pool_member() {
        let (_dir, db) = pool_test_db();
        insert_test_loop(&db, "loop-owner");
        db.set_loop_active_run_pool("loop-owner", Some("pool-1"))
            .unwrap();

        db.insert_loop_spec(&running_spec("spec-a")).unwrap();
        db.insert_loop_spec(&standalone_spec("spec-b")).unwrap();
        insert_pool(&db, "pool-1");
        db.append_pool_member("pool-1", "spec-a", None).unwrap();
        db.append_pool_member("pool-1", "spec-b", None).unwrap();

        // Should succeed without error — a running spec exists.
        assert!(handle_retry_current_node(&db, "loop-owner").is_ok());
    }

    #[test]
    fn pool_reorder_rejects_duplicate_spec() {
        let current = vec![
            "spec-a".to_string(),
            "spec-b".to_string(),
            "spec-c".to_string(),
        ];
        let order = vec![
            "spec-a".to_string(),
            "spec-a".to_string(),
            "spec-b".to_string(),
        ];

        let error = validate_pool_reorder(&current, &order).unwrap_err();
        assert!(error.contains("more than once"), "{error}");
    }

    #[test]
    fn pool_reorder_locking_refuses_ordering_that_moves_a_running_member() {
        // R6: the currently running spec is immutable in the pool's order.
        // Swapping it with a pending member must be refused, even though the
        // result is still a valid total permutation.
        let (_dir, db) = pool_test_db();
        db.insert_loop_spec(&running_spec("spec-a")).unwrap();
        db.insert_loop_spec(&standalone_spec("spec-b")).unwrap();
        db.insert_loop_spec(&standalone_spec("spec-c")).unwrap();
        insert_pool(&db, "pool-1");
        for id in ["spec-a", "spec-b", "spec-c"] {
            db.append_pool_member("pool-1", id, None).unwrap();
        }
        let current = db.list_pool_member_spec_ids("pool-1").unwrap();

        // Moves spec-a (running) from position 0 to position 1.
        let order = vec![
            "spec-b".to_string(),
            "spec-a".to_string(),
            "spec-c".to_string(),
        ];
        assert!(validate_pool_reorder(&current, &order).is_ok());

        let error = validate_pool_reorder_locking(&db, &current, &order).unwrap_err();
        assert!(error.contains("spec-a"), "{error}");
        assert!(error.contains("running"), "{error}");
    }

    #[test]
    fn pool_reorder_locking_refuses_ordering_that_moves_a_completed_member() {
        let (_dir, db) = pool_test_db();
        let mut done = standalone_spec("spec-a");
        done.status = LoopSpecStatus::Completed;
        db.insert_loop_spec(&done).unwrap();
        db.insert_loop_spec(&standalone_spec("spec-b")).unwrap();
        insert_pool(&db, "pool-1");
        for id in ["spec-a", "spec-b"] {
            db.append_pool_member("pool-1", id, None).unwrap();
        }
        let current = db.list_pool_member_spec_ids("pool-1").unwrap();

        let order = vec!["spec-b".to_string(), "spec-a".to_string()];
        let error = validate_pool_reorder_locking(&db, &current, &order).unwrap_err();
        assert!(error.contains("spec-a"), "{error}");
        assert!(error.contains("completed"), "{error}");
    }

    #[test]
    fn pool_reorder_locking_allows_permuting_pending_members_only() {
        let (_dir, db) = pool_test_db();
        db.insert_loop_spec(&running_spec("spec-a")).unwrap();
        db.insert_loop_spec(&standalone_spec("spec-b")).unwrap();
        db.insert_loop_spec(&standalone_spec("spec-c")).unwrap();
        insert_pool(&db, "pool-1");
        for id in ["spec-a", "spec-b", "spec-c"] {
            db.append_pool_member("pool-1", id, None).unwrap();
        }
        let current = db.list_pool_member_spec_ids("pool-1").unwrap();

        // spec-a (running) stays at position 0; only the pending tail moves.
        let order = vec![
            "spec-a".to_string(),
            "spec-c".to_string(),
            "spec-b".to_string(),
        ];
        assert!(validate_pool_reorder_locking(&db, &current, &order).is_ok());
    }

    #[test]
    fn pool_remove_spec_refuses_the_currently_running_spec() {
        let (_dir, db) = pool_test_db();
        db.insert_loop_spec(&running_spec("spec-a")).unwrap();
        insert_pool(&db, "pool-1");
        db.append_pool_member("pool-1", "spec-a", None).unwrap();

        let error = validate_pool_member_removable(&db, "pool-1", "spec-a").unwrap_err();
        assert!(error.contains("spec-a"), "{error}");
        assert!(error.contains("running"), "{error}");

        // Once it's no longer running, removal is allowed again.
        db.update_loop_spec_status(
            "spec-a",
            LoopSpecStatus::Completed,
            None,
            Some(chrono::Utc::now()),
        )
        .unwrap();
        assert!(validate_pool_member_removable(&db, "pool-1", "spec-a").is_ok());
    }

    #[test]
    fn pool_add_spec_rejects_nonexistent_spec() {
        let (_dir, db) = pool_test_db();
        insert_pool(&db, "pool-1");

        let error = validate_spec_exists(&db, "ghost-spec").unwrap_err();
        assert!(error.contains("not found"), "{error}");
    }

    #[test]
    fn pool_operations_reject_nonexistent_pool() {
        let (_dir, db) = pool_test_db();

        let error = validate_pool_exists(&db, "does-not-exist").unwrap_err();
        assert!(error.contains("not found"), "{error}");
    }

    #[test]
    fn resolve_graph_target_requires_exactly_one_of_spec_or_loop() {
        let dir = tempdir().unwrap();
        let db = Database::new(&dir.path().join("test.db")).unwrap();

        let neither = resolve_graph_target(&db, None, None).unwrap_err();
        assert!(neither.contains("exactly one"), "{neither}");

        let both = resolve_graph_target(&db, Some("spec-x"), Some("loop-x")).unwrap_err();
        assert!(both.contains("exactly one"), "{both}");
    }

    #[test]
    fn resolve_graph_target_rejects_unknown_loop_id() {
        let dir = tempdir().unwrap();
        let db = Database::new(&dir.path().join("test.db")).unwrap();

        let error = resolve_graph_target(&db, None, Some("does-not-exist")).unwrap_err();
        assert!(error.contains("not found"), "{error}");
    }

    #[test]
    fn loop_get_response_includes_loop_level_graph_alongside_specs() {
        // R1: `loop_get` (via `loop_details_json`) must surface the
        // loop-level graph, not just each spec's own nodes/edges.
        let dir = tempdir().unwrap();
        let db = Database::new(&dir.path().join("test.db")).unwrap();
        let loop_id = "loop-with-graph".to_string();
        db.insert_loop(&Loop {
            id: loop_id.clone(),
            name: "Loop".to_string(),
            description: None,
            workdir: dir.path().to_string_lossy().to_string(),
            status: LoopStatus::Draft,
            trigger: None,
            created_at: chrono::Utc::now(),
            started_at: None,
            completed_at: None,
            autorun_at: None,
            auto_continue_at: None,
            auto_continue_action: None,
            active_run_pool_id: None,
            on_completed: None,
        })
        .unwrap();
        db.insert_loop_node(&LoopNode {
            id: "graph-node".to_string(),
            spec_id: None,
            loop_id: Some(loop_id.clone()),
            name: "implement".to_string(),
            kind: LoopNodeKind::Agent,
            config: serde_json::json!({"platform": "claude"}),
            position: 1,
            created_at: chrono::Utc::now(),
        })
        .unwrap();

        let details = db.get_loop_details(&loop_id).unwrap().unwrap();
        let json = loop_details_json(&db, &details).unwrap();

        assert_eq!(json["graph"]["nodes"].as_array().unwrap().len(), 1);
        assert_eq!(json["graph"]["nodes"][0]["id"], "graph-node");
        assert_eq!(json["graph"]["nodes"][0]["loop_id"], loop_id);
        assert!(json["specs"].as_array().unwrap().is_empty());
    }

    #[test]
    fn loop_get_response_surfaces_pinned_skills_on_a_node() {
        // S2: an agent node's pinned `skills` array must be visible to
        // agents inspecting/building loops via loop_get, not just used
        // internally at spawn time.
        let dir = tempdir().unwrap();
        let db = Database::new(&dir.path().join("test.db")).unwrap();
        let loop_id = "loop-with-pinned-skills".to_string();
        db.insert_loop(&Loop {
            id: loop_id.clone(),
            name: "Loop".to_string(),
            description: None,
            workdir: dir.path().to_string_lossy().to_string(),
            status: LoopStatus::Draft,
            trigger: None,
            created_at: chrono::Utc::now(),
            started_at: None,
            completed_at: None,
            autorun_at: None,
            auto_continue_at: None,
            auto_continue_action: None,
            active_run_pool_id: None,
            on_completed: None,
        })
        .unwrap();
        db.insert_loop_node(&LoopNode {
            id: "pinned-node".to_string(),
            spec_id: None,
            loop_id: Some(loop_id.clone()),
            name: "implement".to_string(),
            kind: LoopNodeKind::Agent,
            config: serde_json::json!({
                "platform": "claude",
                "skills": ["coder", "rust-idiomatic-patterns"]
            }),
            position: 1,
            created_at: chrono::Utc::now(),
        })
        .unwrap();

        let details = db.get_loop_details(&loop_id).unwrap().unwrap();
        let json = loop_details_json(&db, &details).unwrap();

        assert_eq!(
            json["graph"]["nodes"][0]["config"]["skills"],
            serde_json::json!(["coder", "rust-idiomatic-patterns"])
        );
    }

    #[test]
    fn loop_get_response_exposes_autorun_at_so_agents_never_need_sql() {
        // B14: a failed loop's pending autorun schedule must be visible via
        // `loop_get` — before this, an agent had to query
        // `background_agents.db` directly to learn whether a recovery wait
        // was already scheduled.
        let dir = tempdir().unwrap();
        let db = Database::new(&dir.path().join("test.db")).unwrap();
        let loop_id = "loop-with-autorun".to_string();
        db.insert_loop(&Loop {
            id: loop_id.clone(),
            name: "Loop".to_string(),
            description: None,
            workdir: dir.path().to_string_lossy().to_string(),
            status: LoopStatus::Failed,
            trigger: None,
            created_at: chrono::Utc::now(),
            started_at: None,
            completed_at: None,
            autorun_at: None,
            auto_continue_at: None,
            auto_continue_action: None,
            active_run_pool_id: None,
            on_completed: None,
        })
        .unwrap();

        // Unset: field must be present and null, not just absent.
        let details = db.get_loop_details(&loop_id).unwrap().unwrap();
        let json = loop_details_json(&db, &details).unwrap();
        assert!(json["autorun_at"].is_null());

        // `autorun_at` round-trips through an INTEGER (unix seconds) column,
        // so compare against a second-precision timestamp.
        let at = chrono::DateTime::from_timestamp(
            (chrono::Utc::now() + chrono::Duration::hours(2)).timestamp(),
            0,
        )
        .unwrap();
        db.schedule_loop_autorun(&loop_id, at).unwrap();

        let details = db.get_loop_details(&loop_id).unwrap().unwrap();
        let json = loop_details_json(&db, &details).unwrap();
        assert_eq!(json["autorun_at"].as_str().unwrap(), at.to_rfc3339());
    }

    // ── B41: cancel a scheduled autorun via loop_schedule_autorun(at=None) ──

    fn autorun_test_loop(loop_id: &str, workdir: &str, status: LoopStatus) -> Loop {
        Loop {
            id: loop_id.to_string(),
            name: loop_id.to_string(),
            description: None,
            workdir: workdir.to_string(),
            status,
            trigger: None,
            created_at: chrono::Utc::now(),
            started_at: None,
            completed_at: None,
            autorun_at: None,
            auto_continue_at: None,
            auto_continue_action: None,
            active_run_pool_id: None,
            on_completed: None,
        }
    }

    /// B41: a `failed` loop with a pending autorun is exactly the state a
    /// scheduled wake-up needs to be cancellable from — cancelling must
    /// succeed and report the time that was cleared.
    #[tokio::test]
    async fn loop_schedule_autorun_cancels_pending_schedule_on_failed_loop() {
        use rmcp::handler::server::wrapper::Parameters;

        let (dir, db, handler) = queue_test_handler();
        let loop_id = "loop-failed-autorun";
        db.insert_loop(&autorun_test_loop(
            loop_id,
            &dir.path().to_string_lossy(),
            LoopStatus::Failed,
        ))
        .unwrap();
        let scheduled_at = chrono::Utc::now() + chrono::Duration::hours(1);
        db.schedule_loop_autorun(loop_id, scheduled_at).unwrap();

        let result = handler
            .loop_schedule_autorun(Parameters(LoopScheduleAutorunParams {
                loop_id: loop_id.to_string(),
                at: None,
                quota_reset_message: None,
            }))
            .await
            .unwrap();

        let text = result_text(&result);
        assert!(text.contains("cancelled"), "{text}");
        let lp = db.get_loop(loop_id).unwrap().unwrap();
        assert!(lp.autorun_at.is_none(), "cancel must clear autorun_at");
        assert_eq!(
            lp.status,
            LoopStatus::Failed,
            "cancelling must not touch loop status"
        );
    }

    /// B41: cancelling must also work on a `completed` loop — the other
    /// status a loop with a pending autorun can hold.
    #[tokio::test]
    async fn loop_schedule_autorun_cancels_pending_schedule_on_completed_loop() {
        use rmcp::handler::server::wrapper::Parameters;

        let (dir, db, handler) = queue_test_handler();
        let loop_id = "loop-completed-autorun";
        db.insert_loop(&autorun_test_loop(
            loop_id,
            &dir.path().to_string_lossy(),
            LoopStatus::Completed,
        ))
        .unwrap();
        let scheduled_at = chrono::Utc::now() + chrono::Duration::hours(1);
        db.schedule_loop_autorun(loop_id, scheduled_at).unwrap();

        let result = handler
            .loop_schedule_autorun(Parameters(LoopScheduleAutorunParams {
                loop_id: loop_id.to_string(),
                at: None,
                quota_reset_message: None,
            }))
            .await
            .unwrap();

        assert!(result_text(&result).contains("cancelled"));
        let lp = db.get_loop(loop_id).unwrap().unwrap();
        assert!(lp.autorun_at.is_none());
    }

    /// B41: cancelling when nothing is scheduled must succeed and say so —
    /// it is not an error, since the caller's intent (no pending autorun) is
    /// already satisfied.
    #[tokio::test]
    async fn loop_schedule_autorun_cancel_with_nothing_scheduled_is_not_an_error() {
        use rmcp::handler::server::wrapper::Parameters;

        let (dir, db, handler) = queue_test_handler();
        let loop_id = "loop-no-autorun";
        db.insert_loop(&autorun_test_loop(
            loop_id,
            &dir.path().to_string_lossy(),
            LoopStatus::Failed,
        ))
        .unwrap();

        let result = handler
            .loop_schedule_autorun(Parameters(LoopScheduleAutorunParams {
                loop_id: loop_id.to_string(),
                at: None,
                quota_reset_message: None,
            }))
            .await
            .unwrap();

        assert!(!result.is_error.unwrap_or(false), "{result:?}");
        let text = result_text(&result);
        assert!(text.contains("no pending autorun"), "{text}");
    }

    // ── loop_schedule_continue: deferred resume of a paused loop ───────

    /// Setting a schedule on a paused loop must persist `auto_continue_at`
    /// and default the action to `retry_current_node` when omitted.
    #[tokio::test]
    async fn loop_schedule_continue_sets_pending_schedule_with_default_action() {
        use rmcp::handler::server::wrapper::Parameters;

        let (dir, db, handler) = queue_test_handler();
        let loop_id = "loop-paused-continue";
        db.insert_loop(&autorun_test_loop(
            loop_id,
            &dir.path().to_string_lossy(),
            LoopStatus::Paused,
        ))
        .unwrap();
        let scheduled_at = chrono::DateTime::from_timestamp(
            (chrono::Utc::now() + chrono::Duration::hours(1)).timestamp(),
            0,
        )
        .unwrap();

        let result = handler
            .loop_schedule_continue(Parameters(LoopScheduleContinueParams {
                loop_id: loop_id.to_string(),
                at: Some(scheduled_at.to_rfc3339()),
                action: None,
            }))
            .await
            .unwrap();

        assert!(!result.is_error.unwrap_or(false), "{result:?}");
        let text = result_text(&result);
        assert!(text.contains("retry_current_node"), "{text}");
        let lp = db.get_loop(loop_id).unwrap().unwrap();
        assert_eq!(lp.auto_continue_at, Some(scheduled_at));
        assert_eq!(
            lp.auto_continue_action.as_deref(),
            Some("retry_current_node")
        );
    }

    /// An explicit `skip_next_spec` action must be persisted as given.
    #[tokio::test]
    async fn loop_schedule_continue_persists_explicit_skip_action() {
        use rmcp::handler::server::wrapper::Parameters;

        let (dir, db, handler) = queue_test_handler();
        let loop_id = "loop-paused-continue-skip";
        db.insert_loop(&autorun_test_loop(
            loop_id,
            &dir.path().to_string_lossy(),
            LoopStatus::Paused,
        ))
        .unwrap();

        handler
            .loop_schedule_continue(Parameters(LoopScheduleContinueParams {
                loop_id: loop_id.to_string(),
                at: Some((chrono::Utc::now() + chrono::Duration::hours(1)).to_rfc3339()),
                action: Some("skip_next_spec".to_string()),
            }))
            .await
            .unwrap();

        let lp = db.get_loop(loop_id).unwrap().unwrap();
        assert_eq!(lp.auto_continue_action.as_deref(), Some("skip_next_spec"));
    }

    /// An invalid action must be rejected without touching the schedule.
    #[tokio::test]
    async fn loop_schedule_continue_rejects_invalid_action() {
        use rmcp::handler::server::wrapper::Parameters;

        let (dir, db, handler) = queue_test_handler();
        let loop_id = "loop-paused-continue-bad-action";
        db.insert_loop(&autorun_test_loop(
            loop_id,
            &dir.path().to_string_lossy(),
            LoopStatus::Paused,
        ))
        .unwrap();

        let result = handler
            .loop_schedule_continue(Parameters(LoopScheduleContinueParams {
                loop_id: loop_id.to_string(),
                at: Some((chrono::Utc::now() + chrono::Duration::hours(1)).to_rfc3339()),
                action: Some("not_a_real_action".to_string()),
            }))
            .await
            .unwrap();

        assert!(result.is_error.unwrap_or(false), "{result:?}");
        let lp = db.get_loop(loop_id).unwrap().unwrap();
        assert!(
            lp.auto_continue_at.is_none(),
            "an invalid action must not schedule anything"
        );
    }

    /// Omitting `at` must cancel a pending auto-continue schedule, mirroring
    /// `loop_schedule_autorun`'s cancel semantics (B41).
    #[tokio::test]
    async fn loop_schedule_continue_cancels_pending_schedule() {
        use rmcp::handler::server::wrapper::Parameters;

        let (dir, db, handler) = queue_test_handler();
        let loop_id = "loop-paused-continue-cancel";
        db.insert_loop(&autorun_test_loop(
            loop_id,
            &dir.path().to_string_lossy(),
            LoopStatus::Paused,
        ))
        .unwrap();
        db.schedule_loop_auto_continue(
            loop_id,
            chrono::Utc::now() + chrono::Duration::hours(1),
            Some("retry_current_node"),
        )
        .unwrap();

        let result = handler
            .loop_schedule_continue(Parameters(LoopScheduleContinueParams {
                loop_id: loop_id.to_string(),
                at: None,
                action: None,
            }))
            .await
            .unwrap();

        let text = result_text(&result);
        assert!(text.contains("cancelled"), "{text}");
        let lp = db.get_loop(loop_id).unwrap().unwrap();
        assert!(
            lp.auto_continue_at.is_none(),
            "cancel must clear auto_continue_at"
        );
        assert_eq!(
            lp.status,
            LoopStatus::Paused,
            "cancelling must not touch loop status"
        );
    }

    /// Cancelling when nothing is scheduled must succeed and say so.
    #[tokio::test]
    async fn loop_schedule_continue_cancel_with_nothing_scheduled_is_not_an_error() {
        use rmcp::handler::server::wrapper::Parameters;

        let (dir, db, handler) = queue_test_handler();
        let loop_id = "loop-paused-no-continue";
        db.insert_loop(&autorun_test_loop(
            loop_id,
            &dir.path().to_string_lossy(),
            LoopStatus::Paused,
        ))
        .unwrap();

        let result = handler
            .loop_schedule_continue(Parameters(LoopScheduleContinueParams {
                loop_id: loop_id.to_string(),
                at: None,
                action: None,
            }))
            .await
            .unwrap();

        assert!(!result.is_error.unwrap_or(false), "{result:?}");
        let text = result_text(&result);
        assert!(text.contains("no pending auto-continue"), "{text}");
    }

    /// Scheduling `loop_schedule_continue` must never touch `autorun_at`,
    /// and vice versa — the two schedules must stay fully independent so
    /// they can never be conflated at fire time.
    #[tokio::test]
    async fn loop_schedule_continue_and_loop_schedule_autorun_are_independent() {
        use rmcp::handler::server::wrapper::Parameters;

        let (dir, db, handler) = queue_test_handler();
        let loop_id = "loop-independent-schedules";
        db.insert_loop(&autorun_test_loop(
            loop_id,
            &dir.path().to_string_lossy(),
            LoopStatus::Paused,
        ))
        .unwrap();

        handler
            .loop_schedule_continue(Parameters(LoopScheduleContinueParams {
                loop_id: loop_id.to_string(),
                at: Some((chrono::Utc::now() + chrono::Duration::hours(1)).to_rfc3339()),
                action: None,
            }))
            .await
            .unwrap();

        let lp = db.get_loop(loop_id).unwrap().unwrap();
        assert!(lp.auto_continue_at.is_some());
        assert!(
            lp.autorun_at.is_none(),
            "loop_schedule_continue must not set autorun_at"
        );
    }

    // ── U10: copy nodes and ensembles ────────────────────────────────

    fn u10_check(
        id: &str,
        spec_id: Option<&str>,
        loop_id: Option<&str>,
        position: i64,
    ) -> LoopNode {
        LoopNode {
            id: id.to_string(),
            spec_id: spec_id.map(str::to_string),
            loop_id: loop_id.map(str::to_string),
            name: id.to_string(),
            kind: LoopNodeKind::Check,
            config: serde_json::json!({"command": "true"}),
            position,
            created_at: chrono::Utc::now(),
        }
    }

    fn u10_agent(
        id: &str,
        spec_id: Option<&str>,
        loop_id: Option<&str>,
        position: i64,
        config: serde_json::Value,
    ) -> LoopNode {
        LoopNode {
            id: id.to_string(),
            spec_id: spec_id.map(str::to_string),
            loop_id: loop_id.map(str::to_string),
            name: id.to_string(),
            kind: LoopNodeKind::Agent,
            config,
            position,
            created_at: chrono::Utc::now(),
        }
    }

    /// A persisted source ensemble ("proposers"): two members (claude, codex/o1),
    /// wired `from` → members → join → `to`.
    fn u10_source_ensemble(
        db: &Database,
        spec_id: Option<&str>,
        loop_id: Option<&str>,
        from: &str,
        to: &str,
    ) -> BuiltEnsembleUnit {
        let members = [
            ("claude".to_string(), None),
            ("codex".to_string(), Some("o1".to_string())),
        ];
        let built = build_ensemble_unit(&EnsembleUnitSpec {
            spec_id: spec_id.map(str::to_string),
            loop_id: loop_id.map(str::to_string),
            name: "proposers",
            prompt_template: "propose a solution",
            members: &members,
            entry_from_node: from,
            entry_condition: LoopEdgeCondition::Always,
            on_pass_to: to,
            on_fail_to: None,
            min_pass: 2,
            timeout_minutes: 30,
            straggler_timeout_minutes: None,
            start_position: 10,
        });
        db.insert_ensemble_unit(
            &built.ensemble,
            &built.members,
            &built.member_nodes,
            &built.join_node,
            &built.edges,
        )
        .unwrap();
        built
    }

    #[test]
    fn loop_copy_node_applies_overrides_and_wiring() {
        let dir = tempdir().unwrap();
        let db = Database::new(&dir.path().join("t.db")).unwrap();
        insert_test_loop(&db, "loop-1");
        db.insert_loop_node(&u10_agent(
            "impl",
            None,
            Some("loop-1"),
            1,
            serde_json::json!({"platform":"claude","prompt_template":"implement it","timeout_minutes":30}),
        ))
        .unwrap();
        db.insert_loop_node(&u10_check("gate", None, Some("loop-1"), 2))
            .unwrap();

        let params: LoopCopyNodeParams = serde_json::from_value(serde_json::json!({
            "source_node_id": "impl",
            "loop_id": "loop-1",
            "name": "review",
            "config_overrides": {"prompt_template": "review it", "platform": "codex"},
            "on_pass_to": "gate"
        }))
        .unwrap();
        let plan = plan_node_copy(&db, &params).unwrap();

        assert_ne!(plan.node.id, "impl", "the copy must have a fresh id");
        assert_eq!(plan.node.name, "review");
        assert_eq!(plan.node.kind, LoopNodeKind::Agent);
        // Overridden keys win; untouched source keys are preserved.
        assert_eq!(plan.node.config["prompt_template"], "review it");
        assert_eq!(plan.node.config["platform"], "codex");
        assert_eq!(plan.node.config["timeout_minutes"], 30);
        assert_eq!(plan.node.loop_id.as_deref(), Some("loop-1"));
        assert!(plan.node.spec_id.is_none());
        // One outgoing pass edge to the wiring target.
        assert_eq!(plan.edges.len(), 1);
        assert_eq!(plan.edges[0].from_node, plan.node.id);
        assert_eq!(plan.edges[0].to_node, "gate");
        assert_eq!(plan.edges[0].condition.as_str(), "pass");
        assert_eq!(plan.wiring.get("on_pass_to").unwrap(), "gate");
    }

    #[test]
    fn loop_copy_node_unwired_copy_is_valid_and_reported() {
        let dir = tempdir().unwrap();
        let db = Database::new(&dir.path().join("t.db")).unwrap();
        insert_test_loop(&db, "loop-1");
        db.insert_loop_node(&u10_agent(
            "impl",
            None,
            Some("loop-1"),
            1,
            serde_json::json!({"platform":"claude"}),
        ))
        .unwrap();

        // No wiring, no target override → copy into the source's own graph.
        let params: LoopCopyNodeParams =
            serde_json::from_value(serde_json::json!({"source_node_id": "impl"})).unwrap();
        let plan = plan_node_copy(&db, &params).unwrap();

        assert!(plan.edges.is_empty(), "an unwired copy creates no edges");
        assert!(plan.wiring.is_empty());
        assert_eq!(plan.node.loop_id.as_deref(), Some("loop-1"));
        assert_eq!(plan.node.config["platform"], "claude");
        assert_ne!(plan.node.id, "impl");

        // The response note must flag the unwired state explicitly.
        let note = node_copy_note(&plan.source_id, &plan.node.id, false);
        assert!(note.contains("Unwired copy"), "{note}");
        assert!(note.contains("loop_add_edge"), "{note}");
    }

    #[test]
    fn loop_copy_node_rejects_ensemble_owned_source() {
        let dir = tempdir().unwrap();
        let db = Database::new(&dir.path().join("t.db")).unwrap();
        insert_test_loop(&db, "loop-1");
        db.insert_loop_node(&u10_check("kickoff", None, Some("loop-1"), 1))
            .unwrap();
        db.insert_loop_node(&u10_agent(
            "arbiter",
            None,
            Some("loop-1"),
            2,
            serde_json::json!({"platform":"claude"}),
        ))
        .unwrap();
        let src = u10_source_ensemble(&db, None, Some("loop-1"), "kickoff", "arbiter");

        // A member node can't be copied directly — must go through the ensemble.
        let params: LoopCopyNodeParams = serde_json::from_value(serde_json::json!({
            "source_node_id": src.members[0].node_id
        }))
        .unwrap();
        let err = plan_node_copy(&db, &params).unwrap_err();
        assert!(err.contains("loop_copy_ensemble"), "{err}");
    }

    #[test]
    fn loop_copy_ensemble_overrides_prompt_and_rewires() {
        let dir = tempdir().unwrap();
        let db = Database::new(&dir.path().join("t.db")).unwrap();
        insert_test_loop(&db, "loop-1");
        db.insert_loop_node(&u10_check("kickoff", None, Some("loop-1"), 1))
            .unwrap();
        db.insert_loop_node(&u10_agent(
            "arbiter",
            None,
            Some("loop-1"),
            2,
            serde_json::json!({"platform":"claude"}),
        ))
        .unwrap();
        db.insert_loop_node(&u10_check("review_from", None, Some("loop-1"), 3))
            .unwrap();
        db.insert_loop_node(&u10_check("review_next", None, Some("loop-1"), 4))
            .unwrap();
        let src = u10_source_ensemble(&db, None, Some("loop-1"), "kickoff", "arbiter");

        // Canonical use: copy the proposer ensemble, swap in a review prompt,
        // rewire it after the gates.
        let params: LoopCopyEnsembleParams = serde_json::from_value(serde_json::json!({
            "source_ensemble_id": src.ensemble.id,
            "prompt_template": "review the proposal",
            "from_node": "review_from",
            "condition": "pass",
            "on_pass_to": "review_next"
        }))
        .unwrap();
        let plan = plan_ensemble_copy(&db, &params).unwrap();

        assert_ne!(plan.built.ensemble.id, src.ensemble.id);
        assert_eq!(plan.built.ensemble.prompt_template, "review the proposal");
        assert_eq!(plan.entry_from_node, "review_from");
        assert_eq!(plan.entry_condition.as_str(), "pass");
        assert_eq!(plan.on_pass_to, "review_next");
        assert!(!plan.members_replaced);
        assert_eq!(plan.built.member_nodes.len(), 2);
        for node in &plan.built.member_nodes {
            // The shared prompt propagates to every member node's config.
            assert_eq!(node.config["prompt_template"], "review the proposal");
            assert!(
                !src.member_nodes.iter().any(|m| m.id == node.id),
                "member node ids must be fresh"
            );
        }

        // Persisting the plan yields a resolvable ensemble with the same members.
        db.insert_ensemble_unit(
            &plan.built.ensemble,
            &plan.built.members,
            &plan.built.member_nodes,
            &plan.built.join_node,
            &plan.built.edges,
        )
        .unwrap();
        let details = db
            .get_ensemble_details(&plan.built.ensemble.id)
            .unwrap()
            .unwrap();
        assert_eq!(details.members.len(), 2);
        assert_eq!(details.members[0].platform, "claude");
        assert_eq!(details.members[1].platform, "codex");
    }

    #[test]
    fn loop_copy_ensemble_cross_loop_requires_and_uses_target_wiring() {
        let dir = tempdir().unwrap();
        let db = Database::new(&dir.path().join("t.db")).unwrap();
        insert_test_loop(&db, "loop-1");
        db.insert_loop_node(&u10_check("kickoff", None, Some("loop-1"), 1))
            .unwrap();
        db.insert_loop_node(&u10_agent(
            "arbiter",
            None,
            Some("loop-1"),
            2,
            serde_json::json!({"platform":"claude"}),
        ))
        .unwrap();
        let src = u10_source_ensemble(&db, None, Some("loop-1"), "kickoff", "arbiter");

        insert_test_loop(&db, "loop-2");
        db.insert_loop_node(&u10_check("k2", None, Some("loop-2"), 1))
            .unwrap();
        db.insert_loop_node(&u10_agent(
            "a2",
            None,
            Some("loop-2"),
            2,
            serde_json::json!({"platform":"claude"}),
        ))
        .unwrap();

        // Cross-loop copy without wiring: the source's entry/exit nodes don't
        // exist in loop-2, so it must be refused with an actionable message.
        let bad: LoopCopyEnsembleParams = serde_json::from_value(serde_json::json!({
            "source_ensemble_id": src.ensemble.id,
            "loop_id": "loop-2"
        }))
        .unwrap();
        let err = plan_ensemble_copy(&db, &bad).unwrap_err();
        assert!(err.contains("not found in the target graph"), "{err}");

        // With target wiring, the whole unit lands in loop-2's graph.
        let ok: LoopCopyEnsembleParams = serde_json::from_value(serde_json::json!({
            "source_ensemble_id": src.ensemble.id,
            "loop_id": "loop-2",
            "from_node": "k2",
            "on_pass_to": "a2"
        }))
        .unwrap();
        let plan = plan_ensemble_copy(&db, &ok).unwrap();
        assert_eq!(plan.built.ensemble.loop_id.as_deref(), Some("loop-2"));
        assert!(plan.built.ensemble.spec_id.is_none());
        assert_eq!(plan.built.join_node.loop_id.as_deref(), Some("loop-2"));
        for node in &plan.built.member_nodes {
            assert_eq!(node.loop_id.as_deref(), Some("loop-2"));
        }
        assert_eq!(plan.entry_from_node, "k2");
        assert_eq!(plan.on_pass_to, "a2");
    }

    #[test]
    fn loop_copy_ensemble_copies_config_only_no_runtime_state() {
        let dir = tempdir().unwrap();
        let db = Database::new(&dir.path().join("t.db")).unwrap();
        insert_test_loop(&db, "loop-1");
        db.insert_loop_spec(&standalone_spec("spec-src")).unwrap();
        db.insert_loop_node(&u10_check("kickoff", Some("spec-src"), None, 1))
            .unwrap();
        db.insert_loop_node(&u10_agent(
            "arbiter",
            Some("spec-src"),
            None,
            2,
            serde_json::json!({"platform":"claude"}),
        ))
        .unwrap();
        let src = u10_source_ensemble(&db, Some("spec-src"), None, "kickoff", "arbiter");

        // A completed run attached to a SOURCE member node: runtime state that
        // must never be carried into the copy.
        let source_member = src.members[0].node_id.clone();
        db.insert_loop_run(&LoopNodeRun {
            id: "run-src".to_string(),
            loop_id: "loop-1".to_string(),
            spec_id: "spec-src".to_string(),
            node_id: source_member,
            status: LoopRunStatus::Pass,
            input: None,
            output: None,
            started_at: chrono::Utc::now(),
            completed_at: Some(chrono::Utc::now()),
            iteration: 1,
            pid: None,
            boot_id: None,
            session_id: None,
        })
        .unwrap();

        let params: LoopCopyEnsembleParams = serde_json::from_value(serde_json::json!({
            "source_ensemble_id": src.ensemble.id
        }))
        .unwrap();
        let plan = plan_ensemble_copy(&db, &params).unwrap();
        db.insert_ensemble_unit(
            &plan.built.ensemble,
            &plan.built.members,
            &plan.built.member_nodes,
            &plan.built.join_node,
            &plan.built.edges,
        )
        .unwrap();

        // Every copied node has a fresh identity...
        for node in &plan.built.member_nodes {
            assert!(!src.member_nodes.iter().any(|m| m.id == node.id));
        }
        // ...and NO run was copied: the only run is still the original, and
        // none reference the new member nodes.
        let runs = db.list_loop_runs_for_spec("spec-src").unwrap();
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].id, "run-src");
        let new_ids: Vec<&str> = plan
            .built
            .member_nodes
            .iter()
            .map(|n| n.id.as_str())
            .collect();
        assert!(!runs.iter().any(|r| new_ids.contains(&r.node_id.as_str())));
    }

    // ── validate_non_empty ──────────────────────────────────────────

    #[test]
    fn validate_non_empty_rejects_empty_string() {
        assert!(validate_non_empty("", "name").is_err());
    }

    #[test]
    fn validate_non_empty_rejects_whitespace_only() {
        assert!(validate_non_empty("   ", "name").is_err());
        assert!(validate_non_empty("\t\n", "name").is_err());
    }

    #[test]
    fn validate_non_empty_accepts_valid_value() {
        assert!(validate_non_empty("hello", "name").is_ok());
        assert!(validate_non_empty("  hello  ", "name").is_ok());
    }

    #[test]
    fn validate_non_empty_error_includes_field_name() {
        let err = validate_non_empty("", "workdir").unwrap_err();
        assert!(err.contains("workdir"), "{err}");
        assert!(err.contains("must not be empty"), "{err}");
    }

    // ── validate_absolute_dir ───────────────────────────────────────

    #[test]
    fn validate_absolute_dir_rejects_relative_path() {
        assert!(validate_absolute_dir("relative/path").is_err());
    }

    #[test]
    fn validate_absolute_dir_rejects_nonexistent_directory() {
        let err = validate_absolute_dir("/nonexistent/dir/path").unwrap_err();
        assert!(err.contains("existing directory"), "{err}");
    }

    #[test]
    fn validate_absolute_dir_accepts_existing_directory() {
        let dir = tempdir().unwrap();
        assert!(validate_absolute_dir(dir.path().to_str().unwrap()).is_ok());
    }

    // ── validate_spec_workdir ───────────────────────────────────────

    #[test]
    fn validate_spec_workdir_rejects_relative_path() {
        let err = validate_spec_workdir("relative/path").unwrap_err();
        assert!(err.contains("absolute path"), "{err}");
    }

    #[test]
    fn validate_spec_workdir_accepts_absolute_path() {
        assert!(validate_spec_workdir("/some/absolute/path").is_ok());
    }

    #[test]
    fn validate_spec_workdir_does_not_require_existing_directory() {
        assert!(validate_spec_workdir("/nonexistent/dir/path").is_ok());
    }

    // ── build_loop_trigger ──────────────────────────────────────────

    #[test]
    fn build_loop_trigger_none_returns_none() {
        assert!(build_loop_trigger(&None).unwrap().is_none());
    }

    #[test]
    fn build_loop_trigger_manual_returns_none() {
        let params = LoopTriggerParams {
            kind: "manual".to_string(),
            schedule: None,
            path: None,
            events: None,
            debounce_seconds: None,
            recursive: None,
        };
        assert!(build_loop_trigger(&Some(params)).unwrap().is_none());
    }

    #[test]
    fn build_loop_trigger_empty_kind_returns_none() {
        let params = LoopTriggerParams {
            kind: "".to_string(),
            schedule: None,
            path: None,
            events: None,
            debounce_seconds: None,
            recursive: None,
        };
        assert!(build_loop_trigger(&Some(params)).unwrap().is_none());
    }

    #[test]
    fn build_loop_trigger_cron_requires_schedule() {
        let params = LoopTriggerParams {
            kind: "cron".to_string(),
            schedule: None,
            path: None,
            events: None,
            debounce_seconds: None,
            recursive: None,
        };
        let err = build_loop_trigger(&Some(params)).unwrap_err();
        assert!(err.contains("schedule"), "{err}");
    }

    #[test]
    fn build_loop_trigger_cron_rejects_empty_schedule() {
        let params = LoopTriggerParams {
            kind: "cron".to_string(),
            schedule: Some("".to_string()),
            path: None,
            events: None,
            debounce_seconds: None,
            recursive: None,
        };
        let err = build_loop_trigger(&Some(params)).unwrap_err();
        assert!(err.contains("schedule"), "{err}");
    }

    #[test]
    fn build_loop_trigger_watch_requires_path() {
        let params = LoopTriggerParams {
            kind: "watch".to_string(),
            schedule: None,
            path: None,
            events: Some(vec!["modify".to_string()]),
            debounce_seconds: None,
            recursive: None,
        };
        let err = build_loop_trigger(&Some(params)).unwrap_err();
        assert!(err.contains("path"), "{err}");
    }

    #[test]
    fn build_loop_trigger_watch_rejects_relative_path() {
        let params = LoopTriggerParams {
            kind: "watch".to_string(),
            schedule: None,
            path: Some("relative/path".to_string()),
            events: Some(vec!["modify".to_string()]),
            debounce_seconds: None,
            recursive: None,
        };
        let err = build_loop_trigger(&Some(params)).unwrap_err();
        assert!(err.contains("absolute"), "{err}");
    }

    #[test]
    fn build_loop_trigger_watch_requires_events() {
        let params = LoopTriggerParams {
            kind: "watch".to_string(),
            schedule: None,
            path: Some("/tmp/test".to_string()),
            events: None,
            debounce_seconds: None,
            recursive: None,
        };
        let err = build_loop_trigger(&Some(params)).unwrap_err();
        assert!(err.contains("event"), "{err}");
    }

    #[test]
    fn build_loop_trigger_watch_accepts_valid_config() {
        let params = LoopTriggerParams {
            kind: "watch".to_string(),
            schedule: None,
            path: Some("/tmp/test".to_string()),
            events: Some(vec!["create".to_string(), "modify".to_string()]),
            debounce_seconds: Some(5),
            recursive: Some(true),
        };
        let trigger = build_loop_trigger(&Some(params)).unwrap().unwrap();
        match trigger {
            Trigger::Watch {
                path,
                events,
                debounce_seconds,
                recursive,
            } => {
                assert_eq!(path, "/tmp/test");
                assert_eq!(events.len(), 2);
                assert_eq!(debounce_seconds, 5);
                assert!(recursive);
            }
            other => panic!("expected Trigger::Watch, got {other:?}"),
        }
    }

    #[test]
    fn build_loop_trigger_unknown_kind_returns_error() {
        let params = LoopTriggerParams {
            kind: "unknown".to_string(),
            schedule: None,
            path: None,
            events: None,
            debounce_seconds: None,
            recursive: None,
        };
        let err = build_loop_trigger(&Some(params)).unwrap_err();
        assert!(err.contains("Unknown trigger kind"), "{err}");
        assert!(err.contains("unknown"), "{err}");
    }

    // ── build_loop_completion_hook ──────────────────────────────────

    #[test]
    fn build_loop_completion_hook_rejects_empty_platform() {
        let params = LoopCompletionHookParams {
            platform: "".to_string(),
            model: None,
            prompt: "run tests".to_string(),
            timeout_minutes: None,
        };
        let err = build_loop_completion_hook(&params).unwrap_err();
        assert!(err.contains("platform"), "{err}");
    }

    #[test]
    fn build_loop_completion_hook_rejects_whitespace_platform() {
        let params = LoopCompletionHookParams {
            platform: "   ".to_string(),
            model: None,
            prompt: "run tests".to_string(),
            timeout_minutes: None,
        };
        let err = build_loop_completion_hook(&params).unwrap_err();
        assert!(err.contains("platform"), "{err}");
    }

    #[test]
    fn build_loop_completion_hook_rejects_empty_prompt() {
        let params = LoopCompletionHookParams {
            platform: "claude".to_string(),
            model: None,
            prompt: "".to_string(),
            timeout_minutes: None,
        };
        let err = build_loop_completion_hook(&params).unwrap_err();
        assert!(err.contains("prompt"), "{err}");
    }

    #[test]
    fn build_loop_completion_hook_rejects_whitespace_prompt() {
        let params = LoopCompletionHookParams {
            platform: "claude".to_string(),
            model: None,
            prompt: "  \t  ".to_string(),
            timeout_minutes: None,
        };
        let err = build_loop_completion_hook(&params).unwrap_err();
        assert!(err.contains("prompt"), "{err}");
    }

    #[test]
    fn build_loop_completion_hook_accepts_valid_config() {
        let params = LoopCompletionHookParams {
            platform: "claude".to_string(),
            model: Some("opus-4".to_string()),
            prompt: "{{loop_name}} completed".to_string(),
            timeout_minutes: Some(10),
        };
        let hook = build_loop_completion_hook(&params).unwrap();
        assert_eq!(hook.platform, "claude");
        assert_eq!(hook.model, Some("opus-4".to_string()));
        assert_eq!(hook.prompt, "{{loop_name}} completed");
        assert_eq!(hook.timeout_minutes, Some(10));
    }

    #[test]
    fn build_loop_completion_hook_strips_whitespace() {
        let params = LoopCompletionHookParams {
            platform: "  claude  ".to_string(),
            model: Some("  opus  ".to_string()),
            prompt: "  test  ".to_string(),
            timeout_minutes: None,
        };
        let hook = build_loop_completion_hook(&params).unwrap();
        assert_eq!(hook.platform, "claude");
        assert_eq!(hook.model, Some("opus".to_string()));
        assert_eq!(hook.prompt, "test");
    }

    #[test]
    fn build_loop_completion_hook_empty_model_becomes_none() {
        let params = LoopCompletionHookParams {
            platform: "claude".to_string(),
            model: Some("  ".to_string()),
            prompt: "test".to_string(),
            timeout_minutes: None,
        };
        let hook = build_loop_completion_hook(&params).unwrap();
        assert!(hook.model.is_none());
    }

    // ── member_node_config ──────────────────────────────────────────

    #[test]
    fn member_node_config_produces_correct_shape() {
        let config = member_node_config("claude", Some("opus-4"), "do the thing", 30);
        assert_eq!(config["platform"], "claude");
        assert_eq!(config["model"], "opus-4");
        assert_eq!(config["prompt_template"], "do the thing");
        assert_eq!(config["timeout_minutes"], 30);
    }

    #[test]
    fn member_node_config_null_model_when_none() {
        let config = member_node_config("claude", None, "prompt", 15);
        assert_eq!(config["platform"], "claude");
        assert!(config["model"].is_null());
    }

    // ── validate_edge_condition ─────────────────────────────────────

    #[test]
    fn validate_edge_condition_accepts_pass() {
        assert_eq!(
            validate_edge_condition("pass").unwrap(),
            LoopEdgeCondition::Pass
        );
    }

    #[test]
    fn validate_edge_condition_accepts_fail() {
        assert_eq!(
            validate_edge_condition("fail").unwrap(),
            LoopEdgeCondition::Fail
        );
    }

    #[test]
    fn validate_edge_condition_accepts_always() {
        assert_eq!(
            validate_edge_condition("always").unwrap(),
            LoopEdgeCondition::Always
        );
    }

    #[test]
    fn validate_edge_condition_trims_whitespace() {
        assert_eq!(
            validate_edge_condition("  pass  ").unwrap(),
            LoopEdgeCondition::Pass
        );
    }

    #[test]
    fn validate_edge_condition_rejects_invalid() {
        let err = validate_edge_condition("sometimes").unwrap_err();
        assert!(err.contains("pass"), "{err}");
        assert!(err.contains("fail"), "{err}");
        assert!(err.contains("always"), "{err}");
    }

    // ── validate_node_kind ──────────────────────────────────────────

    #[test]
    fn validate_node_kind_accepts_agent() {
        assert_eq!(validate_node_kind("agent").unwrap(), LoopNodeKind::Agent);
    }

    #[test]
    fn validate_node_kind_accepts_check() {
        assert_eq!(validate_node_kind("check").unwrap(), LoopNodeKind::Check);
    }

    #[test]
    fn validate_node_kind_accepts_gate() {
        assert_eq!(validate_node_kind("gate").unwrap(), LoopNodeKind::Gate);
    }

    #[test]
    fn validate_node_kind_accepts_join() {
        assert_eq!(validate_node_kind("join").unwrap(), LoopNodeKind::Join);
    }

    #[test]
    fn validate_node_kind_trims_whitespace() {
        assert_eq!(
            validate_node_kind("  agent  ").unwrap(),
            LoopNodeKind::Agent
        );
    }

    #[test]
    fn validate_node_kind_rejects_invalid() {
        let err = validate_node_kind("invalid").unwrap_err();
        assert!(err.contains("agent"), "{err}");
        assert!(err.contains("check"), "{err}");
        assert!(err.contains("gate"), "{err}");
    }

    // ── validate_not_join_kind ──────────────────────────────────────

    #[test]
    fn validate_not_join_kind_rejects_join() {
        let err = validate_not_join_kind(LoopNodeKind::Join).unwrap_err();
        assert!(err.contains("engine-managed"), "{err}");
        assert!(err.contains("loop_add_ensemble"), "{err}");
    }

    #[test]
    fn validate_not_join_kind_accepts_agent() {
        assert!(validate_not_join_kind(LoopNodeKind::Agent).is_ok());
    }

    #[test]
    fn validate_not_join_kind_accepts_check() {
        assert!(validate_not_join_kind(LoopNodeKind::Check).is_ok());
    }

    #[test]
    fn validate_not_join_kind_accepts_gate() {
        assert!(validate_not_join_kind(LoopNodeKind::Gate).is_ok());
    }

    // ── validate_spec_status ────────────────────────────────────────

    #[test]
    fn validate_spec_status_accepts_all_valid_statuses() {
        assert_eq!(
            validate_spec_status("pending").unwrap(),
            LoopSpecStatus::Pending
        );
        assert_eq!(
            validate_spec_status("running").unwrap(),
            LoopSpecStatus::Running
        );
        assert_eq!(
            validate_spec_status("completed").unwrap(),
            LoopSpecStatus::Completed
        );
        assert_eq!(
            validate_spec_status("failed").unwrap(),
            LoopSpecStatus::Failed
        );
        assert_eq!(
            validate_spec_status("skipped").unwrap(),
            LoopSpecStatus::Skipped
        );
    }

    #[test]
    fn validate_spec_status_is_case_insensitive() {
        assert_eq!(
            validate_spec_status("PENDING").unwrap(),
            LoopSpecStatus::Pending
        );
        assert_eq!(
            validate_spec_status("Running").unwrap(),
            LoopSpecStatus::Running
        );
    }

    #[test]
    fn validate_spec_status_trims_whitespace() {
        assert_eq!(
            validate_spec_status("  pending  ").unwrap(),
            LoopSpecStatus::Pending
        );
    }

    #[test]
    fn validate_spec_status_rejects_invalid() {
        let err = validate_spec_status("unknown").unwrap_err();
        assert!(err.contains("pending"), "{err}");
        assert!(err.contains("running"), "{err}");
        assert!(err.contains("completed"), "{err}");
        assert!(err.contains("failed"), "{err}");
        assert!(err.contains("skipped"), "{err}");
    }

    // ── validate_spec_set_status_target ─────────────────────────────

    #[test]
    fn validate_spec_set_status_target_accepts_valid() {
        assert_eq!(
            validate_spec_set_status_target("pending").unwrap(),
            LoopSpecStatus::Pending
        );
        assert_eq!(
            validate_spec_set_status_target("completed").unwrap(),
            LoopSpecStatus::Completed
        );
        assert_eq!(
            validate_spec_set_status_target("skipped").unwrap(),
            LoopSpecStatus::Skipped
        );
    }

    #[test]
    fn validate_spec_set_status_target_rejects_running() {
        let err = validate_spec_set_status_target("running").unwrap_err();
        assert!(err.contains("pending"), "{err}");
        assert!(err.contains("completed"), "{err}");
        assert!(err.contains("skipped"), "{err}");
    }

    #[test]
    fn validate_spec_set_status_target_rejects_failed() {
        let err = validate_spec_set_status_target("failed").unwrap_err();
        assert!(err.contains("pending"), "{err}");
        assert!(err.contains("completed"), "{err}");
        assert!(err.contains("skipped"), "{err}");
    }

    #[test]
    fn validate_spec_set_status_target_rejects_invalid() {
        assert!(validate_spec_set_status_target("unknown").is_err());
    }

    // ── json_value_kind_name ────────────────────────────────────────

    #[test]
    fn json_value_kind_name_null() {
        assert_eq!(json_value_kind_name(&serde_json::Value::Null), "null");
    }

    #[test]
    fn json_value_kind_name_bool() {
        assert_eq!(json_value_kind_name(&serde_json::json!(true)), "a boolean");
    }

    #[test]
    fn json_value_kind_name_number() {
        assert_eq!(json_value_kind_name(&serde_json::json!(42)), "a number");
    }

    #[test]
    fn json_value_kind_name_string() {
        assert_eq!(
            json_value_kind_name(&serde_json::json!("hello")),
            "a JSON-encoded string"
        );
    }

    #[test]
    fn json_value_kind_name_array() {
        assert_eq!(
            json_value_kind_name(&serde_json::json!([1, 2, 3])),
            "an array"
        );
    }

    #[test]
    fn json_value_kind_name_object() {
        assert_eq!(
            json_value_kind_name(&serde_json::json!({"key": "value"})),
            "an object"
        );
    }

    // ── validate_at_least_one_bool ──────────────────────────────────

    #[test]
    fn validate_at_least_one_bool_rejects_all_false() {
        let err = validate_at_least_one_bool(&[false, false, false], "fields").unwrap_err();
        assert!(err.contains("fields"), "{err}");
        assert!(err.contains("at least one"), "{err}");
    }

    #[test]
    fn validate_at_least_one_bool_accepts_one_true() {
        assert!(validate_at_least_one_bool(&[false, true, false], "fields").is_ok());
    }

    #[test]
    fn validate_at_least_one_bool_accepts_all_true() {
        assert!(validate_at_least_one_bool(&[true, true, true], "fields").is_ok());
    }

    // ── validate_pool_reorder ───────────────────────────────────────

    #[test]
    fn validate_pool_reorder_accepts_valid_permutation() {
        let current = vec!["a".to_string(), "b".to_string(), "c".to_string()];
        let reordered = vec!["c".to_string(), "a".to_string(), "b".to_string()];
        assert!(validate_pool_reorder(&current, &reordered).is_ok());
    }

    #[test]
    fn validate_pool_reorder_accepts_same_order() {
        let current = vec!["a".to_string(), "b".to_string()];
        let reordered = vec!["a".to_string(), "b".to_string()];
        assert!(validate_pool_reorder(&current, &reordered).is_ok());
    }

    #[test]
    fn validate_pool_reorder_rejects_length_mismatch() {
        let current = vec!["a".to_string(), "b".to_string()];
        let reordered = vec!["a".to_string()];
        let err = validate_pool_reorder(&current, &reordered).unwrap_err();
        assert!(err.contains("2"), "{err}");
        assert!(err.contains("1"), "{err}");
    }

    #[test]
    fn validate_pool_reorder_rejects_duplicate() {
        let current = vec!["a".to_string(), "b".to_string(), "c".to_string()];
        let reordered = vec!["a".to_string(), "a".to_string(), "b".to_string()];
        let err = validate_pool_reorder(&current, &reordered).unwrap_err();
        assert!(err.contains("a"), "{err}");
        assert!(err.contains("more than once"), "{err}");
    }

    #[test]
    fn validate_pool_reorder_rejects_unknown_id() {
        let current = vec!["a".to_string(), "b".to_string()];
        let reordered = vec!["a".to_string(), "x".to_string()];
        let err = validate_pool_reorder(&current, &reordered).unwrap_err();
        assert!(err.contains("x"), "{err}");
        assert!(err.contains("no spec"), "{err}");
    }

    // ── spec_summary_json ───────────────────────────────────────────

    #[test]
    fn spec_summary_json_includes_required_fields() {
        let spec = LoopSpec {
            id: "spec-1".to_string(),
            loop_id: Some("loop-1".to_string()),
            name: "My Spec".to_string(),
            description: Some("does stuff".to_string()),
            position: 3,
            parallelizable: true,
            status: LoopSpecStatus::Pending,
            started_at: None,
            completed_at: None,
            spec_start_head: None,
            workdir: Some("/tmp/project".to_string()),
            completed_via: None,
            completed_via_reason: None,
            completed_via_at: None,
        };
        let json = spec_summary_json(&spec);
        assert_eq!(json["id"], "spec-1");
        assert_eq!(json["loop_id"], "loop-1");
        assert_eq!(json["name"], "My Spec");
        assert_eq!(json["description"], "does stuff");
        assert_eq!(json["position"], 3);
        assert_eq!(json["parallelizable"], true);
        assert_eq!(json["status"], "pending");
        assert_eq!(json["workdir"], "/tmp/project");
        assert!(json.get("completed_via").is_none());
    }

    #[test]
    fn spec_summary_json_includes_completed_via_when_set() {
        let spec = LoopSpec {
            id: "spec-2".to_string(),
            loop_id: None,
            name: "Done".to_string(),
            description: None,
            position: 1,
            parallelizable: false,
            status: LoopSpecStatus::Completed,
            started_at: None,
            completed_at: None,
            spec_start_head: None,
            workdir: None,
            completed_via: Some("loop_reset".to_string()),
            completed_via_reason: None,
            completed_via_at: None,
        };
        let json = spec_summary_json(&spec);
        assert_eq!(json["completed_via"], "loop_reset");
    }

    // ── blueprint_json ──────────────────────────────────────────────

    #[test]
    fn blueprint_json_includes_all_fields() {
        let bp = Blueprint {
            id: "bp-1".to_string(),
            name: "my-blueprint".to_string(),
            kind: LoopNodeKind::Agent,
            config: serde_json::json!({"platform": "claude"}),
            builtin: true,
            created_at: chrono::Utc::now(),
        };
        let json = super::blueprint_json(&bp);
        assert_eq!(json["id"], "bp-1");
        assert_eq!(json["name"], "my-blueprint");
        assert_eq!(json["kind"], "agent");
        assert_eq!(json["builtin"], true);
        assert_eq!(json["config"]["platform"], "claude");
    }

    #[test]
    fn blueprint_json_non_builtin() {
        let bp = Blueprint {
            id: "bp-2".to_string(),
            name: "custom".to_string(),
            kind: LoopNodeKind::Check,
            config: serde_json::json!({"command": "true"}),
            builtin: false,
            created_at: chrono::Utc::now(),
        };
        let json = super::blueprint_json(&bp);
        assert_eq!(json["builtin"], false);
        assert_eq!(json["kind"], "check");
    }

    // ── build_id_result / build_json_result ─────────────────────────

    #[test]
    fn build_id_result_contains_key_and_id() {
        let result = build_id_result("abc-123", "loop_id");
        let text = format!("{:?}", result.content);
        assert!(text.contains("abc-123"), "{text}");
        assert!(text.contains("loop_id"), "{text}");
        assert!(result.is_error != Some(true));
    }

    #[test]
    fn build_json_result_contains_value() {
        let value = serde_json::json!({"key": "value", "count": 42});
        let result = build_json_result(&value);
        let text = format!("{:?}", result.content);
        assert!(text.contains("key"), "{text}");
        assert!(text.contains("value"), "{text}");
        assert!(text.contains("42"), "{text}");
        assert!(result.is_error != Some(true));
    }

    // ── loop_trigger_json ───────────────────────────────────────────

    fn make_loop_with_trigger(trigger: Option<Trigger>) -> Loop {
        Loop {
            id: "loop-1".to_string(),
            name: "Test Loop".to_string(),
            description: None,
            workdir: "/tmp".to_string(),
            status: LoopStatus::Draft,
            trigger,
            created_at: chrono::Utc::now(),
            started_at: None,
            completed_at: None,
            autorun_at: None,
            auto_continue_at: None,
            auto_continue_action: None,
            active_run_pool_id: None,
            on_completed: None,
        }
    }

    #[test]
    fn loop_trigger_json_manual() {
        let lp = make_loop_with_trigger(None);
        let json = loop_trigger_json(&lp);
        assert_eq!(json["type"], "manual");
        assert!(json.get("schedule").is_none());
        assert!(json.get("path").is_none());
    }

    #[test]
    fn loop_trigger_json_cron() {
        let lp = make_loop_with_trigger(Some(Trigger::Cron {
            schedule_expr: "0 9 * * *".to_string(),
        }));
        let json = loop_trigger_json(&lp);
        assert_eq!(json["type"], "cron");
        assert_eq!(json["schedule"], "0 9 * * *");
    }

    #[test]
    fn loop_trigger_json_watch() {
        let lp = make_loop_with_trigger(Some(Trigger::Watch {
            path: "/tmp/project".to_string(),
            events: vec![
                crate::domain::models::WatchEvent::Create,
                crate::domain::models::WatchEvent::Modify,
            ],
            debounce_seconds: 3,
            recursive: true,
        }));
        let json = loop_trigger_json(&lp);
        assert_eq!(json["type"], "watch");
        assert_eq!(json["path"], "/tmp/project");
        let events = json["events"].as_array().unwrap();
        assert!(events.contains(&serde_json::json!("create")));
        assert!(events.contains(&serde_json::json!("modify")));
    }

    // ── build_get_tools_response ────────────────────────────────────

    #[test]
    fn build_get_tools_response_session_start() {
        let json = build_get_tools_response("session_start");
        assert_eq!(json["scope"], "session_start");
        assert_eq!(json["risk"], "low");
        assert!(json["protocol"].is_array());
        assert!(json["tools"].is_array());
    }

    #[test]
    fn build_get_tools_response_file_write() {
        let json = build_get_tools_response("file_write");
        assert_eq!(json["scope"], "file_write");
        assert_eq!(json["risk"], "high");
    }

    #[test]
    fn build_get_tools_response_test_run() {
        let json = build_get_tools_response("test_run");
        assert_eq!(json["scope"], "test_run");
        assert_eq!(json["risk"], "medium");
    }

    #[test]
    fn build_get_tools_response_close_session() {
        let json = build_get_tools_response("close_session");
        assert_eq!(json["scope"], "close_session");
        assert_eq!(json["risk"], "low");
    }

    #[test]
    fn build_get_tools_response_multi_agent() {
        let json = build_get_tools_response("multi_agent");
        assert_eq!(json["scope"], "multi_agent");
        assert_eq!(json["risk"], "varies");
    }

    // ── rag_result_json ─────────────────────────────────────────────

    #[test]
    fn rag_result_json_includes_all_fields() {
        let search_result = crate::rag::vector_store::SearchResult {
            id: "sr-1".to_string(),
            file_path: "/tmp/doc.md".to_string(),
            content: "some content here".to_string(),
            created_at: 0,
            distance: Some(0.42f32),
        };
        let json = rag_result_json(&search_result);
        assert_eq!(json["source"], "/tmp/doc.md");
        assert_eq!(json["content"], "some content here");
        assert!(json["distance"].as_f64().unwrap() > 0.41);
        assert!(json["distance"].as_f64().unwrap() < 0.43);
    }

    // ── node_copy_note ──────────────────────────────────────────────

    #[test]
    fn node_copy_note_wired() {
        let note = node_copy_note("src-1", "new-1", true);
        assert!(note.contains("src-1"), "{note}");
        assert!(note.contains("new-1"), "{note}");
        assert!(!note.contains("NO"), "{note}");
    }

    #[test]
    fn node_copy_note_unwired() {
        let note = node_copy_note("src-2", "new-2", false);
        assert!(note.contains("src-2"), "{note}");
        assert!(note.contains("new-2"), "{note}");
        assert!(note.contains("NO"), "{note}");
        assert!(note.contains("loop_add_edge"), "{note}");
    }

    // ── build_loop_update_response / build_spec_update_response / build_node_update_response ──

    #[test]
    fn build_loop_update_response_contains_loop_id() {
        let result = build_loop_update_response("loop-42");
        let text = format!("{:?}", result.content);
        assert!(text.contains("loop-42"), "{text}");
        assert!(text.contains("updated"), "{text}");
        assert!(result.is_error != Some(true));
    }

    #[test]
    fn build_spec_update_response_contains_spec_id() {
        let result = build_spec_update_response("spec-99");
        let text = format!("{:?}", result.content);
        assert!(text.contains("spec-99"), "{text}");
        assert!(text.contains("updated"), "{text}");
        assert!(result.is_error != Some(true));
    }

    #[test]
    fn build_node_update_response_contains_node_id() {
        let result = build_node_update_response("node-7");
        let text = format!("{:?}", result.content);
        assert!(text.contains("node-7"), "{text}");
        assert!(text.contains("updated"), "{text}");
        assert!(result.is_error != Some(true));
    }

    // ── validate_node_config edge cases ─────────────────────────────

    #[test]
    fn validate_node_config_gate_without_evaluate_defaults_to_output_contains() {
        // When "evaluate" is absent, it defaults to "output_contains", so "value" is required.
        let config = serde_json::json!({});
        let err = validate_node_config(LoopNodeKind::Gate, &config).unwrap_err();
        assert!(err.contains("value"), "{err}");
    }

    #[test]
    fn validate_node_config_gate_with_non_output_contains_evaluate_skips_value_check() {
        let config = serde_json::json!({"evaluate": "exit_code_0"});
        assert!(validate_node_config(LoopNodeKind::Gate, &config).is_ok());
    }

    #[test]
    fn validate_node_config_agent_with_cli_only() {
        let config = serde_json::json!({"cli": "codex"});
        assert!(validate_node_config(LoopNodeKind::Agent, &config).is_ok());
    }

    #[test]
    fn validate_node_config_agent_with_both_platform_and_cli() {
        let config = serde_json::json!({"platform": "claude", "cli": "codex"});
        assert!(validate_node_config(LoopNodeKind::Agent, &config).is_ok());
    }

    #[test]
    fn validate_node_config_rejects_non_object() {
        let cases = [
            serde_json::json!(null),
            serde_json::json!(42),
            serde_json::json!("string"),
            serde_json::json!([1, 2]),
        ];
        for config in &cases {
            let err = validate_node_config(LoopNodeKind::Agent, config).unwrap_err();
            assert!(err.contains("JSON object"), "{err}");
        }
    }

    #[test]
    fn validate_node_config_agent_rejects_empty_platform_and_cli() {
        let config = serde_json::json!({"platform": "", "cli": ""});
        let err = validate_node_config(LoopNodeKind::Agent, &config).unwrap_err();
        assert!(err.contains("platform"), "{err}");
    }

    #[test]
    fn validate_node_config_agent_rejects_whitespace_only_platform() {
        let config = serde_json::json!({"platform": "   "});
        let err = validate_node_config(LoopNodeKind::Agent, &config).unwrap_err();
        assert!(err.contains("platform"), "{err}");
    }

    // ── missing_sync_identity_error ─────────────────────────────────

    #[test]
    fn missing_sync_identity_error_message_matches_constant() {
        let err = missing_sync_identity_error();
        assert_eq!(err.message, MISSING_SYNC_IDENTITY_MESSAGE);
    }

    // ── LoopEdgeCondition::from_str / LoopNodeKind::from_str (domain) ──

    #[test]
    fn loop_edge_condition_from_str_roundtrip() {
        for cond in [
            LoopEdgeCondition::Pass,
            LoopEdgeCondition::Fail,
            LoopEdgeCondition::Always,
        ] {
            let s = cond.as_str();
            assert_eq!(LoopEdgeCondition::from_str(s), Some(cond));
        }
    }

    #[test]
    fn loop_edge_condition_from_str_invalid() {
        assert_eq!(LoopEdgeCondition::from_str("sometimes"), None);
        assert_eq!(LoopEdgeCondition::from_str(""), None);
    }

    #[test]
    fn loop_node_kind_from_str_roundtrip() {
        for kind in [
            LoopNodeKind::Agent,
            LoopNodeKind::Check,
            LoopNodeKind::Gate,
            LoopNodeKind::Join,
        ] {
            let s = kind.as_str();
            assert_eq!(LoopNodeKind::from_str(s), Some(kind));
        }
    }

    #[test]
    fn loop_node_kind_from_str_invalid() {
        assert_eq!(LoopNodeKind::from_str("invalid"), None);
        assert_eq!(LoopNodeKind::from_str(""), None);
    }

    #[test]
    fn loop_spec_status_from_str_roundtrip() {
        for status in [
            LoopSpecStatus::Pending,
            LoopSpecStatus::Running,
            LoopSpecStatus::Completed,
            LoopSpecStatus::Failed,
            LoopSpecStatus::Skipped,
        ] {
            let s = status.as_str();
            assert_eq!(LoopSpecStatus::from_str(s), status);
        }
    }

    #[test]
    fn loop_spec_status_from_str_defaults_to_pending() {
        assert_eq!(LoopSpecStatus::from_str("unknown"), LoopSpecStatus::Pending);
        assert_eq!(LoopSpecStatus::from_str(""), LoopSpecStatus::Pending);
    }

    // ── build_loop_trigger: cron with invalid expression ────────────

    #[test]
    fn build_loop_trigger_cron_invalid_expression() {
        let params = LoopTriggerParams {
            kind: "cron".to_string(),
            schedule: Some("not-a-cron".to_string()),
            path: None,
            events: None,
            debounce_seconds: None,
            recursive: None,
        };
        let err = build_loop_trigger(&Some(params)).unwrap_err();
        assert!(err.contains("Invalid cron expression"), "{err}");
    }

    #[test]
    fn build_loop_trigger_cron_valid_expression() {
        let params = LoopTriggerParams {
            kind: "cron".to_string(),
            schedule: Some("*/5 * * * *".to_string()),
            path: None,
            events: None,
            debounce_seconds: None,
            recursive: None,
        };
        let trigger = build_loop_trigger(&Some(params)).unwrap().unwrap();
        match trigger {
            Trigger::Cron { schedule_expr } => {
                assert_eq!(schedule_expr, "*/5 * * * *");
            }
            other => panic!("expected Trigger::Cron, got {other:?}"),
        }
    }
}

#[cfg(test)]
mod additional_tests {
    use super::*;
    use crate::daemon::params::{
        EnsembleMemberParams, LoopCompletionHookParams, LoopTriggerParams,
    };
    use crate::db::Database;
    use crate::domain::loops::{
        Loop, LoopNode, LoopNodeKind, LoopNodeRun, LoopRunStatus, LoopSpec, LoopSpecStatus,
        LoopStatus,
    };
    use crate::domain::models::Trigger;
    use crate::domain::pools::Pool;
    use tempfile::tempdir;

    fn standalone_spec(id: &str) -> LoopSpec {
        LoopSpec {
            id: id.to_string(),
            loop_id: None,
            name: id.to_string(),
            description: None,
            position: 0,
            parallelizable: false,
            status: LoopSpecStatus::Pending,
            started_at: None,
            completed_at: None,
            spec_start_head: None,
            workdir: None,
            completed_via: None,
            completed_via_reason: None,
            completed_via_at: None,
        }
    }

    fn insert_test_loop(db: &Database, id: &str) {
        db.insert_loop(&Loop {
            id: id.to_string(),
            name: id.to_string(),
            description: None,
            workdir: "/tmp".to_string(),
            status: LoopStatus::Draft,
            trigger: None,
            created_at: chrono::Utc::now(),
            started_at: None,
            completed_at: None,
            autorun_at: None,
            auto_continue_at: None,
            auto_continue_action: None,
            active_run_pool_id: None,
            on_completed: None,
        })
        .unwrap();
    }

    fn insert_test_node(db: &Database, id: &str, spec_id: &str) {
        db.insert_loop_node(&LoopNode {
            id: id.to_string(),
            spec_id: Some(spec_id.to_string()),
            loop_id: None,
            name: id.to_string(),
            kind: LoopNodeKind::Check,
            config: serde_json::json!({"command": "true"}),
            position: 1,
            created_at: chrono::Utc::now(),
        })
        .unwrap();
    }

    fn loop_run_row(id: &str, loop_id: &str, spec_id: &str, status: LoopRunStatus) -> LoopNodeRun {
        LoopNodeRun {
            id: id.to_string(),
            loop_id: loop_id.to_string(),
            spec_id: spec_id.to_string(),
            node_id: "node-1".to_string(),
            status,
            input: None,
            output: None,
            started_at: chrono::Utc::now(),
            completed_at: (status != LoopRunStatus::Running).then(chrono::Utc::now),
            iteration: 1,
            pid: None,
            boot_id: None,
            session_id: None,
        }
    }

    // ── validate_position_conflict ────────────────────────────────

    #[test]
    fn validate_position_conflict_no_loop_id_returns_ok() {
        let dir = tempdir().unwrap();
        let db = Database::new(&dir.path().join("test.db")).unwrap();
        assert!(validate_position_conflict(&db, None, "spec-x", 1).is_ok());
    }

    #[test]
    fn validate_position_conflict_no_conflict() {
        let dir = tempdir().unwrap();
        let db = Database::new(&dir.path().join("test.db")).unwrap();
        insert_test_loop(&db, "loop-1");
        let mut spec = standalone_spec("spec-a");
        spec.loop_id = Some("loop-1".to_string());
        db.insert_loop_spec(&spec).unwrap();
        // spec-a is at position 0, checking position 1 — no conflict.
        assert!(validate_position_conflict(&db, Some("loop-1"), "spec-x", 1).is_ok());
    }

    #[test]
    fn validate_position_conflict_detects_conflict() {
        let dir = tempdir().unwrap();
        let db = Database::new(&dir.path().join("test.db")).unwrap();
        insert_test_loop(&db, "loop-1");
        let mut spec = standalone_spec("spec-a");
        spec.loop_id = Some("loop-1".to_string());
        db.insert_loop_spec(&spec).unwrap();
        let error = validate_position_conflict(&db, Some("loop-1"), "spec-x", 0).unwrap_err();
        assert!(error.contains("position 0"), "{error}");
        assert!(error.contains("loop-1"), "{error}");
    }

    #[test]
    fn validate_position_conflict_excludes_self() {
        let dir = tempdir().unwrap();
        let db = Database::new(&dir.path().join("test.db")).unwrap();
        insert_test_loop(&db, "loop-1");
        let mut spec = standalone_spec("spec-a");
        spec.loop_id = Some("loop-1".to_string());
        db.insert_loop_spec(&spec).unwrap();
        // spec-a at position 0, checking spec-a itself at position 0 — no conflict.
        assert!(validate_position_conflict(&db, Some("loop-1"), "spec-a", 0).is_ok());
    }

    // ── validate_node_position_conflict ───────────────────────────

    #[test]
    fn validate_node_position_conflict_no_owner_returns_ok() {
        let dir = tempdir().unwrap();
        let db = Database::new(&dir.path().join("test.db")).unwrap();
        let node = LoopNode {
            id: "n1".to_string(),
            spec_id: None,
            loop_id: None,
            name: "n1".to_string(),
            kind: LoopNodeKind::Check,
            config: serde_json::json!({}),
            position: 1,
            created_at: chrono::Utc::now(),
        };
        assert!(validate_node_position_conflict(&db, &node, "n1", 1).is_ok());
    }

    #[test]
    fn validate_node_position_conflict_spec_owner() {
        let dir = tempdir().unwrap();
        let db = Database::new(&dir.path().join("test.db")).unwrap();
        db.insert_loop_spec(&standalone_spec("spec-1")).unwrap();
        let now = chrono::Utc::now();
        db.insert_loop_node(&LoopNode {
            id: "n1".to_string(),
            spec_id: Some("spec-1".to_string()),
            loop_id: None,
            name: "n1".to_string(),
            kind: LoopNodeKind::Agent,
            config: serde_json::json!({"platform": "claude"}),
            position: 1,
            created_at: now,
        })
        .unwrap();
        let error = validate_node_position_conflict(
            &db,
            &LoopNode {
                id: "n1".to_string(),
                spec_id: Some("spec-1".to_string()),
                loop_id: None,
                name: "n1".to_string(),
                kind: LoopNodeKind::Agent,
                config: serde_json::json!({"platform": "claude"}),
                position: 5,
                created_at: now,
            },
            "other-node",
            1,
        )
        .unwrap_err();
        assert!(error.contains("Spec 'spec-1'"), "{error}");
        assert!(error.contains("position 1"), "{error}");
    }

    #[test]
    fn validate_node_position_conflict_loop_owner() {
        let dir = tempdir().unwrap();
        let db = Database::new(&dir.path().join("test.db")).unwrap();
        insert_test_loop(&db, "loop-1");
        let now = chrono::Utc::now();
        db.insert_loop_node(&LoopNode {
            id: "n1".to_string(),
            spec_id: None,
            loop_id: Some("loop-1".to_string()),
            name: "n1".to_string(),
            kind: LoopNodeKind::Agent,
            config: serde_json::json!({"platform": "claude"}),
            position: 1,
            created_at: now,
        })
        .unwrap();
        let error = validate_node_position_conflict(
            &db,
            &LoopNode {
                id: "n1".to_string(),
                spec_id: None,
                loop_id: Some("loop-1".to_string()),
                name: "n1".to_string(),
                kind: LoopNodeKind::Agent,
                config: serde_json::json!({"platform": "claude"}),
                position: 5,
                created_at: now,
            },
            "other-node",
            1,
        )
        .unwrap_err();
        assert!(error.contains("Loop 'loop-1'"), "{error}");
    }

    // ── validate_spec_workdir edge cases ──────────────────────────

    #[test]
    fn validate_spec_workdir_whitespace_is_absolute() {
        // Whitespace-only relative path is still relative.
        assert!(validate_spec_workdir("  relative  ").is_err());
    }

    #[test]
    fn validate_spec_workdir_root_path() {
        assert!(validate_spec_workdir("/").is_ok());
    }

    // ── validate_not_join_kind edge cases ─────────────────────────

    #[test]
    fn validate_not_join_kind_join_returns_error_with_actionable_message() {
        let err = validate_not_join_kind(LoopNodeKind::Join).unwrap_err();
        assert!(err.contains("engine-managed"), "{err}");
        assert!(err.contains("loop_add_ensemble"), "{err}");
    }

    // ── validate_edge_condition edge cases ────────────────────────

    #[test]
    fn validate_edge_condition_empty_string() {
        let err = validate_edge_condition("").unwrap_err();
        assert!(err.contains("pass"), "{err}");
    }

    #[test]
    fn validate_edge_condition_case_sensitive() {
        // "Pass" (capital P) should not match — only lowercase.
        assert!(validate_edge_condition("Pass").is_err());
    }

    // ── validate_node_kind edge cases ─────────────────────────────

    #[test]
    fn validate_node_kind_empty_string() {
        assert!(validate_node_kind("").is_err());
    }

    #[test]
    fn validate_node_kind_uppercase() {
        assert!(validate_node_kind("AGENT").is_err());
    }

    // ── validate_spec_status edge cases ───────────────────────────

    #[test]
    fn validate_spec_status_empty_string() {
        let err = validate_spec_status("").unwrap_err();
        assert!(err.contains("pending"), "{err}");
    }

    #[test]
    fn validate_spec_set_status_target_empty_string() {
        assert!(validate_spec_set_status_target("").is_err());
    }

    // ── validate_spec_set_status_target edge cases ────────────────

    #[test]
    fn validate_spec_set_status_target_case_insensitive() {
        assert_eq!(
            validate_spec_set_status_target("COMPLETED").unwrap(),
            LoopSpecStatus::Completed
        );
        assert_eq!(
            validate_spec_set_status_target("  Skipped  ").unwrap(),
            LoopSpecStatus::Skipped
        );
    }

    // ── json_value_kind_name edge cases ───────────────────────────

    #[test]
    fn json_value_kind_name_nested_object() {
        let v = serde_json::json!({"a": {"b": 1}});
        assert_eq!(json_value_kind_name(&v), "an object");
    }

    #[test]
    fn json_value_kind_name_empty_array() {
        let v = serde_json::json!([]);
        assert_eq!(json_value_kind_name(&v), "an array");
    }

    // ── validate_at_least_one_bool edge cases ─────────────────────

    #[test]
    fn validate_at_least_one_bool_single_true() {
        assert!(validate_at_least_one_bool(&[true], "f").is_ok());
    }

    #[test]
    fn validate_at_least_one_bool_single_false() {
        assert!(validate_at_least_one_bool(&[false], "f").is_err());
    }

    #[test]
    fn validate_at_least_one_bool_empty_slice() {
        // All false in an empty slice.
        assert!(validate_at_least_one_bool(&[], "f").is_err());
    }

    // ── validate_pool_reorder edge cases ──────────────────────────

    #[test]
    fn validate_pool_reorder_empty_current_and_ids() {
        assert!(validate_pool_reorder(&[], &[]).is_ok());
    }

    #[test]
    fn validate_pool_reorder_single_element() {
        let current = vec!["a".to_string()];
        let reordered = vec!["a".to_string()];
        assert!(validate_pool_reorder(&current, &reordered).is_ok());
    }

    // ── validate_non_empty edge cases ─────────────────────────────

    #[test]
    fn validate_non_empty_single_space() {
        assert!(validate_non_empty(" ", "field").is_err());
    }

    #[test]
    fn validate_non_empty_tabs_and_newlines() {
        assert!(validate_non_empty("\t\n\r", "field").is_err());
    }

    // ── validate_absolute_dir edge cases ──────────────────────────

    #[test]
    fn validate_absolute_dir_root() {
        assert!(validate_absolute_dir("/").is_ok());
    }

    #[test]
    fn validate_absolute_dir_home() {
        let home = std::env::var("HOME").unwrap_or_else(|_| "/root".to_string());
        // Home dir should exist and be absolute.
        if std::path::Path::new(&home).is_dir() {
            assert!(validate_absolute_dir(&home).is_ok());
        }
    }

    // ── node_copy_note edge cases ─────────────────────────────────

    #[test]
    fn node_copy_note_wired_mentions_both_ids() {
        let note = node_copy_note("source-abc", "target-xyz", true);
        assert!(note.contains("source-abc"));
        assert!(note.contains("target-xyz"));
        assert!(!note.contains("Unwired"));
    }

    #[test]
    fn node_copy_note_unwired_suggests_wiring_tool() {
        let note = node_copy_note("src", "dst", false);
        assert!(note.contains("loop_add_edge"));
        assert!(note.contains("entry_from_node") || note.contains("on_pass_to"));
    }

    // ── build_loop_trigger: watch with debounce_seconds and recursive ──

    #[test]
    fn build_loop_trigger_watch_defaults() {
        let params = LoopTriggerParams {
            kind: "watch".to_string(),
            schedule: None,
            path: Some("/tmp".to_string()),
            events: Some(vec!["modify".to_string()]),
            debounce_seconds: None,
            recursive: None,
        };
        let trigger = build_loop_trigger(&Some(params)).unwrap().unwrap();
        match trigger {
            Trigger::Watch {
                debounce_seconds,
                recursive,
                ..
            } => {
                assert_eq!(debounce_seconds, 2, "default debounce should be 2");
                assert!(!recursive, "default recursive should be false");
            }
            other => panic!("expected Watch, got {other:?}"),
        }
    }

    // ── build_loop_completion_hook edge cases ─────────────────────

    #[test]
    fn build_loop_completion_hook_none_model_becomes_none() {
        let params = LoopCompletionHookParams {
            platform: "claude".to_string(),
            model: None,
            prompt: "test".to_string(),
            timeout_minutes: None,
        };
        let hook = build_loop_completion_hook(&params).unwrap();
        assert!(hook.model.is_none());
    }

    #[test]
    fn build_loop_completion_hook_timeout_passthrough() {
        let params = LoopCompletionHookParams {
            platform: "mimo".to_string(),
            model: None,
            prompt: "do stuff".to_string(),
            timeout_minutes: Some(45),
        };
        let hook = build_loop_completion_hook(&params).unwrap();
        assert_eq!(hook.timeout_minutes, Some(45));
    }

    // ── loop_run_status_guard: Draft and Paused are accepted ──────

    #[test]
    fn loop_run_status_guard_draft_accepted() {
        assert!(loop_run_status_guard("loop-d", LoopStatus::Draft).is_ok());
    }

    #[test]
    fn loop_run_status_guard_paused_accepted() {
        assert!(loop_run_status_guard("loop-p", LoopStatus::Paused).is_ok());
    }

    // ── validate_node_config: Join kind ───────────────────────────

    #[test]
    fn validate_node_config_join_always_passes() {
        let config = serde_json::json!({});
        assert!(validate_node_config(LoopNodeKind::Join, &config).is_ok());
    }

    // ── build_loop_trigger: watch with empty events ───────────────

    #[test]
    fn build_loop_trigger_watch_empty_events_rejected() {
        let params = LoopTriggerParams {
            kind: "watch".to_string(),
            schedule: None,
            path: Some("/tmp".to_string()),
            events: Some(vec![]),
            debounce_seconds: None,
            recursive: None,
        };
        let err = build_loop_trigger(&Some(params)).unwrap_err();
        assert!(err.contains("event"), "{err}");
    }

    // ── validate_pool_reorder_locking: pending spec is movable ────

    #[test]
    fn validate_pool_reorder_locking_allows_moving_pending_members() {
        let dir = tempdir().unwrap();
        let db = Database::new(&dir.path().join("test.db")).unwrap();
        db.insert_loop_spec(&standalone_spec("spec-a")).unwrap();
        db.insert_loop_spec(&standalone_spec("spec-b")).unwrap();
        db.insert_loop_spec(&standalone_spec("spec-c")).unwrap();
        db.insert_pool(&Pool {
            id: "pool-1".to_string(),
            name: "pool-1".to_string(),
            created_at: chrono::Utc::now(),
        })
        .unwrap();
        for id in ["spec-a", "spec-b", "spec-c"] {
            db.append_pool_member("pool-1", id, None).unwrap();
        }
        let current = db.list_pool_member_spec_ids("pool-1").unwrap();

        // All pending — any permutation is allowed.
        let order = vec![
            "spec-c".to_string(),
            "spec-a".to_string(),
            "spec-b".to_string(),
        ];
        assert!(validate_pool_reorder_locking(&db, &current, &order).is_ok());
    }

    // ── validate_pool_member_removable: non-running spec ──────────

    #[test]
    fn validate_pool_member_removable_pending_spec_ok() {
        let dir = tempdir().unwrap();
        let db = Database::new(&dir.path().join("test.db")).unwrap();
        db.insert_loop_spec(&standalone_spec("spec-a")).unwrap();
        db.insert_pool(&Pool {
            id: "pool-1".to_string(),
            name: "pool-1".to_string(),
            created_at: chrono::Utc::now(),
        })
        .unwrap();
        assert!(validate_pool_member_removable(&db, "pool-1", "spec-a").is_ok());
    }

    // ── resolve_reported_run: run not found for node_id ───────────

    #[test]
    fn resolve_reported_run_rejects_node_id_mismatch_even_if_running() {
        let dir = tempdir().unwrap();
        let db = Database::new(&dir.path().join("test.db")).unwrap();
        insert_test_loop(&db, "loop-1");
        db.insert_loop_spec(&standalone_spec("spec-a")).unwrap();
        insert_test_node(&db, "node-1", "spec-a");
        db.insert_loop_run(&loop_run_row(
            "run-1",
            "loop-1",
            "spec-a",
            LoopRunStatus::Running,
        ))
        .unwrap();

        let result = resolve_reported_run(&db, "run-1", "wrong-node").unwrap();
        let err = result.expect_err("mismatched node_id must be rejected");
        assert!(err.is_error.unwrap_or(false));
    }

    // ── validate_pool_not_consumed: empty pool ────────────────────

    #[test]
    fn validate_pool_not_consumed_empty_pool() {
        let dir = tempdir().unwrap();
        let db = Database::new(&dir.path().join("test.db")).unwrap();
        db.insert_pool(&Pool {
            id: "pool-empty".to_string(),
            name: "empty".to_string(),
            created_at: chrono::Utc::now(),
        })
        .unwrap();
        assert!(validate_pool_not_consumed(&db, "pool-empty", "loop-1").is_ok());
    }

    // ── loop_run_status_guard: error messages name the loop ───────

    #[test]
    fn loop_run_status_guard_running_error_names_loop() {
        let err = loop_run_status_guard("my-loop", LoopStatus::Running).unwrap_err();
        assert!(err.contains("my-loop"), "{err}");
        assert!(err.contains("already running"), "{err}");
    }

    #[test]
    fn loop_run_status_guard_completed_error_names_reset() {
        let err = loop_run_status_guard("l", LoopStatus::Completed).unwrap_err();
        assert!(err.contains("loop_reset"), "{err}");
    }

    // ── validate_spec_deletable: no loop_id ───────────────────────

    #[test]
    fn validate_spec_deletable_standalone_spec_ok() {
        let spec = LoopSpec {
            id: "spec-1".to_string(),
            loop_id: None,
            name: "test".to_string(),
            description: None,
            position: 0,
            parallelizable: false,
            status: LoopSpecStatus::Pending,
            started_at: None,
            completed_at: None,
            spec_start_head: None,
            workdir: None,
            completed_via: None,
            completed_via_reason: None,
            completed_via_at: None,
        };
        assert!(validate_spec_deletable(&spec).is_ok());
    }

    // ── validate_ensemble_members: whitespace-only platform ───────

    #[test]
    fn validate_ensemble_members_rejects_whitespace_platform() {
        let members = vec![
            EnsembleMemberParams {
                platform: "claude".to_string(),
                model: None,
            },
            EnsembleMemberParams {
                platform: "\t\n".to_string(),
                model: None,
            },
        ];
        let err = validate_ensemble_members(&members).unwrap_err();
        assert!(err.contains("platform"), "{err}");
    }

    // ── validate_ensemble_members: model trimming ─────────────────

    #[test]
    fn validate_ensemble_members_trims_model() {
        let members = vec![
            EnsembleMemberParams {
                platform: "claude".to_string(),
                model: Some("  opus-4  ".to_string()),
            },
            EnsembleMemberParams {
                platform: "mimo".to_string(),
                model: Some("   ".to_string()),
            },
        ];
        let result = validate_ensemble_members(&members).unwrap();
        assert_eq!(result[0].1.as_deref(), Some("opus-4"));
        assert_eq!(result[1].1, None); // whitespace-only model becomes None
    }

    // ── validate_position_conflict: no conflict with same position on different spec ──

    #[test]
    fn validate_position_conflict_same_position_different_loop() {
        let dir = tempdir().unwrap();
        let db = Database::new(&dir.path().join("test.db")).unwrap();
        insert_test_loop(&db, "loop-1");
        insert_test_loop(&db, "loop-2");
        let mut spec = standalone_spec("spec-a");
        spec.loop_id = Some("loop-1".to_string());
        db.insert_loop_spec(&spec).unwrap();
        // spec-a at position 0 in loop-1; check position 0 in loop-2 — no conflict.
        assert!(validate_position_conflict(&db, Some("loop-2"), "spec-x", 0).is_ok());
    }

    // ── build_loop_update_response ────────────────────────────────

    #[test]
    fn build_loop_update_response_text() {
        let result = build_loop_update_response("loop-xyz");
        let text = format!("{:?}", result.content);
        assert!(text.contains("loop-xyz"));
        assert!(text.contains("updated"));
    }

    // ── build_spec_update_response ────────────────────────────────

    #[test]
    fn build_spec_update_response_text() {
        let result = build_spec_update_response("spec-abc");
        let text = format!("{:?}", result.content);
        assert!(text.contains("spec-abc"));
        assert!(text.contains("updated"));
    }

    // ── build_node_update_response ────────────────────────────────

    #[test]
    fn build_node_update_response_text() {
        let result = build_node_update_response("node-xyz");
        let text = format!("{:?}", result.content);
        assert!(text.contains("node-xyz"));
        assert!(text.contains("updated"));
    }

    // ── blueprint_json ────────────────────────────────────────────

    #[test]
    fn blueprint_json_with_agent_kind() {
        let bp = Blueprint {
            id: "bp-a".to_string(),
            name: "agent-bp".to_string(),
            kind: LoopNodeKind::Agent,
            config: serde_json::json!({"platform": "mimo"}),
            builtin: false,
            created_at: chrono::Utc::now(),
        };
        let json = blueprint_json(&bp);
        assert_eq!(json["kind"], "agent");
        assert_eq!(json["builtin"], false);
    }

    // ── spec_summary_json edge cases ──────────────────────────────

    #[test]
    fn spec_summary_json_with_null_fields() {
        let spec = LoopSpec {
            id: "s1".to_string(),
            loop_id: None,
            name: "spec".to_string(),
            description: None,
            position: 0,
            parallelizable: false,
            status: LoopSpecStatus::Pending,
            started_at: None,
            completed_at: None,
            spec_start_head: None,
            workdir: None,
            completed_via: None,
            completed_via_reason: None,
            completed_via_at: None,
        };
        let json = spec_summary_json(&spec);
        assert!(json["loop_id"].is_null());
        assert!(json["description"].is_null());
        assert!(json["workdir"].is_null());
    }

    // ── build_get_tools_response: all scopes return objects ────────

    #[test]
    fn build_get_tools_response_all_scopes_are_objects() {
        for scope in [
            "session_start",
            "file_write",
            "test_run",
            "close_session",
            "multi_agent",
        ] {
            let json = build_get_tools_response(scope);
            assert!(json.is_object(), "scope '{scope}' should be an object");
            assert!(json["scope"].as_str().is_some());
            assert!(json["protocol"].is_array());
            assert!(json["tools"].is_array());
        }
    }

    // ── rag_result_json edge cases ────────────────────────────────

    #[test]
    fn rag_result_json_with_none_distance() {
        let result = crate::rag::vector_store::SearchResult {
            id: "sr-2".to_string(),
            file_path: "/doc.md".to_string(),
            content: "text".to_string(),
            created_at: 0,
            distance: None,
        };
        let json = rag_result_json(&result);
        assert!(json["distance"].is_null());
    }

    // ── validate_node_config: gate with custom evaluate ───────────

    #[test]
    fn validate_node_config_gate_custom_evaluate_no_value_required() {
        let config = serde_json::json!({"evaluate": "exit_code_0"});
        assert!(validate_node_config(LoopNodeKind::Gate, &config).is_ok());
    }

    // ── validate_pool_reorder_locking: skipped spec is locked ─────

    #[test]
    fn validate_pool_reorder_locking_refuses_moving_skipped_spec() {
        let dir = tempdir().unwrap();
        let db = Database::new(&dir.path().join("test.db")).unwrap();
        let mut skipped = standalone_spec("spec-a");
        skipped.status = LoopSpecStatus::Skipped;
        db.insert_loop_spec(&skipped).unwrap();
        db.insert_loop_spec(&standalone_spec("spec-b")).unwrap();
        db.insert_pool(&Pool {
            id: "pool-1".to_string(),
            name: "pool-1".to_string(),
            created_at: chrono::Utc::now(),
        })
        .unwrap();
        db.append_pool_member("pool-1", "spec-a", None).unwrap();
        db.append_pool_member("pool-1", "spec-b", None).unwrap();
        let current = db.list_pool_member_spec_ids("pool-1").unwrap();

        let order = vec!["spec-b".to_string(), "spec-a".to_string()];
        let error = validate_pool_reorder_locking(&db, &current, &order).unwrap_err();
        assert!(error.contains("spec-a"), "{error}");
        assert!(error.contains("skipped"), "{error}");
    }

    // ── node_copy_note ────────────────────────────────────────────

    #[test]
    fn node_copy_note_wired() {
        let note = node_copy_note("src", "dst", true);
        assert!(note.contains("src"));
        assert!(note.contains("dst"));
        assert!(!note.contains("Unwired"));
    }

    #[test]
    fn node_copy_note_unwired() {
        let note = node_copy_note("src", "dst", false);
        assert!(note.contains("Unwired"));
        assert!(note.contains("src"));
        assert!(note.contains("dst"));
        assert!(note.contains("NO incoming or outgoing edges"));
    }

    // ── json_value_kind_name ──────────────────────────────────────

    #[test]
    fn json_value_kind_name_null() {
        assert_eq!(json_value_kind_name(&serde_json::Value::Null), "null");
    }

    #[test]
    fn json_value_kind_name_bool() {
        assert_eq!(json_value_kind_name(&serde_json::json!(true)), "a boolean");
    }

    #[test]
    fn json_value_kind_name_number() {
        assert_eq!(json_value_kind_name(&serde_json::json!(42)), "a number");
    }

    #[test]
    fn json_value_kind_name_string() {
        assert_eq!(
            json_value_kind_name(&serde_json::json!("hello")),
            "a JSON-encoded string"
        );
    }

    #[test]
    fn json_value_kind_name_array() {
        assert_eq!(json_value_kind_name(&serde_json::json!([1, 2])), "an array");
    }

    #[test]
    fn json_value_kind_name_object() {
        assert_eq!(
            json_value_kind_name(&serde_json::json!({"a": 1})),
            "an object"
        );
    }

    // ── validate_node_config ──────────────────────────────────────

    #[test]
    fn validate_node_config_agent_needs_platform() {
        let config = serde_json::json!({"command": "test"});
        let err = validate_node_config(LoopNodeKind::Agent, &config).unwrap_err();
        assert!(err.contains("platform"), "{err}");
    }

    #[test]
    fn validate_node_config_agent_with_platform() {
        let config = serde_json::json!({"platform": "claude"});
        assert!(validate_node_config(LoopNodeKind::Agent, &config).is_ok());
    }

    #[test]
    fn validate_node_config_agent_with_cli() {
        let config = serde_json::json!({"cli": "opencode"});
        assert!(validate_node_config(LoopNodeKind::Agent, &config).is_ok());
    }

    #[test]
    fn validate_node_config_check_needs_command() {
        let config = serde_json::json!({"platform": "claude"});
        let err = validate_node_config(LoopNodeKind::Check, &config).unwrap_err();
        assert!(err.contains("command"), "{err}");
    }

    #[test]
    fn validate_node_config_check_with_command() {
        let config = serde_json::json!({"command": "cargo test"});
        assert!(validate_node_config(LoopNodeKind::Check, &config).is_ok());
    }

    #[test]
    fn validate_node_config_gate_needs_value_when_output_contains() {
        let config = serde_json::json!({});
        let err = validate_node_config(LoopNodeKind::Gate, &config).unwrap_err();
        assert!(err.contains("value"), "{err}");
    }

    #[test]
    fn validate_node_config_gate_with_value() {
        let config = serde_json::json!({"value": "success"});
        assert!(validate_node_config(LoopNodeKind::Gate, &config).is_ok());
    }

    #[test]
    fn validate_node_config_not_object() {
        let config = serde_json::json!("not an object");
        let err = validate_node_config(LoopNodeKind::Agent, &config).unwrap_err();
        assert!(err.contains("JSON object"), "{err}");
    }

    #[test]
    fn validate_node_config_join_always_ok() {
        let config = serde_json::json!({});
        assert!(validate_node_config(LoopNodeKind::Join, &config).is_ok());
    }

    #[test]
    fn validate_node_config_agent_empty_platform() {
        let config = serde_json::json!({"platform": "  "});
        let err = validate_node_config(LoopNodeKind::Agent, &config).unwrap_err();
        assert!(err.contains("platform"), "{err}");
    }

    // ── validate_edge_condition ───────────────────────────────────

    #[test]
    fn validate_edge_condition_pass() {
        assert!(matches!(
            validate_edge_condition("pass").unwrap(),
            LoopEdgeCondition::Pass
        ));
    }

    #[test]
    fn validate_edge_condition_fail() {
        assert!(matches!(
            validate_edge_condition("fail").unwrap(),
            LoopEdgeCondition::Fail
        ));
    }

    #[test]
    fn validate_edge_condition_always() {
        assert!(matches!(
            validate_edge_condition("always").unwrap(),
            LoopEdgeCondition::Always
        ));
    }

    #[test]
    fn validate_edge_condition_invalid() {
        assert!(validate_edge_condition("sometimes").is_err());
    }

    #[test]
    fn validate_edge_condition_with_whitespace() {
        assert!(matches!(
            validate_edge_condition("  pass  ").unwrap(),
            LoopEdgeCondition::Pass
        ));
    }

    // ── validate_node_kind ────────────────────────────────────────

    #[test]
    fn validate_node_kind_agent() {
        assert!(matches!(
            validate_node_kind("agent").unwrap(),
            LoopNodeKind::Agent
        ));
    }

    #[test]
    fn validate_node_kind_check() {
        assert!(matches!(
            validate_node_kind("check").unwrap(),
            LoopNodeKind::Check
        ));
    }

    #[test]
    fn validate_node_kind_gate() {
        assert!(matches!(
            validate_node_kind("gate").unwrap(),
            LoopNodeKind::Gate
        ));
    }

    #[test]
    fn validate_node_kind_invalid() {
        assert!(validate_node_kind("invalid").is_err());
    }

    #[test]
    fn validate_node_kind_with_whitespace() {
        assert!(matches!(
            validate_node_kind("  agent  ").unwrap(),
            LoopNodeKind::Agent
        ));
    }

    // ── validate_not_join_kind ────────────────────────────────────

    #[test]
    fn validate_not_join_kind_rejects_join() {
        assert!(validate_not_join_kind(LoopNodeKind::Join).is_err());
    }

    #[test]
    fn validate_not_join_kind_accepts_agent() {
        assert!(validate_not_join_kind(LoopNodeKind::Agent).is_ok());
    }

    #[test]
    fn validate_not_join_kind_accepts_check() {
        assert!(validate_not_join_kind(LoopNodeKind::Check).is_ok());
    }

    #[test]
    fn validate_not_join_kind_accepts_gate() {
        assert!(validate_not_join_kind(LoopNodeKind::Gate).is_ok());
    }

    // ── validate_spec_status ──────────────────────────────────────

    #[test]
    fn validate_spec_status_all_valid() {
        assert!(matches!(
            validate_spec_status("pending").unwrap(),
            LoopSpecStatus::Pending
        ));
        assert!(matches!(
            validate_spec_status("running").unwrap(),
            LoopSpecStatus::Running
        ));
        assert!(matches!(
            validate_spec_status("completed").unwrap(),
            LoopSpecStatus::Completed
        ));
        assert!(matches!(
            validate_spec_status("failed").unwrap(),
            LoopSpecStatus::Failed
        ));
        assert!(matches!(
            validate_spec_status("skipped").unwrap(),
            LoopSpecStatus::Skipped
        ));
    }

    #[test]
    fn validate_spec_status_case_insensitive() {
        assert!(validate_spec_status("PENDING").is_ok());
        assert!(validate_spec_status("Running").is_ok());
    }

    #[test]
    fn validate_spec_status_with_whitespace() {
        assert!(validate_spec_status("  pending  ").is_ok());
    }

    #[test]
    fn validate_spec_status_invalid() {
        assert!(validate_spec_status("unknown").is_err());
    }

    // ── validate_spec_set_status_target ───────────────────────────

    #[test]
    fn validate_spec_set_status_target_valid() {
        assert!(validate_spec_set_status_target("pending").is_ok());
        assert!(validate_spec_set_status_target("completed").is_ok());
        assert!(validate_spec_set_status_target("skipped").is_ok());
    }

    #[test]
    fn validate_spec_set_status_target_running_invalid() {
        assert!(validate_spec_set_status_target("running").is_err());
    }

    #[test]
    fn validate_spec_set_status_target_failed_invalid() {
        assert!(validate_spec_set_status_target("failed").is_err());
    }

    // ── validate_at_least_one_bool ────────────────────────────────

    #[test]
    fn validate_at_least_one_bool_all_false() {
        assert!(validate_at_least_one_bool(&[false, false, false], "test").is_err());
    }

    #[test]
    fn validate_at_least_one_bool_one_true() {
        assert!(validate_at_least_one_bool(&[false, true, false], "test").is_ok());
    }

    #[test]
    fn validate_at_least_one_bool_all_true() {
        assert!(validate_at_least_one_bool(&[true, true], "test").is_ok());
    }

    #[test]
    fn validate_at_least_one_bool_empty() {
        assert!(validate_at_least_one_bool(&[], "test").is_err());
    }

    // ── missing_sync_identity_error ───────────────────────────────

    #[test]
    fn missing_sync_identity_error_includes_canopy_identity() {
        let err = missing_sync_identity_error();
        assert!(err.message.contains("Canopy session identity"));
    }

    // ── validate_spec_workdir ─────────────────────────────────────

    #[test]
    fn validate_spec_workdir_absolute() {
        assert!(validate_spec_workdir("/tmp").is_ok());
    }

    #[test]
    fn validate_spec_workdir_relative() {
        assert!(validate_spec_workdir("relative/path").is_err());
    }

    // ── validate_non_empty ────────────────────────────────────────

    #[test]
    fn validate_non_empty_valid() {
        assert!(validate_non_empty("hello", "field").is_ok());
    }

    #[test]
    fn validate_non_empty_empty() {
        assert!(validate_non_empty("", "field").is_err());
    }

    #[test]
    fn validate_non_empty_whitespace() {
        assert!(validate_non_empty("   ", "field").is_err());
    }

    #[test]
    fn validate_non_empty_error_message() {
        let err = validate_non_empty("", "my_field").unwrap_err();
        assert!(err.contains("my_field"));
    }

    // ── spec_summary_json ─────────────────────────────────────────

    #[test]
    fn spec_summary_json_basic() {
        let spec = standalone_spec("spec-1");
        let json = spec_summary_json(&spec);
        assert_eq!(json["id"], "spec-1");
        assert_eq!(json["name"], "spec-1");
        assert_eq!(json["status"], "pending");
        assert_eq!(json["parallelizable"], false);
    }

    #[test]
    fn spec_summary_json_with_completed_via() {
        let mut spec = standalone_spec("spec-1");
        spec.completed_via = Some("test".to_string());
        let json = spec_summary_json(&spec);
        assert_eq!(json["completed_via"], "test");
    }

    #[test]
    fn spec_summary_json_with_workdir() {
        let mut spec = standalone_spec("spec-1");
        spec.workdir = Some("/tmp/project".to_string());
        let json = spec_summary_json(&spec);
        assert_eq!(json["workdir"], "/tmp/project");
    }

    // ── build_get_tools_response ──────────────────────────────────

    #[test]
    fn build_get_tools_response_session_start() {
        let json = build_get_tools_response("session_start");
        assert_eq!(json["scope"], "session_start");
        assert_eq!(json["risk"], "low");
        assert!(json["protocol"].is_array());
        assert!(json["tools"].is_array());
    }

    #[test]
    fn build_get_tools_response_file_write() {
        let json = build_get_tools_response("file_write");
        assert_eq!(json["scope"], "file_write");
        assert_eq!(json["risk"], "high");
    }

    #[test]
    fn build_get_tools_response_test_run() {
        let json = build_get_tools_response("test_run");
        assert_eq!(json["scope"], "test_run");
        assert_eq!(json["risk"], "medium");
    }

    #[test]
    fn build_get_tools_response_close_session() {
        let json = build_get_tools_response("close_session");
        assert_eq!(json["scope"], "close_session");
        assert_eq!(json["risk"], "low");
    }

    #[test]
    fn build_get_tools_response_multi_agent() {
        let json = build_get_tools_response("multi_agent");
        assert_eq!(json["scope"], "multi_agent");
        assert_eq!(json["risk"], "varies");
    }

    // ── loop_run_status_guard ─────────────────────────────────────

    #[test]
    fn loop_run_status_guard_running() {
        let err = loop_run_status_guard("loop-1", LoopStatus::Running).unwrap_err();
        assert!(err.contains("already running"));
    }

    #[test]
    fn loop_run_status_guard_completed() {
        let err = loop_run_status_guard("loop-1", LoopStatus::Completed).unwrap_err();
        assert!(err.contains("cannot be resumed directly"), "{err}");
    }

    #[test]
    fn loop_run_status_guard_failed() {
        let err = loop_run_status_guard("loop-1", LoopStatus::Failed).unwrap_err();
        assert!(err.contains("cannot be resumed directly"), "{err}");
    }

    #[test]
    fn loop_run_status_guard_draft() {
        assert!(loop_run_status_guard("loop-1", LoopStatus::Draft).is_ok());
    }

    // ── loop_trigger_json ─────────────────────────────────────────

    #[test]
    fn loop_trigger_json_manual() {
        let lp = Loop {
            id: "l1".to_string(),
            name: "l1".to_string(),
            description: None,
            workdir: "/tmp".to_string(),
            status: LoopStatus::Draft,
            trigger: None,
            created_at: chrono::Utc::now(),
            started_at: None,
            completed_at: None,
            autorun_at: None,
            auto_continue_at: None,
            auto_continue_action: None,
            active_run_pool_id: None,
            on_completed: None,
        };
        let json = loop_trigger_json(&lp);
        assert_eq!(json["type"], "manual");
        assert!(json.get("schedule").is_none());
    }

    #[test]
    fn loop_trigger_json_cron() {
        let lp = Loop {
            id: "l1".to_string(),
            name: "l1".to_string(),
            description: None,
            workdir: "/tmp".to_string(),
            status: LoopStatus::Draft,
            trigger: Some(Trigger::Cron {
                schedule_expr: "0 9 * * *".to_string(),
            }),
            created_at: chrono::Utc::now(),
            started_at: None,
            completed_at: None,
            autorun_at: None,
            auto_continue_at: None,
            auto_continue_action: None,
            active_run_pool_id: None,
            on_completed: None,
        };
        let json = loop_trigger_json(&lp);
        assert_eq!(json["type"], "cron");
        assert_eq!(json["schedule"], "0 9 * * *");
    }

    // ── validate_pool_reorder ─────────────────────────────────────

    #[test]
    fn validate_pool_reorder_wrong_count() {
        let current = vec!["a".to_string(), "b".to_string()];
        let spec_ids = vec!["a".to_string()];
        assert!(validate_pool_reorder(&current, &spec_ids).is_err());
    }

    #[test]
    fn validate_pool_reorder_duplicate() {
        let current = vec!["a".to_string(), "b".to_string()];
        let spec_ids = vec!["a".to_string(), "a".to_string()];
        assert!(validate_pool_reorder(&current, &spec_ids).is_err());
    }

    #[test]
    fn validate_pool_reorder_unknown_spec() {
        let current = vec!["a".to_string(), "b".to_string()];
        let spec_ids = vec!["a".to_string(), "c".to_string()];
        assert!(validate_pool_reorder(&current, &spec_ids).is_err());
    }

    #[test]
    fn validate_pool_reorder_valid() {
        let current = vec!["a".to_string(), "b".to_string()];
        let spec_ids = vec!["b".to_string(), "a".to_string()];
        assert!(validate_pool_reorder(&current, &spec_ids).is_ok());
    }

    // ── blueprint_json ────────────────────────────────────────────

    #[test]
    fn blueprint_json_basic() {
        let bp = Blueprint {
            id: "bp-1".to_string(),
            name: "ensemble-proposers".to_string(),
            kind: LoopNodeKind::Agent,
            config: serde_json::json!({"members": []}),
            builtin: true,
            created_at: chrono::Utc::now(),
        };
        let json = blueprint_json(&bp);
        assert_eq!(json["id"], "bp-1");
        assert_eq!(json["name"], "ensemble-proposers");
        assert_eq!(json["builtin"], true);
    }

    // ── validate_absolute_dir ─────────────────────────────────────

    #[test]
    fn validate_absolute_dir_valid() {
        assert!(validate_absolute_dir("/tmp").is_ok());
    }

    #[test]
    fn validate_absolute_dir_relative() {
        assert!(validate_absolute_dir("relative").is_err());
    }
}

// ── Additional coverage tests for handler.rs ─────────────────────
// Tests targeting uncovered functions: transport_details,
// append_temporal_agents_section, loop_run_blocker, loop_node_json,
// loop_edge_json, loop_run_json, ensemble_details_json,
// loop_completion_hook_json, loop_completion_hook_run_json,
// format_system_time, model_result_footer, build_ensemble_unit,
// and edge-case branches across validation functions.
#[cfg(test)]
mod coverage_tests {
    use super::*;
    use crate::daemon::params::EnsembleMemberParams;
    use crate::db::Database;
    use crate::domain::loops::{
        Ensemble, EnsembleDetails, EnsembleMember, Loop, LoopCompletionHook, LoopCompletionHookRun,
        LoopEdge, LoopEdgeCondition, LoopNode, LoopNodeKind, LoopNodeRun, LoopRunStatus, LoopSpec,
        LoopSpecStatus, LoopStatus,
    };
    use crate::domain::models::{Agent, Cli};
    use crate::domain::pools::Pool;
    use tempfile::tempdir;

    fn standalone_spec(id: &str) -> LoopSpec {
        LoopSpec {
            id: id.to_string(),
            loop_id: None,
            name: id.to_string(),
            description: None,
            position: 0,
            parallelizable: false,
            status: LoopSpecStatus::Pending,
            started_at: None,
            completed_at: None,
            spec_start_head: None,
            workdir: None,
            completed_via: None,
            completed_via_reason: None,
            completed_via_at: None,
        }
    }

    fn running_spec(id: &str) -> LoopSpec {
        let mut spec = standalone_spec(id);
        spec.status = LoopSpecStatus::Running;
        spec
    }

    fn make_agent(id: &str) -> Agent {
        Agent {
            id: id.to_string(),
            prompt: "test prompt".to_string(),
            trigger: None,
            cli: Cli("opencode".to_string()),
            model: None,
            working_dir: None,
            enabled: true,
            enable_at: None,
            created_at: chrono::Utc::now(),
            log_path: "/tmp/test.log".to_string(),
            timeout_minutes: 15,
            expires_at: None,
            last_run_at: None,
            last_run_ok: None,
            last_triggered_at: None,
            trigger_count: 0,
        }
    }

    fn make_loop(loop_id: &str, status: LoopStatus) -> Loop {
        Loop {
            id: loop_id.to_string(),
            name: loop_id.to_string(),
            description: None,
            workdir: "/tmp".to_string(),
            status,
            trigger: None,
            created_at: chrono::Utc::now(),
            started_at: None,
            completed_at: None,
            autorun_at: None,
            auto_continue_at: None,
            auto_continue_action: None,
            active_run_pool_id: None,
            on_completed: None,
        }
    }

    fn loop_run_row(id: &str, loop_id: &str, spec_id: &str, status: LoopRunStatus) -> LoopNodeRun {
        LoopNodeRun {
            id: id.to_string(),
            loop_id: loop_id.to_string(),
            spec_id: spec_id.to_string(),
            node_id: "node-1".to_string(),
            status,
            input: None,
            output: None,
            started_at: chrono::Utc::now(),
            completed_at: (status != LoopRunStatus::Running).then(chrono::Utc::now),
            iteration: 1,
            pid: None,
            boot_id: None,
            session_id: None,
        }
    }

    fn insert_test_loop(db: &Database, id: &str) {
        db.insert_loop(&make_loop(id, LoopStatus::Draft)).unwrap();
    }

    fn insert_test_node(db: &Database, id: &str, spec_id: &str) {
        db.insert_loop_node(&LoopNode {
            id: id.to_string(),
            spec_id: Some(spec_id.to_string()),
            loop_id: None,
            name: id.to_string(),
            kind: LoopNodeKind::Check,
            config: serde_json::json!({"command": "true"}),
            position: 1,
            created_at: chrono::Utc::now(),
        })
        .unwrap();
    }

    // ── transport_details ──────────────────────────────────────────

    #[test]
    fn transport_details_streamable_http() {
        let (transport, port_str) = super::transport_details(8080);
        assert_eq!(transport, "Streamable HTTP");
        assert_eq!(port_str, "8080");
    }

    #[test]
    fn transport_details_stdio() {
        let (transport, port_str) = super::transport_details(0);
        assert_eq!(transport, "stdio");
        assert_eq!(port_str, "N/A");
    }

    #[test]
    fn transport_details_large_port() {
        let (transport, port_str) = super::transport_details(65535);
        assert_eq!(transport, "Streamable HTTP");
        assert_eq!(port_str, "65535");
    }

    // ── append_temporal_agents_section ──────────────────────────────

    #[test]
    fn append_temporal_empty_agents() {
        let mut status = "Canopy v0.1.0".to_string();
        super::append_temporal_agents_section(&mut status, &[]);
        assert_eq!(status, "Canopy v0.1.0");
    }

    #[test]
    fn append_temporal_no_expiring() {
        let mut status = "status".to_string();
        let agents = vec![make_agent("a1")];
        super::append_temporal_agents_section(&mut status, &agents);
        assert_eq!(status, "status");
    }

    #[test]
    fn append_temporal_active_expiry() {
        let mut status = "status".to_string();
        let mut a = make_agent("exp");
        a.expires_at = Some(chrono::Utc::now() + chrono::Duration::minutes(10));
        super::append_temporal_agents_section(&mut status, &[a]);
        assert!(status.contains("Temporal agents:"));
        assert!(status.contains("exp"));
        assert!(status.contains("remaining"));
    }

    #[test]
    fn append_temporal_expired() {
        let mut status = "status".to_string();
        let mut a = make_agent("old");
        a.expires_at = Some(chrono::Utc::now() - chrono::Duration::minutes(5));
        super::append_temporal_agents_section(&mut status, &[a]);
        assert!(status.contains("EXPIRED"));
    }

    #[test]
    fn append_temporal_skips_disabled() {
        let mut status = "status".to_string();
        let mut a = make_agent("dis");
        a.enabled = false;
        a.expires_at = Some(chrono::Utc::now() + chrono::Duration::minutes(10));
        super::append_temporal_agents_section(&mut status, &[a]);
        assert_eq!(status, "status");
    }

    #[test]
    fn append_temporal_multiple_agents() {
        let mut status = String::new();
        let mut a1 = make_agent("a1");
        a1.expires_at = Some(chrono::Utc::now() + chrono::Duration::minutes(10));
        let mut a2 = make_agent("a2");
        a2.expires_at = Some(chrono::Utc::now() + chrono::Duration::minutes(5));
        let a3 = make_agent("a3"); // no expiry
        super::append_temporal_agents_section(&mut status, &[a1, a2, a3]);
        assert!(status.contains("a1"));
        assert!(status.contains("a2"));
        assert!(!status.contains("a3"));
    }

    // ── loop_run_blocker ───────────────────────────────────────────

    #[test]
    fn blocker_present() {
        let run = LoopNodeRun {
            id: "r1".into(),
            loop_id: "l1".into(),
            spec_id: "s1".into(),
            node_id: "n1".into(),
            status: LoopRunStatus::Fail,
            input: None,
            output: Some(serde_json::json!({"blocker": "needs review"})),
            started_at: chrono::Utc::now(),
            completed_at: Some(chrono::Utc::now()),
            iteration: 1,
            pid: None,
            boot_id: None,
            session_id: None,
        };
        assert_eq!(
            super::loop_run_blocker(&run).as_deref(),
            Some("needs review")
        );
    }

    #[test]
    fn blocker_no_output() {
        let run = LoopNodeRun {
            id: "r2".into(),
            loop_id: "l1".into(),
            spec_id: "s1".into(),
            node_id: "n1".into(),
            status: LoopRunStatus::Pass,
            input: None,
            output: None,
            started_at: chrono::Utc::now(),
            completed_at: Some(chrono::Utc::now()),
            iteration: 1,
            pid: None,
            boot_id: None,
            session_id: None,
        };
        assert!(super::loop_run_blocker(&run).is_none());
    }

    #[test]
    fn blocker_no_blocker_key() {
        let run = LoopNodeRun {
            id: "r3".into(),
            loop_id: "l1".into(),
            spec_id: "s1".into(),
            node_id: "n1".into(),
            status: LoopRunStatus::Fail,
            input: None,
            output: Some(serde_json::json!({"result": "failed"})),
            started_at: chrono::Utc::now(),
            completed_at: Some(chrono::Utc::now()),
            iteration: 1,
            pid: None,
            boot_id: None,
            session_id: None,
        };
        assert!(super::loop_run_blocker(&run).is_none());
    }

    #[test]
    fn blocker_non_string_value() {
        let run = LoopNodeRun {
            id: "r4".into(),
            loop_id: "l1".into(),
            spec_id: "s1".into(),
            node_id: "n1".into(),
            status: LoopRunStatus::Fail,
            input: None,
            output: Some(serde_json::json!({"blocker": 42})),
            started_at: chrono::Utc::now(),
            completed_at: Some(chrono::Utc::now()),
            iteration: 1,
            pid: None,
            boot_id: None,
            session_id: None,
        };
        assert!(super::loop_run_blocker(&run).is_none());
    }

    // ── loop_node_json ─────────────────────────────────────────────

    #[test]
    fn node_json_agent() {
        let node = LoopNode {
            id: "n1".into(),
            spec_id: Some("s1".into()),
            loop_id: None,
            name: "implement".into(),
            kind: LoopNodeKind::Agent,
            config: serde_json::json!({"platform": "claude"}),
            position: 5,
            created_at: chrono::Utc::now(),
        };
        let json = super::loop_node_json(&node);
        assert_eq!(json["id"], "n1");
        assert_eq!(json["spec_id"], "s1");
        assert!(json["loop_id"].is_null());
        assert_eq!(json["kind"], "agent");
        assert_eq!(json["position"], 5);
    }

    #[test]
    fn node_json_join_displays_quorum() {
        let node = LoopNode {
            id: "j1".into(),
            spec_id: None,
            loop_id: Some("l1".into()),
            name: "quorum".into(),
            kind: LoopNodeKind::Join,
            config: serde_json::json!({}),
            position: 10,
            created_at: chrono::Utc::now(),
        };
        let json = super::loop_node_json(&node);
        assert_eq!(json["kind"], "quorum");
    }

    // ── loop_edge_json ─────────────────────────────────────────────

    #[test]
    fn edge_json_all_fields() {
        let edge = LoopEdge {
            id: "e1".into(),
            spec_id: Some("s1".into()),
            loop_id: None,
            from_node: "n1".into(),
            to_node: "n2".into(),
            condition: LoopEdgeCondition::Pass,
        };
        let json = super::loop_edge_json(&edge);
        assert_eq!(json["id"], "e1");
        assert_eq!(json["from_node"], "n1");
        assert_eq!(json["to_node"], "n2");
        assert_eq!(json["condition"], "pass");
    }

    #[test]
    fn edge_json_loop_owner() {
        let edge = LoopEdge {
            id: "e2".into(),
            spec_id: None,
            loop_id: Some("l1".into()),
            from_node: "n1".into(),
            to_node: "n2".into(),
            condition: LoopEdgeCondition::Always,
        };
        let json = super::loop_edge_json(&edge);
        assert_eq!(json["loop_id"], "l1");
        assert!(json["spec_id"].is_null());
        assert_eq!(json["condition"], "always");
    }

    // ── loop_run_json ──────────────────────────────────────────────

    #[test]
    fn run_json_pass_with_output() {
        let run = LoopNodeRun {
            id: "r1".into(),
            loop_id: "l1".into(),
            spec_id: "s1".into(),
            node_id: "n1".into(),
            status: LoopRunStatus::Pass,
            input: Some(serde_json::json!({"key": "val"})),
            output: Some(serde_json::json!({"result": "ok"})),
            started_at: chrono::Utc::now(),
            completed_at: Some(chrono::Utc::now()),
            iteration: 3,
            pid: None,
            boot_id: None,
            session_id: None,
        };
        let json = super::loop_run_json(&run);
        assert_eq!(json["status"], "pass");
        assert_eq!(json["iteration"], 3);
        assert_eq!(json["input"]["key"], "val");
        assert_eq!(json["output"]["result"], "ok");
    }

    #[test]
    fn run_json_running_no_completed() {
        let run = LoopNodeRun {
            id: "r2".into(),
            loop_id: "l1".into(),
            spec_id: "s1".into(),
            node_id: "n1".into(),
            status: LoopRunStatus::Running,
            input: None,
            output: None,
            started_at: chrono::Utc::now(),
            completed_at: None,
            iteration: 1,
            pid: None,
            boot_id: None,
            session_id: None,
        };
        let json = super::loop_run_json(&run);
        assert!(json["completed_at"].is_null());
    }

    #[test]
    fn run_json_fail() {
        let run = LoopNodeRun {
            id: "r3".into(),
            loop_id: "l1".into(),
            spec_id: "s1".into(),
            node_id: "n1".into(),
            status: LoopRunStatus::Fail,
            input: None,
            output: Some(serde_json::json!({"blocker": "stuck"})),
            started_at: chrono::Utc::now(),
            completed_at: Some(chrono::Utc::now()),
            iteration: 2,
            pid: None,
            boot_id: None,
            session_id: None,
        };
        let json = super::loop_run_json(&run);
        assert_eq!(json["status"], "fail");
        assert_eq!(json["iteration"], 2);
    }

    // ── ensemble_details_json ──────────────────────────────────────

    #[test]
    fn ensemble_json_full() {
        let details = EnsembleDetails {
            ensemble: Ensemble {
                id: "ens1".into(),
                spec_id: Some("s1".into()),
                loop_id: None,
                name: "proposers".into(),
                prompt_template: "draft".into(),
                join_node_id: "j1".into(),
                entry_from_node: "kickoff".into(),
                entry_condition: LoopEdgeCondition::Always,
                min_pass: 2,
                straggler_timeout_minutes: Some(10),
                timeout_minutes: 30,
                on_pass_to: "arbiter".into(),
                on_fail_to: Some("cleanup".into()),
                created_at: chrono::Utc::now(),
            },
            members: vec![
                EnsembleMember {
                    ensemble_id: "ens1".into(),
                    node_id: "m1".into(),
                    position: 0,
                    platform: "claude".into(),
                    model: None,
                },
                EnsembleMember {
                    ensemble_id: "ens1".into(),
                    node_id: "m2".into(),
                    position: 1,
                    platform: "codex".into(),
                    model: Some("o1".into()),
                },
            ],
        };
        let json = super::ensemble_details_json(&details);
        assert_eq!(json["id"], "ens1");
        assert_eq!(json["min_pass"], 2);
        assert_eq!(json["effective_straggler_timeout_minutes"], 10);
        assert_eq!(json["on_fail_to"], "cleanup");
        assert_eq!(json["members"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn ensemble_json_no_straggler() {
        let details = EnsembleDetails {
            ensemble: Ensemble {
                id: "ens2".into(),
                spec_id: None,
                loop_id: Some("l1".into()),
                name: "t".into(),
                prompt_template: "t".into(),
                join_node_id: "j".into(),
                entry_from_node: "f".into(),
                entry_condition: LoopEdgeCondition::Always,
                min_pass: 1,
                straggler_timeout_minutes: None,
                timeout_minutes: 45,
                on_pass_to: "to".into(),
                on_fail_to: None,
                created_at: chrono::Utc::now(),
            },
            members: vec![EnsembleMember {
                ensemble_id: "ens2".into(),
                node_id: "m1".into(),
                position: 0,
                platform: "claude".into(),
                model: None,
            }],
        };
        let json = super::ensemble_details_json(&details);
        assert_eq!(json["effective_straggler_timeout_minutes"], 45);
        assert!(json["on_fail_to"].is_null());
    }

    // ── loop_completion_hook_json ──────────────────────────────────

    #[test]
    fn hook_json_full() {
        let hook = LoopCompletionHook {
            platform: "claude".into(),
            model: Some("opus-4".into()),
            prompt: "{{loop_name}} done".into(),
            timeout_minutes: Some(10),
        };
        let json = super::loop_completion_hook_json(&hook);
        assert_eq!(json["platform"], "claude");
        assert_eq!(json["model"], "opus-4");
        assert_eq!(json["timeout_minutes"], 10);
    }

    #[test]
    fn hook_json_no_model() {
        let hook = LoopCompletionHook {
            platform: "mimo".into(),
            model: None,
            prompt: "t".into(),
            timeout_minutes: None,
        };
        let json = super::loop_completion_hook_json(&hook);
        assert!(json["model"].is_null());
        assert!(json["timeout_minutes"].is_null());
    }

    // ── loop_completion_hook_run_json ──────────────────────────────

    #[test]
    fn hook_run_json_full() {
        let run = LoopCompletionHookRun {
            id: "chr1".into(),
            loop_id: "l1".into(),
            status: LoopRunStatus::Pass,
            output: Some(serde_json::json!({"summary": "done"})),
            summary: Some("ok".into()),
            started_at: chrono::Utc::now(),
            completed_at: Some(chrono::Utc::now()),
            pid: Some(123),
            boot_id: None,
        };
        let json = super::loop_completion_hook_run_json(&run);
        assert_eq!(json["id"], "chr1");
        assert_eq!(json["status"], "pass");
        assert_eq!(json["summary"], "ok");
    }

    #[test]
    fn hook_run_json_no_completed() {
        let run = LoopCompletionHookRun {
            id: "chr2".into(),
            loop_id: "l1".into(),
            status: LoopRunStatus::Running,
            output: None,
            summary: None,
            started_at: chrono::Utc::now(),
            completed_at: None,
            pid: None,
            boot_id: None,
        };
        let json = super::loop_completion_hook_run_json(&run);
        assert!(json["completed_at"].is_null());
        assert!(json["summary"].is_null());
    }

    // ── format_system_time ─────────────────────────────────────────

    #[test]
    fn format_system_time_now() {
        let formatted = super::format_system_time(std::time::SystemTime::now());
        assert!(chrono::DateTime::parse_from_rfc3339(&formatted).is_ok());
    }

    // ── model_result_footer ────────────────────────────────────────

    #[test]
    fn footer_live_source() {
        use crate::domain::models_db::CatalogSource;
        let f = super::model_result_footer(
            "Models:",
            CatalogSource::Live,
            std::time::SystemTime::now(),
        );
        assert!(f.contains("Source: live"));
        assert!(!f.contains("out of date"));
    }

    #[test]
    fn footer_stale_source() {
        use crate::domain::models_db::CatalogSource;
        let f = super::model_result_footer(
            "Models:",
            CatalogSource::Stale,
            std::time::SystemTime::now(),
        );
        assert!(f.contains("Source: stale"));
        assert!(f.contains("out of date"));
    }

    // ── build_ensemble_unit: with and without on_fail_to ───────────

    #[test]
    fn ensemble_unit_with_fail_to() {
        let members = vec![("claude".into(), None), ("codex".into(), Some("o1".into()))];
        let built = build_ensemble_unit(&EnsembleUnitSpec {
            spec_id: Some("s1".into()),
            loop_id: None,
            name: "failing",
            prompt_template: "do it",
            members: &members,
            entry_from_node: "kickoff",
            entry_condition: LoopEdgeCondition::Always,
            on_pass_to: "arbiter",
            on_fail_to: Some("cleanup"),
            min_pass: 1,
            timeout_minutes: 30,
            straggler_timeout_minutes: Some(10),
            start_position: 1,
        });
        assert_eq!(built.ensemble.on_fail_to.as_deref(), Some("cleanup"));
        assert_eq!(built.ensemble.straggler_timeout_minutes, Some(10));
        let fail_edges: Vec<_> = built
            .edges
            .iter()
            .filter(|e| e.condition == LoopEdgeCondition::Fail)
            .collect();
        assert_eq!(fail_edges.len(), 1);
    }

    #[test]
    fn ensemble_unit_no_fail_to() {
        let members = vec![("claude".into(), None)];
        let built = build_ensemble_unit(&EnsembleUnitSpec {
            spec_id: None,
            loop_id: Some("l1".into()),
            name: "no-fail",
            prompt_template: "p",
            members: &members,
            entry_from_node: "start",
            entry_condition: LoopEdgeCondition::Pass,
            on_pass_to: "end",
            on_fail_to: None,
            min_pass: 1,
            timeout_minutes: 15,
            straggler_timeout_minutes: None,
            start_position: 5,
        });
        assert!(built.ensemble.on_fail_to.is_none());
        let fail_edges: Vec<_> = built
            .edges
            .iter()
            .filter(|e| e.condition == LoopEdgeCondition::Fail)
            .collect();
        assert!(fail_edges.is_empty());
    }

    #[test]
    fn ensemble_unit_positions_sequential() {
        let members = vec![
            ("p1".into(), None),
            ("p2".into(), None),
            ("p3".into(), None),
        ];
        let built = build_ensemble_unit(&EnsembleUnitSpec {
            spec_id: None,
            loop_id: Some("l1".into()),
            name: "pos",
            prompt_template: "p",
            members: &members,
            entry_from_node: "start",
            entry_condition: LoopEdgeCondition::Always,
            on_pass_to: "end",
            on_fail_to: None,
            min_pass: 3,
            timeout_minutes: 30,
            straggler_timeout_minutes: None,
            start_position: 10,
        });
        for (i, member) in built.members.iter().enumerate() {
            assert_eq!(member.position, i as i64);
        }
        for (i, node) in built.member_nodes.iter().enumerate() {
            assert_eq!(node.position, 10 + i as i64);
        }
        assert_eq!(built.join_node.position, 13);
    }

    // ── validate_position_conflict: no loop and empty loop ─────────

    #[test]
    fn position_conflict_no_loop() {
        let dir = tempdir().unwrap();
        let db = Database::new(&dir.path().join("test.db")).unwrap();
        assert!(validate_position_conflict(&db, None, "spec-x", 1).is_ok());
    }

    #[test]
    fn position_conflict_empty_loop() {
        let dir = tempdir().unwrap();
        let db = Database::new(&dir.path().join("test.db")).unwrap();
        insert_test_loop(&db, "loop-empty");
        assert!(validate_position_conflict(&db, Some("loop-empty"), "spec-x", 0).is_ok());
    }

    // ── validate_node_position_conflict: edge cases ────────────────

    #[test]
    fn node_position_no_owner() {
        let dir = tempdir().unwrap();
        let db = Database::new(&dir.path().join("test.db")).unwrap();
        let node = LoopNode {
            id: "n1".into(),
            spec_id: None,
            loop_id: None,
            name: "n1".into(),
            kind: LoopNodeKind::Check,
            config: serde_json::json!({}),
            position: 1,
            created_at: chrono::Utc::now(),
        };
        assert!(validate_node_position_conflict(&db, &node, "n1", 1).is_ok());
    }

    #[test]
    fn node_position_empty_siblings() {
        let dir = tempdir().unwrap();
        let db = Database::new(&dir.path().join("test.db")).unwrap();
        db.insert_loop_spec(&standalone_spec("s1")).unwrap();
        let node = LoopNode {
            id: "n1".into(),
            spec_id: Some("s1".into()),
            loop_id: None,
            name: "n1".into(),
            kind: LoopNodeKind::Agent,
            config: serde_json::json!({"platform": "claude"}),
            position: 1,
            created_at: chrono::Utc::now(),
        };
        assert!(validate_node_position_conflict(&db, &node, "n1", 5).is_ok());
    }

    #[test]
    fn node_position_no_conflict_different_position() {
        let dir = tempdir().unwrap();
        let db = Database::new(&dir.path().join("test.db")).unwrap();
        db.insert_loop_spec(&standalone_spec("s1")).unwrap();
        db.insert_loop_node(&LoopNode {
            id: "n1".into(),
            spec_id: Some("s1".into()),
            loop_id: None,
            name: "n1".into(),
            kind: LoopNodeKind::Agent,
            config: serde_json::json!({"platform": "claude"}),
            position: 1,
            created_at: chrono::Utc::now(),
        })
        .unwrap();
        let node = LoopNode {
            id: "n2".into(),
            spec_id: Some("s1".into()),
            loop_id: None,
            name: "n2".into(),
            kind: LoopNodeKind::Check,
            config: serde_json::json!({"command": "true"}),
            position: 5,
            created_at: chrono::Utc::now(),
        };
        assert!(validate_node_position_conflict(&db, &node, "n2", 2).is_ok());
    }

    // ── validate_pool_not_consumed edge cases ──────────────────────

    #[test]
    fn pool_not_consumed_empty_pool() {
        let dir = tempdir().unwrap();
        let db = Database::new(&dir.path().join("test.db")).unwrap();
        db.insert_pool(&Pool {
            id: "pool-e".into(),
            name: "empty".into(),
            created_at: chrono::Utc::now(),
        })
        .unwrap();
        assert!(validate_pool_not_consumed(&db, "pool-e", "loop-1").is_ok());
    }

    #[test]
    fn pool_not_consumed_pending_spec() {
        let dir = tempdir().unwrap();
        let db = Database::new(&dir.path().join("test.db")).unwrap();
        db.insert_loop_spec(&standalone_spec("s1")).unwrap();
        db.insert_pool(&Pool {
            id: "pool-p".into(),
            name: "pool-p".into(),
            created_at: chrono::Utc::now(),
        })
        .unwrap();
        db.append_pool_member("pool-p", "s1", None).unwrap();
        assert!(validate_pool_not_consumed(&db, "pool-p", "loop-other").is_ok());
    }

    #[test]
    fn pool_not_consumed_own_loop() {
        let dir = tempdir().unwrap();
        let db = Database::new(&dir.path().join("test.db")).unwrap();
        db.insert_loop_spec(&running_spec("s-owned")).unwrap();
        db.insert_loop(&make_loop("loop-owner", LoopStatus::Running))
            .unwrap();
        db.insert_pool(&Pool {
            id: "pool-o".into(),
            name: "pool-o".into(),
            created_at: chrono::Utc::now(),
        })
        .unwrap();
        db.append_pool_member("pool-o", "s-owned", None).unwrap();
        insert_test_node(&db, "node-1", "s-owned");
        db.insert_loop_run(&loop_run_row(
            "run1",
            "loop-owner",
            "s-owned",
            LoopRunStatus::Running,
        ))
        .unwrap();
        assert!(validate_pool_not_consumed(&db, "pool-o", "loop-owner").is_ok());
    }

    // ── validate_pool_member_removable edge cases ──────────────────

    #[test]
    fn pool_member_removable_pending() {
        let dir = tempdir().unwrap();
        let db = Database::new(&dir.path().join("test.db")).unwrap();
        db.insert_loop_spec(&standalone_spec("s1")).unwrap();
        db.insert_pool(&Pool {
            id: "pool-r".into(),
            name: "pool-r".into(),
            created_at: chrono::Utc::now(),
        })
        .unwrap();
        assert!(validate_pool_member_removable(&db, "pool-r", "s1").is_ok());
    }

    #[test]
    fn pool_member_removable_nonexistent() {
        let dir = tempdir().unwrap();
        let db = Database::new(&dir.path().join("test.db")).unwrap();
        db.insert_pool(&Pool {
            id: "pool-r".into(),
            name: "pool-r".into(),
            created_at: chrono::Utc::now(),
        })
        .unwrap();
        assert!(validate_pool_member_removable(&db, "pool-r", "ghost").is_ok());
    }

    // ── resolve_reported_run: stale statuses ───────────────────────

    #[test]
    fn reported_run_rejects_pass_status() {
        let dir = tempdir().unwrap();
        let db = Database::new(&dir.path().join("test.db")).unwrap();
        insert_test_loop(&db, "l1");
        db.insert_loop_spec(&standalone_spec("s1")).unwrap();
        insert_test_node(&db, "node-1", "s1");
        db.insert_loop_run(&loop_run_row("r1", "l1", "s1", LoopRunStatus::Pass))
            .unwrap();
        let result = resolve_reported_run(&db, "r1", "n1").unwrap();
        assert!(result
            .expect_err("pass run must be stale")
            .is_error
            .unwrap_or(false));
    }

    #[test]
    fn reported_run_rejects_fail_status() {
        let dir = tempdir().unwrap();
        let db = Database::new(&dir.path().join("test.db")).unwrap();
        insert_test_loop(&db, "l1");
        db.insert_loop_spec(&standalone_spec("s1")).unwrap();
        insert_test_node(&db, "node-1", "s1");
        db.insert_loop_run(&loop_run_row("r1", "l1", "s1", LoopRunStatus::Fail))
            .unwrap();
        let result = resolve_reported_run(&db, "r1", "n1").unwrap();
        assert!(result
            .expect_err("fail run must be stale")
            .is_error
            .unwrap_or(false));
    }

    // ── validate_pool_reorder_locking edge cases ───────────────────

    #[test]
    fn reorder_locking_all_pending() {
        let dir = tempdir().unwrap();
        let db = Database::new(&dir.path().join("test.db")).unwrap();
        db.insert_loop_spec(&standalone_spec("a")).unwrap();
        db.insert_loop_spec(&standalone_spec("b")).unwrap();
        db.insert_loop_spec(&standalone_spec("c")).unwrap();
        db.insert_pool(&Pool {
            id: "p1".into(),
            name: "p1".into(),
            created_at: chrono::Utc::now(),
        })
        .unwrap();
        for id in ["a", "b", "c"] {
            db.append_pool_member("p1", id, None).unwrap();
        }
        let current = db.list_pool_member_spec_ids("p1").unwrap();
        let order = vec!["c".into(), "a".into(), "b".into()];
        assert!(validate_pool_reorder_locking(&db, &current, &order).is_ok());
    }

    #[test]
    fn reorder_locking_failed_spec_locked() {
        let dir = tempdir().unwrap();
        let db = Database::new(&dir.path().join("test.db")).unwrap();
        let mut f = standalone_spec("f");
        f.status = LoopSpecStatus::Failed;
        db.insert_loop_spec(&f).unwrap();
        db.insert_loop_spec(&standalone_spec("p")).unwrap();
        db.insert_pool(&Pool {
            id: "p1".into(),
            name: "p1".into(),
            created_at: chrono::Utc::now(),
        })
        .unwrap();
        db.append_pool_member("p1", "f", None).unwrap();
        db.append_pool_member("p1", "p", None).unwrap();
        let current = db.list_pool_member_spec_ids("p1").unwrap();
        let order = vec!["p".into(), "f".into()];
        let err = validate_pool_reorder_locking(&db, &current, &order).unwrap_err();
        assert!(err.contains("failed"), "{err}");
    }

    #[test]
    fn reorder_locking_skipped_spec_locked() {
        let dir = tempdir().unwrap();
        let db = Database::new(&dir.path().join("test.db")).unwrap();
        let mut s = standalone_spec("s");
        s.status = LoopSpecStatus::Skipped;
        db.insert_loop_spec(&s).unwrap();
        db.insert_loop_spec(&standalone_spec("p")).unwrap();
        db.insert_pool(&Pool {
            id: "p1".into(),
            name: "p1".into(),
            created_at: chrono::Utc::now(),
        })
        .unwrap();
        db.append_pool_member("p1", "s", None).unwrap();
        db.append_pool_member("p1", "p", None).unwrap();
        let current = db.list_pool_member_spec_ids("p1").unwrap();
        let order = vec!["p".into(), "s".into()];
        let err = validate_pool_reorder_locking(&db, &current, &order).unwrap_err();
        assert!(err.contains("skipped"), "{err}");
    }

    #[test]
    fn reorder_locking_mixed_pending_and_running() {
        let dir = tempdir().unwrap();
        let db = Database::new(&dir.path().join("test.db")).unwrap();
        db.insert_loop_spec(&running_spec("r")).unwrap();
        db.insert_loop_spec(&standalone_spec("p1")).unwrap();
        db.insert_loop_spec(&standalone_spec("p2")).unwrap();
        db.insert_pool(&Pool {
            id: "p1".into(),
            name: "p1".into(),
            created_at: chrono::Utc::now(),
        })
        .unwrap();
        db.append_pool_member("p1", "r", None).unwrap();
        db.append_pool_member("p1", "p1", None).unwrap();
        db.append_pool_member("p1", "p2", None).unwrap();
        let current = db.list_pool_member_spec_ids("p1").unwrap();
        let order = vec!["r".into(), "p2".into(), "p1".into()];
        assert!(validate_pool_reorder_locking(&db, &current, &order).is_ok());
    }

    // ── validate_pool_reorder: all permutations ────────────────────

    #[test]
    fn pool_reorder_all_perms_of_three() {
        let current = vec!["a".into(), "b".into(), "c".into()];
        for perm in [
            ["a", "b", "c"],
            ["a", "c", "b"],
            ["b", "a", "c"],
            ["b", "c", "a"],
            ["c", "a", "b"],
            ["c", "b", "a"],
        ] {
            let reordered: Vec<String> = perm.into_iter().map(String::from).collect();
            assert!(validate_pool_reorder(&current, &reordered).is_ok());
        }
    }

    #[test]
    fn pool_reorder_empty() {
        assert!(validate_pool_reorder(&[], &[]).is_ok());
    }

    #[test]
    fn pool_reorder_large_pool() {
        let current: Vec<String> = (0..100).map(|i| format!("s{i}")).collect();
        let mut reordered = current.clone();
        reordered.reverse();
        assert!(validate_pool_reorder(&current, &reordered).is_ok());
    }

    // ── validate_ensemble_members: boundaries ──────────────────────

    #[test]
    fn ensemble_exactly_min() {
        let m: Vec<EnsembleMemberParams> = (0..2)
            .map(|i| EnsembleMemberParams {
                platform: format!("p{i}"),
                model: None,
            })
            .collect();
        assert!(validate_ensemble_members(&m).is_ok());
    }

    #[test]
    fn ensemble_exactly_max() {
        let m: Vec<EnsembleMemberParams> = (0..8)
            .map(|i| EnsembleMemberParams {
                platform: format!("p{i}"),
                model: None,
            })
            .collect();
        assert!(validate_ensemble_members(&m).is_ok());
    }

    #[test]
    fn ensemble_above_max() {
        let m: Vec<EnsembleMemberParams> = (0..9)
            .map(|i| EnsembleMemberParams {
                platform: format!("p{i}"),
                model: None,
            })
            .collect();
        assert!(validate_ensemble_members(&m).unwrap_err().contains("2-8"));
    }

    #[test]
    fn ensemble_empty_model_becomes_none() {
        let m = vec![
            EnsembleMemberParams {
                platform: "claude".into(),
                model: Some("".into()),
            },
            EnsembleMemberParams {
                platform: "mimo".into(),
                model: None,
            },
        ];
        let result = validate_ensemble_members(&m).unwrap();
        assert_eq!(result[0].1, None);
        assert_eq!(result[1].1, None);
    }

    // ── validate_node_config: gate edge cases ─────────────────────

    #[test]
    fn gate_empty_evaluate_and_value() {
        let config = serde_json::json!({"evaluate": "", "value": ""});
        validate_node_config(LoopNodeKind::Gate, &config).unwrap();
    }

    // ── spec_summary_json: all statuses ────────────────────────────

    #[test]
    fn spec_summary_all_statuses() {
        for status in [
            LoopSpecStatus::Pending,
            LoopSpecStatus::Running,
            LoopSpecStatus::Completed,
            LoopSpecStatus::Failed,
            LoopSpecStatus::Skipped,
        ] {
            let mut spec = standalone_spec("s");
            spec.status = status;
            let json = spec_summary_json(&spec);
            assert_eq!(json["status"], status.as_str());
        }
    }

    // ── validate_non_empty: unicode and special chars ───────────────

    #[test]
    fn non_empty_unicode() {
        assert!(validate_non_empty("こんにちは", "f").is_ok());
    }

    #[test]
    fn non_empty_mixed_whitespace() {
        assert!(validate_non_empty(" \t\n ", "f").is_err());
    }

    // ── validate_absolute_dir: trailing slash ──────────────────────

    #[test]
    fn absolute_dir_trailing_slash() {
        let dir = tempdir().unwrap();
        let path = format!("{}/", dir.path().to_string_lossy());
        assert!(validate_absolute_dir(&path).is_ok());
    }

    // ── validate_spec_set_status_target: all valid ─────────────────

    #[test]
    fn set_status_target_all_valid() {
        assert!(validate_spec_set_status_target("pending").is_ok());
        assert!(validate_spec_set_status_target("completed").is_ok());
        assert!(validate_spec_set_status_target("skipped").is_ok());
    }

    // ── validate_at_least_one_bool: mixed ──────────────────────────

    #[test]
    fn at_least_one_mixed() {
        assert!(validate_at_least_one_bool(&[false, true, false, true], "f").is_ok());
        assert!(validate_at_least_one_bool(&[false, false, false, false], "f").is_err());
    }

    // ── build_id_result: various keys ──────────────────────────────

    #[test]
    fn id_result_spec_id() {
        let r = build_id_result("abc", "spec_id");
        let t = format!("{:?}", r.content);
        assert!(t.contains("spec_id") && t.contains("abc"));
    }

    #[test]
    fn id_result_ensemble_id() {
        let r = build_id_result("ens-1", "ensemble_id");
        let t = format!("{:?}", r.content);
        assert!(t.contains("ensemble_id") && t.contains("ens-1"));
    }

    #[test]
    fn id_result_queue_id() {
        let r = build_id_result("q-1", "queue_id");
        let t = format!("{:?}", r.content);
        assert!(t.contains("queue_id") && t.contains("q-1"));
    }

    // ── build_json_result: nested ──────────────────────────────────

    #[test]
    fn json_result_nested() {
        let v = serde_json::json!({"mapping": {"old": "new"}, "wired": true});
        let r = build_json_result(&v);
        let t = format!("{:?}", r.content);
        assert!(t.contains("mapping") && t.contains("old"));
    }

    // ── member_node_config edge cases ──────────────────────────────

    #[test]
    fn member_config_zero_timeout() {
        let c = member_node_config("claude", None, "p", 0);
        assert_eq!(c["timeout_minutes"], 0);
    }

    // ── resolve_graph_target: whitespace-only ──────────────────────

    #[test]
    fn graph_target_whitespace_both() {
        let dir = tempdir().unwrap();
        let db = Database::new(&dir.path().join("test.db")).unwrap();
        let err = resolve_graph_target(&db, Some("   "), Some("  ")).unwrap_err();
        assert!(err.contains("exactly one"), "{err}");
    }

    // ── rag_result_json: negative distance ─────────────────────────

    #[test]
    fn rag_negative_distance() {
        use crate::rag::vector_store::SearchResult;
        let r = SearchResult {
            id: "sr-neg".into(),
            file_path: "/t.md".into(),
            content: "c".into(),
            created_at: 0,
            distance: Some(-0.5),
        };
        let json = rag_result_json(&r);
        assert!(json["distance"].as_f64().unwrap() < 0.0);
    }

    // ── node_copy_note edge cases ──────────────────────────────────

    #[test]
    fn copy_note_wired() {
        let n = node_copy_note("src", "dst", true);
        assert!(n.contains("src") && n.contains("dst") && !n.contains("Unwired"));
    }

    #[test]
    fn copy_note_unwired() {
        let n = node_copy_note("src", "dst", false);
        assert!(n.contains("Unwired") && n.contains("loop_add_edge"));
    }
}

// ── Direct #[tool] handler-method coverage ───────────────────────────
//
// The three test modules above almost exclusively exercise *pure* helper
// functions (validate_*, build_*, format_*). The `#[tool]` methods on
// `TaskTriggerHandler` itself — `task_add`, `task_watch`, `sync_*`,
// `intelligence_*`, `loop_*`, `spec_*`, `blueprint_*`, etc. — are called
// directly here instead: construct `Parameters<...>`, call the method on a
// real handler wired to a real (tempdir) SQLite database, assert on the
// `CallToolResult` and on the resulting DB state. This is the surface a
// live MCP client actually calls, and where most of this file's uncovered
// lines live (error branches inside each handler in particular).
#[cfg(test)]
mod endpoint_tests {
    use super::*;
    use crate::application::notification_service::{
        DefaultNotificationService, NotificationService,
    };
    use crate::db::Database;
    use crate::domain::models::{Agent, Cli, RunLog, RunStatus, TriggerType};
    use crate::executor::Executor;
    use crate::loop_engine::LoopEngine;
    use crate::rag::ingestion::IngestionManager;
    use crate::sync_manager::SyncManager;
    use crate::watchers::WatcherEngine;
    use rmcp::handler::server::wrapper::Parameters;
    use tempfile::tempdir;
    use tokio::sync::Notify;

    /// RAII guard: sets the real `$HOME` env var to `path` for the test
    /// body, restoring the previous value on drop. `make_log_path` /
    /// `data_dir()` read `dirs::home_dir()` directly (real `$HOME`, not the
    /// `CANOPY_HOME_OVERRIDE` some other subsystems honor), so any handler
    /// path that writes a log file needs this to avoid touching the real
    /// developer's `~/.canopy`. Safe under `cargo nextest` (one process per
    /// test) but would race under plain `cargo test`.
    struct HomeVar {
        prev: Option<std::ffi::OsString>,
    }

    impl HomeVar {
        fn set(path: &std::path::Path) -> Self {
            let prev = std::env::var_os("HOME");
            unsafe { std::env::set_var("HOME", path) };
            HomeVar { prev }
        }
    }

    impl Drop for HomeVar {
        fn drop(&mut self) {
            match &self.prev {
                Some(v) => unsafe { std::env::set_var("HOME", v) },
                None => unsafe { std::env::remove_var("HOME") },
            }
        }
    }

    fn endpoint_test_handler() -> (
        tempfile::TempDir,
        std::sync::Arc<Database>,
        TaskTriggerHandler,
    ) {
        let dir = tempdir().unwrap();
        let db = Arc::new(Database::new(&dir.path().join("test.db")).unwrap());
        let notif: Arc<dyn NotificationService> = Arc::new(DefaultNotificationService);
        let executor = Arc::new(Executor::new(Arc::clone(&db), Arc::clone(&notif)));
        let sync_manager = Arc::new(SyncManager::new(Arc::clone(&db)));
        let loop_engine = Arc::new(LoopEngine::new(Arc::clone(&db), Arc::clone(&notif)));
        let watcher_engine = Arc::new(WatcherEngine::new(
            Arc::clone(&db),
            Arc::clone(&executor),
            Arc::clone(&loop_engine),
        ));
        let ingestion = Arc::new(IngestionManager::new(
            Arc::clone(&db),
            dir.path().to_path_buf(),
        ));
        let dynamic_skills = Arc::new(crate::dynamic_skills::SkillStore::new(
            dir.path().join("skills"),
            Vec::new(),
            15,
        ));
        let handler = TaskTriggerHandler::new(
            Arc::clone(&db),
            executor,
            watcher_engine,
            Arc::new(Notify::new()),
            loop_engine,
            notif,
            sync_manager,
            ingestion,
            dynamic_skills,
            0,
        );
        (dir, db, handler)
    }

    fn text(result: &CallToolResult) -> String {
        format!("{:?}", result.content)
    }

    fn is_err(result: &CallToolResult) -> bool {
        result.is_error == Some(true)
    }

    // ── task_add / agent_add ─────────────────────────────────────

    #[tokio::test]
    async fn task_add_registers_a_cron_agent() {
        let home = tempdir().unwrap();
        let _home_guard = HomeVar::set(home.path());
        let (_dir, db, handler) = endpoint_test_handler();

        let result = handler
            .task_add(Parameters(TaskAddParams {
                id: "agt-1".to_string(),
                prompt: "run the tests".to_string(),
                schedule: "*/5 * * * *".to_string(),
                cli: Some("opencode".to_string()),
                model: None,
                duration_minutes: None,
                working_dir: None,
                timeout_minutes: None,
            }))
            .await
            .unwrap();

        assert!(!is_err(&result), "{}", text(&result));
        assert!(text(&result).contains("registered with schedule"));
        let stored = db.get_agent("agt-1").unwrap().unwrap();
        assert!(matches!(stored.trigger, Some(Trigger::Cron { .. })));
    }

    #[tokio::test]
    async fn task_add_rejects_invalid_cron_expression() {
        let home = tempdir().unwrap();
        let _home_guard = HomeVar::set(home.path());
        let (_dir, db, handler) = endpoint_test_handler();

        let result = handler
            .task_add(Parameters(TaskAddParams {
                id: "agt-bad-cron".to_string(),
                prompt: "run the tests".to_string(),
                schedule: "not a cron expr!".to_string(),
                cli: Some("opencode".to_string()),
                model: None,
                duration_minutes: None,
                working_dir: None,
                timeout_minutes: None,
            }))
            .await
            .unwrap();

        assert!(is_err(&result));
        assert!(text(&result).contains("Invalid cron expression"));
        assert!(db.get_agent("agt-bad-cron").unwrap().is_none());
    }

    #[tokio::test]
    async fn task_add_rejects_invalid_id() {
        let home = tempdir().unwrap();
        let _home_guard = HomeVar::set(home.path());
        let (_dir, _db, handler) = endpoint_test_handler();

        let result = handler
            .task_add(Parameters(TaskAddParams {
                id: "bad id with spaces!".to_string(),
                prompt: "run the tests".to_string(),
                schedule: "*/5 * * * *".to_string(),
                cli: Some("opencode".to_string()),
                model: None,
                duration_minutes: None,
                working_dir: None,
                timeout_minutes: None,
            }))
            .await
            .unwrap();

        assert!(is_err(&result));
        assert!(text(&result).contains("alphanumeric"));
    }

    // ── task_watch / agent_watch ─────────────────────────────────

    #[tokio::test]
    async fn task_watch_registers_a_watcher() {
        let home = tempdir().unwrap();
        let _home_guard = HomeVar::set(home.path());
        let (dir, db, handler) = endpoint_test_handler();

        let result = handler
            .task_watch(Parameters(TaskWatchParams {
                id: "watch-1".to_string(),
                path: dir.path().to_string_lossy().to_string(),
                events: vec!["modify".to_string(), "create".to_string()],
                prompt: "react to changes".to_string(),
                cli: Some("opencode".to_string()),
                model: None,
                debounce_seconds: None,
                recursive: Some(true),
                timeout_minutes: None,
            }))
            .await
            .unwrap();

        assert!(!is_err(&result), "{}", text(&result));
        let stored = db.get_agent("watch-1").unwrap().unwrap();
        assert!(matches!(stored.trigger, Some(Trigger::Watch { .. })));
    }

    #[tokio::test]
    async fn task_watch_rejects_relative_path() {
        let home = tempdir().unwrap();
        let _home_guard = HomeVar::set(home.path());
        let (_dir, db, handler) = endpoint_test_handler();

        let result = handler
            .task_watch(Parameters(TaskWatchParams {
                id: "watch-bad-path".to_string(),
                path: "relative/path".to_string(),
                events: vec!["modify".to_string()],
                prompt: "react to changes".to_string(),
                cli: Some("opencode".to_string()),
                model: None,
                debounce_seconds: None,
                recursive: None,
                timeout_minutes: None,
            }))
            .await
            .unwrap();

        assert!(is_err(&result));
        assert!(text(&result).contains("must be absolute"));
        assert!(db.get_agent("watch-bad-path").unwrap().is_none());
    }

    #[tokio::test]
    async fn task_watch_rejects_unknown_event_name() {
        let home = tempdir().unwrap();
        let _home_guard = HomeVar::set(home.path());
        let (dir, _db, handler) = endpoint_test_handler();

        let result = handler
            .task_watch(Parameters(TaskWatchParams {
                id: "watch-bad-event".to_string(),
                path: dir.path().to_string_lossy().to_string(),
                events: vec!["explode".to_string()],
                prompt: "react to changes".to_string(),
                cli: Some("opencode".to_string()),
                model: None,
                debounce_seconds: None,
                recursive: None,
                timeout_minutes: None,
            }))
            .await
            .unwrap();

        assert!(is_err(&result));
    }

    // ── task_list / agent_list ───────────────────────────────────

    fn sample_agent(id: &str, trigger: Option<Trigger>) -> Agent {
        Agent {
            id: id.to_string(),
            prompt: "do things".to_string(),
            trigger,
            cli: Cli::new("opencode"),
            model: None,
            working_dir: None,
            enabled: true,
            enable_at: None,
            created_at: chrono::Utc::now(),
            log_path: format!("/tmp/{id}.log"),
            timeout_minutes: 15,
            expires_at: None,
            last_run_at: None,
            last_run_ok: None,
            last_triggered_at: None,
            trigger_count: 0,
        }
    }

    #[tokio::test]
    async fn task_list_reports_agents_and_corrupt_rows() {
        let (_dir, db, handler) = endpoint_test_handler();
        db.upsert_agent(&sample_agent("healthy-1", None)).unwrap();
        db.insert_corrupt_agent_for_test("corrupt-1", true).unwrap();

        let result = handler.task_list().await.unwrap();
        let out = text(&result);
        assert!(out.contains("healthy-1"));
        assert!(out.contains("corrupt"));
        assert!(out.contains("corrupt-1"));
    }

    #[tokio::test]
    async fn task_list_reports_empty_state() {
        let (_dir, _db, handler) = endpoint_test_handler();
        let result = handler.task_list().await.unwrap();
        assert!(text(&result).contains("No agents registered"));
    }

    // ── task_remove / agent_remove ───────────────────────────────

    #[tokio::test]
    async fn task_remove_deletes_existing_agent() {
        let (_dir, db, handler) = endpoint_test_handler();
        db.upsert_agent(&sample_agent("to-remove", None)).unwrap();

        let result = handler
            .task_remove(Parameters(IdParam {
                id: "to-remove".to_string(),
            }))
            .await
            .unwrap();

        assert!(!is_err(&result));
        assert!(text(&result).contains("removed"));
        assert!(db.get_agent("to-remove").unwrap().is_none());
    }

    #[tokio::test]
    async fn task_remove_reports_missing_agent() {
        let (_dir, _db, handler) = endpoint_test_handler();
        let result = handler
            .task_remove(Parameters(IdParam {
                id: "does-not-exist".to_string(),
            }))
            .await
            .unwrap();

        assert!(is_err(&result));
        assert!(text(&result).contains("No agent found"));
    }

    // ── task_enable / agent_enable ───────────────────────────────

    #[tokio::test]
    async fn task_enable_clears_expiry_on_expired_agent() {
        let (_dir, db, handler) = endpoint_test_handler();
        let mut agent = sample_agent("expired-1", None);
        agent.enabled = false;
        agent.expires_at = Some(chrono::Utc::now() - chrono::Duration::minutes(5));
        db.upsert_agent(&agent).unwrap();

        let result = handler
            .task_enable(Parameters(IdParam {
                id: "expired-1".to_string(),
            }))
            .await
            .unwrap();

        assert!(!is_err(&result));
        let stored = db.get_agent("expired-1").unwrap().unwrap();
        assert!(stored.enabled);
        assert!(stored.expires_at.is_none());
    }

    #[tokio::test]
    async fn task_enable_reports_missing_agent() {
        let (_dir, _db, handler) = endpoint_test_handler();
        let result = handler
            .task_enable(Parameters(IdParam {
                id: "ghost".to_string(),
            }))
            .await
            .unwrap();
        assert!(is_err(&result));
    }

    // ── task_schedule_enable / agent_schedule_enable ─────────────

    #[tokio::test]
    async fn task_schedule_enable_sets_future_enable_time() {
        let (_dir, db, handler) = endpoint_test_handler();
        let mut agent = sample_agent("sched-1", None);
        agent.enabled = false;
        db.upsert_agent(&agent).unwrap();

        let at = (chrono::Utc::now() + chrono::Duration::hours(1)).to_rfc3339();
        let result = handler
            .task_schedule_enable(Parameters(AgentScheduleEnableParams {
                id: "sched-1".to_string(),
                at: at.clone(),
            }))
            .await
            .unwrap();

        assert!(!is_err(&result));
        assert!(text(&result).contains("scheduled to enable"));
    }

    #[tokio::test]
    async fn task_schedule_enable_rejects_bad_timestamp() {
        let (_dir, db, handler) = endpoint_test_handler();
        db.upsert_agent(&sample_agent("sched-2", None)).unwrap();

        let result = handler
            .task_schedule_enable(Parameters(AgentScheduleEnableParams {
                id: "sched-2".to_string(),
                at: "not-a-timestamp".to_string(),
            }))
            .await
            .unwrap();

        assert!(is_err(&result));
        assert!(text(&result).contains("Invalid ISO 8601"));
    }

    #[tokio::test]
    async fn task_schedule_enable_reports_missing_agent() {
        let (_dir, _db, handler) = endpoint_test_handler();
        let at = (chrono::Utc::now() + chrono::Duration::hours(1)).to_rfc3339();
        let result = handler
            .task_schedule_enable(Parameters(AgentScheduleEnableParams {
                id: "ghost".to_string(),
                at,
            }))
            .await
            .unwrap();
        assert!(is_err(&result));
    }

    // ── task_disable / agent_disable ─────────────────────────────

    #[tokio::test]
    async fn task_disable_stops_a_watch_agent() {
        let (_dir, db, handler) = endpoint_test_handler();
        db.upsert_agent(&sample_agent(
            "watch-disable",
            Some(Trigger::Watch {
                path: "/tmp".to_string(),
                events: vec![crate::domain::models::WatchEvent::Modify],
                debounce_seconds: 2,
                recursive: false,
            }),
        ))
        .unwrap();

        let result = handler
            .task_disable(Parameters(IdParam {
                id: "watch-disable".to_string(),
            }))
            .await
            .unwrap();

        assert!(!is_err(&result));
        let stored = db.get_agent("watch-disable").unwrap().unwrap();
        assert!(!stored.enabled);
    }

    #[tokio::test]
    async fn task_disable_reports_missing_agent() {
        let (_dir, _db, handler) = endpoint_test_handler();
        let result = handler
            .task_disable(Parameters(IdParam {
                id: "ghost".to_string(),
            }))
            .await
            .unwrap();
        assert!(is_err(&result));
    }

    // ── agent_run ─────────────────────────────────────────────────

    #[tokio::test]
    async fn agent_run_launches_existing_agent_in_background() {
        let (_dir, db, handler) = endpoint_test_handler();
        db.upsert_agent(&sample_agent("runnable", None)).unwrap();

        let result = handler
            .agent_run(Parameters(IdParam {
                id: "runnable".to_string(),
            }))
            .await
            .unwrap();

        assert!(!is_err(&result));
        assert!(text(&result).contains("launched in background"));
    }

    #[tokio::test]
    async fn agent_run_reports_missing_agent() {
        let (_dir, _db, handler) = endpoint_test_handler();
        let result = handler
            .agent_run(Parameters(IdParam {
                id: "ghost".to_string(),
            }))
            .await
            .unwrap();
        assert!(is_err(&result));
        assert!(text(&result).contains("No agent found"));
    }

    // ── task_status / agent_status ───────────────────────────────

    #[tokio::test]
    async fn task_status_reports_agent_counts() {
        let (_dir, db, handler) = endpoint_test_handler();
        db.upsert_agent(&sample_agent(
            "cron-a",
            Some(Trigger::Cron {
                schedule_expr: "* * * * *".to_string(),
            }),
        ))
        .unwrap();
        db.upsert_agent(&sample_agent("manual-a", None)).unwrap();

        let result = handler.task_status().await.unwrap();
        let out = text(&result);
        assert!(out.contains("canopy v"));
        assert!(out.contains("cron: 1"));
        assert!(out.contains("manual: 1"));
    }

    // ── task_models / agent_models ───────────────────────────────

    #[tokio::test]
    async fn task_models_rejects_unconfigured_platform() {
        let home = tempdir().unwrap();
        let canopy_dir = home.path().join(".canopy");
        std::fs::create_dir_all(&canopy_dir).unwrap();
        let config = crate::domain::canopy_config::CanopyConfig {
            clis: vec![crate::domain::cli_config::CliConfig {
                name: "echo-cli".to_string(),
                binary: "/bin/echo".to_string(),
                ..Default::default()
            }],
            ..Default::default()
        };
        config.save(&canopy_dir).unwrap();
        let _home_guard = HomeVar::set(home.path());

        let (_dir, _db, handler) = endpoint_test_handler();
        let result = handler
            .task_models(Parameters(TaskModelsParams {
                platform: Some("not-configured-platform".to_string()),
                refresh: None,
            }))
            .await
            .unwrap();

        assert!(is_err(&result));
        assert!(text(&result).contains("is not configured in canopy"));
    }

    #[tokio::test]
    async fn task_models_enumerates_native_platform_models() {
        use std::os::unix::fs::PermissionsExt;

        let home = tempdir().unwrap();
        let canopy_dir = home.path().join(".canopy");
        std::fs::create_dir_all(&canopy_dir).unwrap();
        let script = home.path().join("list-models.sh");
        std::fs::write(&script, "#!/bin/sh\necho model-a\necho model-b\n").unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();

        let config = crate::domain::canopy_config::CanopyConfig {
            clis: vec![crate::domain::cli_config::CliConfig {
                name: "native-cli".to_string(),
                binary: script.to_string_lossy().to_string(),
                models_list_cmd: Some("--list".to_string()),
                ..Default::default()
            }],
            ..Default::default()
        };
        config.save(&canopy_dir).unwrap();
        let _home_guard = HomeVar::set(home.path());

        let (_dir, _db, handler) = endpoint_test_handler();
        let result = handler
            .task_models(Parameters(TaskModelsParams {
                platform: Some("native-cli".to_string()),
                refresh: Some(true),
            }))
            .await
            .unwrap();

        assert!(!is_err(&result), "{}", text(&result));
        let out = text(&result);
        assert!(out.contains("model-a"));
        assert!(out.contains("model-b"));
        assert!(out.contains("Source:"));
    }

    // ── task_logs / agent_logs ───────────────────────────────────

    #[tokio::test]
    async fn task_logs_reports_no_logs_for_unexecuted_agent() {
        let (dir, db, handler) = endpoint_test_handler();
        let mut agent = sample_agent("no-logs-yet", None);
        agent.log_path = dir
            .path()
            .join("no-logs-yet.log")
            .to_string_lossy()
            .to_string();
        db.upsert_agent(&agent).unwrap();

        let result = handler
            .task_logs(Parameters(TaskLogsParams {
                id: "no-logs-yet".to_string(),
                lines: None,
                since: None,
            }))
            .await
            .unwrap();

        assert!(!is_err(&result));
        assert!(text(&result).contains("No logs found"));
    }

    #[tokio::test]
    async fn task_logs_returns_recent_lines() {
        let (dir, db, handler) = endpoint_test_handler();
        let log_path = dir.path().join("has-logs.log");
        std::fs::write(&log_path, "line one\nline two\nline three\n").unwrap();
        let mut agent = sample_agent("has-logs", None);
        agent.log_path = log_path.to_string_lossy().to_string();
        db.upsert_agent(&agent).unwrap();

        let result = handler
            .task_logs(Parameters(TaskLogsParams {
                id: "has-logs".to_string(),
                lines: Some(2),
                since: None,
            }))
            .await
            .unwrap();

        let out = text(&result);
        assert!(out.contains("line two"));
        assert!(out.contains("line three"));
        assert!(!out.contains("line one"));
    }

    // ── task_update / agent_update ───────────────────────────────

    #[tokio::test]
    async fn task_update_renames_and_updates_prompt() {
        let home = tempdir().unwrap();
        let _home_guard = HomeVar::set(home.path());
        let (_dir, db, handler) = endpoint_test_handler();
        db.upsert_agent(&sample_agent(
            "old-id",
            Some(Trigger::Cron {
                schedule_expr: "* * * * *".to_string(),
            }),
        ))
        .unwrap();

        let result = handler
            .task_update(Parameters(TaskUpdateParams {
                id: "old-id".to_string(),
                new_id: Some("new-id".to_string()),
                prompt: Some("updated prompt".to_string()),
                cli: None,
                model: None,
                schedule: None,
                working_dir: None,
                duration_minutes: None,
                path: None,
                events: None,
                debounce_seconds: None,
                recursive: None,
                enabled: None,
                notify_on_success: None,
            }))
            .await
            .unwrap();

        assert!(!is_err(&result), "{}", text(&result));
        assert!(db.get_agent("old-id").unwrap().is_none());
        let renamed = db.get_agent("new-id").unwrap().unwrap();
        assert_eq!(renamed.prompt, "updated prompt");
    }

    #[tokio::test]
    async fn task_update_reports_missing_agent() {
        let home = tempdir().unwrap();
        let _home_guard = HomeVar::set(home.path());
        let (_dir, _db, handler) = endpoint_test_handler();

        let result = handler
            .task_update(Parameters(TaskUpdateParams {
                id: "ghost".to_string(),
                new_id: None,
                prompt: None,
                cli: None,
                model: None,
                schedule: None,
                working_dir: None,
                duration_minutes: None,
                path: None,
                events: None,
                debounce_seconds: None,
                recursive: None,
                enabled: None,
                notify_on_success: None,
            }))
            .await
            .unwrap();

        assert!(is_err(&result));
        assert!(text(&result).contains("No agent found"));
    }

    #[tokio::test]
    async fn task_update_rejects_invalid_new_id() {
        let home = tempdir().unwrap();
        let _home_guard = HomeVar::set(home.path());
        let (_dir, db, handler) = endpoint_test_handler();
        db.upsert_agent(&sample_agent("keep-id", None)).unwrap();

        let result = handler
            .task_update(Parameters(TaskUpdateParams {
                id: "keep-id".to_string(),
                new_id: Some("bad id!".to_string()),
                prompt: None,
                cli: None,
                model: None,
                schedule: None,
                working_dir: None,
                duration_minutes: None,
                path: None,
                events: None,
                debounce_seconds: None,
                recursive: None,
                enabled: None,
                notify_on_success: None,
            }))
            .await
            .unwrap();

        assert!(is_err(&result));
        assert!(db.get_agent("keep-id").unwrap().is_some());
    }

    // ── task_report / agent_report ───────────────────────────────

    fn sample_run(id: &str, agent_id: &str, status: RunStatus) -> RunLog {
        RunLog {
            id: id.to_string(),
            background_agent_id: agent_id.to_string(),
            status,
            trigger_type: TriggerType::Manual,
            summary: None,
            started_at: chrono::Utc::now(),
            finished_at: None,
            exit_code: None,
            timeout_at: None,
        }
    }

    #[tokio::test]
    async fn task_report_transitions_in_progress_to_success() {
        let (_dir, db, handler) = endpoint_test_handler();
        db.upsert_agent(&sample_agent("reporter", None)).unwrap();
        db.insert_run(&sample_run("run-1", "reporter", RunStatus::InProgress))
            .unwrap();

        let result = handler
            .task_report(Parameters(TaskReportParams {
                run_id: "run-1".to_string(),
                status: "success".to_string(),
                summary: Some("all good".to_string()),
            }))
            .await
            .unwrap();

        assert!(!is_err(&result), "{}", text(&result));
        let run = db.get_run("run-1").unwrap().unwrap();
        assert!(matches!(run.status, RunStatus::Success));
        let agent = db.get_agent("reporter").unwrap().unwrap();
        assert_eq!(agent.last_run_ok, Some(true));
    }

    #[tokio::test]
    async fn task_report_rejects_invalid_status() {
        let (_dir, _db, handler) = endpoint_test_handler();
        let result = handler
            .task_report(Parameters(TaskReportParams {
                run_id: "run-x".to_string(),
                status: "sideways".to_string(),
                summary: None,
            }))
            .await
            .unwrap();
        assert!(is_err(&result));
        assert!(text(&result).contains("Invalid status"));
    }

    #[tokio::test]
    async fn task_report_requires_summary_on_success() {
        let (_dir, db, handler) = endpoint_test_handler();
        db.upsert_agent(&sample_agent("reporter-2", None)).unwrap();
        db.insert_run(&sample_run("run-2", "reporter-2", RunStatus::InProgress))
            .unwrap();

        let result = handler
            .task_report(Parameters(TaskReportParams {
                run_id: "run-2".to_string(),
                status: "success".to_string(),
                summary: None,
            }))
            .await
            .unwrap();
        assert!(is_err(&result));
        assert!(text(&result).contains("summary is required"));
    }

    #[tokio::test]
    async fn task_report_rejects_invalid_transition() {
        let (_dir, db, handler) = endpoint_test_handler();
        db.upsert_agent(&sample_agent("reporter-3", None)).unwrap();
        // Already terminal (Success) — Success -> Success is not a valid
        // transition per validate_run_transition.
        db.insert_run(&sample_run("run-3", "reporter-3", RunStatus::Success))
            .unwrap();

        let result = handler
            .task_report(Parameters(TaskReportParams {
                run_id: "run-3".to_string(),
                status: "success".to_string(),
                summary: Some("again".to_string()),
            }))
            .await
            .unwrap();
        assert!(is_err(&result));
        assert!(text(&result).contains("Invalid transition"));
    }

    #[tokio::test]
    async fn task_report_errors_on_unknown_run_id() {
        let (_dir, _db, handler) = endpoint_test_handler();
        let result = handler
            .task_report(Parameters(TaskReportParams {
                run_id: "no-such-run".to_string(),
                status: "success".to_string(),
                summary: Some("done".to_string()),
            }))
            .await;
        assert!(result.is_err());
    }
}
