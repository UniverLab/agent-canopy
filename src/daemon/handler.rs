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
use crate::domain::blueprints::{merge_blueprint_config, validate_blueprint_deletable, Blueprint};
use crate::domain::loops::{
    validate_spec_description_template, Loop, LoopDetails, LoopEdge, LoopEdgeCondition, LoopNode,
    LoopNodeKind, LoopRunStatus, LoopSpec, LoopSpecStatus, LoopStatus,
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
        .ok_or_else(|| format!("Pool '{pool_id}' not found."))
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
                "Pool '{pool_id}' spec '{spec_id}' is already running under loop '{owner}'; wait for it to finish, or pause that loop, before starting a new run against this pool."
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
            "Reorder must list all {} pool spec(s) exactly once; got {}.",
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
            return Err(format!("Pool has no spec '{id}'."));
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
                "Spec '{spec_id}' is currently running and cannot be removed from pool '{pool_id}'; wait for it to finish, or pause the loop, first."
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

/// Core logic for `loop_reset`, factored out of the tool method so it only
/// needs `&Database` (no engine/executor) and can be unit-tested directly.
fn perform_loop_reset(
    db: &Database,
    loop_id: &str,
    specs: Option<&[String]>,
) -> Result<CallToolResult, McpError> {
    let Some(lp) = db.get_loop(loop_id).map_err(internal_error)? else {
        return Ok(error_result(&format!("Loop '{loop_id}' not found.")));
    };

    if lp.status == LoopStatus::Running {
        return Ok(error_result(&format!(
            "Loop '{loop_id}' is running; call loop_pause first, then loop_reset."
        )));
    }

    let loop_specs = db.list_loop_specs(loop_id).map_err(internal_error)?;
    let valid_ids: std::collections::HashSet<&str> =
        loop_specs.iter().map(|spec| spec.id.as_str()).collect();

    let target_ids: Vec<String> = match specs {
        Some(ids) => {
            for id in ids {
                if !valid_ids.contains(id.as_str()) {
                    return Ok(error_result(&format!(
                        "Spec '{id}' does not belong to loop '{loop_id}'."
                    )));
                }
            }
            ids.to_vec()
        }
        None => loop_specs
            .iter()
            .filter(|spec| spec.status != LoopSpecStatus::Completed)
            .map(|spec| spec.id.clone())
            .collect(),
    };

    for spec_id in &target_ids {
        db.reset_loop_spec_status(spec_id).map_err(internal_error)?;
    }
    db.reset_loop_status(loop_id).map_err(internal_error)?;

    Ok(success_result(&format!(
        "Loop '{loop_id}' reset to pending; {} spec(s) reset.",
        target_ids.len()
    )))
}

fn build_spec_update_response(spec_id: &str) -> CallToolResult {
    success_result(&format!("Loop spec '{spec_id}' updated."))
}

/// Summary JSON for `spec_list` — no run/blocker info, since a standalone
/// (unassigned) spec has never run. Compare [`loop_spec_details_json`],
/// which adds that runtime detail for specs already inside a loop.
fn spec_summary_json(spec: &LoopSpec) -> serde_json::Value {
    serde_json::json!({
        "id": spec.id,
        "loop_id": spec.loop_id,
        "name": spec.name,
        "description": spec.description,
        "workdir": spec.workdir,
        "position": spec.position,
        "parallelizable": spec.parallelizable,
        "status": spec.status.as_str(),
    })
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
        loop_engine: Arc<LoopEngine>,
        notification_service: Arc<dyn NotificationService>,
        sync_manager: Arc<SyncManager>,
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

        if let Err(e) = validate_at_least_one_bool(
            &[
                name.is_some(),
                description.is_some(),
                workdir.is_some(),
                new_trigger.is_some(),
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
        name = "pool_create",
        description = "Create a pool: an ordered queue of existing specs, decoupled from any one loop."
    )]
    async fn pool_create(
        &self,
        Parameters(params): Parameters<PoolCreateParams>,
    ) -> Result<CallToolResult, McpError> {
        let name = params.name.trim();
        if let Err(e) = validate_non_empty(name, "Pool name") {
            return Ok(error_result(&e));
        }

        let pool = Pool {
            id: uuid::Uuid::new_v4().to_string(),
            name: name.to_string(),
            created_at: chrono::Utc::now(),
        };
        self.db.insert_pool(&pool).map_err(internal_error)?;

        Ok(build_id_result(&pool.id, "pool_id"))
    }

    #[tool(
        name = "pool_add_spec",
        description = "Append an existing spec to the end of a pool's queue."
    )]
    async fn pool_add_spec(
        &self,
        Parameters(params): Parameters<PoolAddSpecParams>,
    ) -> Result<CallToolResult, McpError> {
        let pool_id = params.pool_id.trim();
        if let Err(e) = validate_pool_exists(&self.db, pool_id) {
            return Ok(error_result(&e));
        }
        let spec_id = params.spec_id.trim();
        if let Err(e) = validate_spec_exists(&self.db, spec_id) {
            return Ok(error_result(&e));
        }
        let already_member = self
            .db
            .pool_has_member(pool_id, spec_id)
            .map_err(internal_error)?;
        if already_member {
            return Ok(error_result(&format!(
                "Spec '{spec_id}' is already in pool '{pool_id}'."
            )));
        }

        self.db
            .append_pool_member(pool_id, spec_id)
            .map_err(internal_error)?;

        Ok(success_result(&format!(
            "Spec '{spec_id}' added to pool '{pool_id}'."
        )))
    }

    #[tool(
        name = "pool_list",
        description = "List a pool's ordered members, or every pool (summary only) if pool_id is omitted."
    )]
    async fn pool_list(
        &self,
        Parameters(params): Parameters<PoolListParams>,
    ) -> Result<CallToolResult, McpError> {
        let pool_id = params
            .pool_id
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty());

        let body = match pool_id {
            Some(pool_id) => {
                let details = match self.db.get_pool_details(pool_id) {
                    Ok(Some(details)) => details,
                    Ok(None) => return Ok(error_result(&format!("Pool '{pool_id}' not found."))),
                    Err(e) => return Err(internal_error(e.to_string())),
                };
                serde_json::json!({ "pool": pool_details_json(&details) })
            }
            None => {
                let pools = self.db.list_pools().map_err(internal_error)?;
                serde_json::json!({
                    "pools": pools
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

    #[tool(
        name = "pool_remove_spec",
        description = "Remove a spec from a pool's queue."
    )]
    async fn pool_remove_spec(
        &self,
        Parameters(params): Parameters<PoolRemoveSpecParams>,
    ) -> Result<CallToolResult, McpError> {
        let pool_id = params.pool_id.trim();
        if let Err(e) = validate_pool_exists(&self.db, pool_id) {
            return Ok(error_result(&e));
        }
        let spec_id = params.spec_id.trim();
        if let Err(e) = validate_pool_member_removable(&self.db, pool_id, spec_id) {
            return Ok(error_result(&e));
        }
        let removed = self
            .db
            .remove_pool_member(pool_id, spec_id)
            .map_err(internal_error)?;
        if !removed {
            return Ok(error_result(&format!(
                "Pool '{pool_id}' has no spec '{spec_id}'."
            )));
        }

        Ok(success_result(&format!(
            "Spec '{spec_id}' removed from pool '{pool_id}'."
        )))
    }

    #[tool(
        name = "pool_reorder",
        description = "Reorder a pool's queue. `spec_ids` must list every pool member exactly once, in the desired order — a total replacement, not a partial swap."
    )]
    async fn pool_reorder(
        &self,
        Parameters(params): Parameters<PoolReorderParams>,
    ) -> Result<CallToolResult, McpError> {
        let pool_id = params.pool_id.trim();
        if let Err(e) = validate_pool_exists(&self.db, pool_id) {
            return Ok(error_result(&e));
        }
        let current = self
            .db
            .list_pool_member_spec_ids(pool_id)
            .map_err(internal_error)?;
        if let Err(e) = validate_pool_reorder(&current, &params.spec_ids) {
            return Ok(error_result(&e));
        }
        if let Err(e) = validate_pool_reorder_locking(&self.db, &current, &params.spec_ids) {
            return Ok(error_result(&e));
        }

        self.db
            .reorder_pool_members(pool_id, &params.spec_ids)
            .map_err(internal_error)?;

        Ok(success_result(&format!("Pool '{pool_id}' reordered.")))
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
        description = "Run a loop in the background, spec by spec. With `pool_id`, runs the pool's pending specs (in queue order) through the loop's graph instead of the loop's own bound specs. `workdir` overrides the loop's workdir for this run only."
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

        if lp.status == LoopStatus::Running {
            return Ok(error_result(&format!(
                "Loop '{}' is already running.",
                params.loop_id
            )));
        }
        if matches!(lp.status, LoopStatus::Completed | LoopStatus::Failed) {
            return Ok(error_result(
                "Completed or failed loops cannot be resumed yet.",
            ));
        }

        let pool_id = params
            .pool_id
            .as_deref()
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
    #[tool(
        name = "loop_reset",
        description = "Reset a completed/failed loop back to pending so loop_run can relaunch it. Without `specs`, resets every non-completed spec, leaving already-completed ones untouched so loop_run resumes at the first pending spec. With `specs`, resets exactly those spec IDs, even if they were completed. Rejects a `running` loop — call loop_pause first."
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
    #[tool(
        name = "loop_schedule_autorun",
        description = "Schedule a one-shot resume for a loop at a future ISO 8601 time — the scheduler launches it once that time is reached and clears the schedule. Useful for a loop that failed on a quota to reschedule its own resumption at the exact reset time."
    )]
    async fn loop_schedule_autorun(
        &self,
        Parameters(LoopScheduleAutorunParams { loop_id, at }): Parameters<
            LoopScheduleAutorunParams,
        >,
    ) -> Result<CallToolResult, McpError> {
        let Some(_existing) = self
            .db
            .get_loop(&loop_id)
            .map_err(|e| McpError::internal_error(e.to_string(), None))?
        else {
            return Ok(error_result(&format!(
                "No loop found with ID '{}'",
                loop_id
            )));
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
            .schedule_loop_autorun(&loop_id, at)
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;

        self.scheduler_notify.notify_one();

        Ok(success_result(&format!(
            "Loop '{}' scheduled to autorun at {}",
            loop_id,
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
            "retry_current_node" => {}
            "skip_next_spec" => self.handle_skip_next_spec(&params.loop_id)?,
            _ => {
                return Ok(error_result(
                    "loop_continue action must be retry_current_node or skip_next_spec.",
                ));
            }
        }

        self.db
            .update_loop_status(&params.loop_id, LoopStatus::Running, None, None)
            .map_err(internal_error)?;
        Arc::clone(&self.loop_engine).start_background(params.loop_id.clone());

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
        let run = match self.db.get_active_loop_run_for_node(&params.node_id) {
            Ok(Some(r)) => r,
            Ok(None) => {
                return Ok(error_result(&format!(
                    "No active loop run found for node '{}'.",
                    params.node_id
                )))
            }
            Err(e) => return Err(internal_error(e.to_string())),
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
        let run = match self.db.get_active_loop_run_for_node(&params.node_id) {
            Ok(Some(r)) => r,
            Ok(None) => {
                return Ok(error_result(&format!(
                    "No active loop run found for node '{}'.",
                    params.node_id
                )))
            }
            Err(e) => return Err(internal_error(e.to_string())),
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
        self.notification_service
            .notify_task_failed(&run.loop_id, 1, &params.description);

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

impl TaskTriggerHandler {
    fn handle_skip_next_spec(&self, loop_id: &str) -> Result<(), McpError> {
        let current_spec = self
            .db
            .list_loop_specs(loop_id)
            .map_err(internal_error)?
            .into_iter()
            .find(|spec| spec.status == LoopSpecStatus::Running)
            .ok_or_else(|| {
                McpError::invalid_params(
                    "No running spec found to skip from this paused loop.",
                    None,
                )
            })?;

        self.db
            .update_loop_spec_status(
                &current_spec.id,
                LoopSpecStatus::Skipped,
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
        "graph": {
            "nodes": lp.graph_nodes.iter().map(loop_node_json).collect::<Vec<_>>(),
            "edges": lp.graph_edges.iter().map(loop_edge_json).collect::<Vec<_>>(),
        },
        "specs": specs,
    }))
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
        "started_at": spec.spec.started_at.map(|value| value.to_rfc3339()),
        "completed_at": spec.spec.completed_at.map(|value| value.to_rfc3339()),
        "nodes": spec.nodes.iter().map(loop_node_json).collect::<Vec<_>>(),
        "edges": spec.edges.iter().map(loop_edge_json).collect::<Vec<_>>(),
        "runs": runs,
    }))
}

fn loop_node_json(node: &LoopNode) -> serde_json::Value {
    serde_json::json!({
        "id": node.id,
        "spec_id": node.spec_id,
        "loop_id": node.loop_id,
        "name": node.name,
        "kind": node.kind.as_str(),
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
        header_str, loop_details_json, missing_sync_identity_error, perform_loop_reset,
        resolve_graph_target, resolve_node_kind_and_config, validate_blueprint_exists,
        validate_node_config, validate_pool_exists, validate_pool_member_removable,
        validate_pool_not_consumed, validate_pool_reorder, validate_pool_reorder_locking,
        validate_spec_deletable, validate_spec_exists, MISSING_SYNC_IDENTITY_MESSAGE,
    };
    use crate::db::Database;
    use crate::domain::blueprints::Blueprint;
    use crate::domain::loops::{
        Loop, LoopNode, LoopNodeKind, LoopNodeRun, LoopRunStatus, LoopSpec, LoopSpecStatus,
        LoopStatus,
    };
    use crate::domain::pools::Pool;
    use crate::shared::sync_identity::CANOPY_AGENT_ID_HEADER;
    use tempfile::tempdir;

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

    #[test]
    fn pool_crud_and_ordering_round_trips() {
        let (_dir, db) = pool_test_db();
        for id in ["spec-a", "spec-b", "spec-c"] {
            db.insert_loop_spec(&standalone_spec(id)).unwrap();
        }
        insert_pool(&db, "pool-1");

        db.append_pool_member("pool-1", "spec-a").unwrap();
        db.append_pool_member("pool-1", "spec-b").unwrap();
        db.append_pool_member("pool-1", "spec-c").unwrap();

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
            db.append_pool_member("pool-1", id).unwrap();
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
        }
    }

    #[test]
    fn pool_not_consumed_allows_start_when_every_member_is_pending() {
        let (_dir, db) = pool_test_db();
        db.insert_loop_spec(&standalone_spec("spec-a")).unwrap();
        db.insert_loop_spec(&standalone_spec("spec-b")).unwrap();
        insert_pool(&db, "pool-1");
        db.append_pool_member("pool-1", "spec-a").unwrap();
        db.append_pool_member("pool-1", "spec-b").unwrap();

        assert!(validate_pool_not_consumed(&db, "pool-1", "loop-requesting").is_ok());
    }

    #[test]
    fn pool_not_consumed_blocks_when_a_member_runs_under_another_loop() {
        let (_dir, db) = pool_test_db();
        db.insert_loop_spec(&running_spec("spec-a")).unwrap();
        insert_pool(&db, "pool-1");
        db.append_pool_member("pool-1", "spec-a").unwrap();
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
        db.append_pool_member("pool-1", "spec-a").unwrap();
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
            db.append_pool_member("pool-1", id).unwrap();
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
            db.append_pool_member("pool-1", id).unwrap();
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
            db.append_pool_member("pool-1", id).unwrap();
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
        db.append_pool_member("pool-1", "spec-a").unwrap();

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
}
