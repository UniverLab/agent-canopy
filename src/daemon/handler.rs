//! MCP Server handler implementing all canopy tools.
//!
//! Uses the `rmcp` SDK's `#[tool_router]` and `#[tool_handler]` macros
//! with `Parameters<T>` for proper MCP protocol compliance.

use std::sync::Arc;

use axum::http::request::Parts;
use rmcp::handler::server::common::AsRequestContext;
use rmcp::handler::server::router::tool::ToolRouter;
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
use crate::daemon::params_extract::Parameters;
use crate::db::intelligence::IntelligenceNodeRecord;
use crate::db::Database;
use crate::domain::blueprints::{merge_blueprint_config, validate_blueprint_deletable, Blueprint};
use crate::domain::loops::{
    validate_router_edges_declared, validate_router_route_coverage, validate_router_routes,
    validate_spec_description_template, ArchiveLoopOutcome, Ensemble, EnsembleMember,
    EnsembleMemberSpec, Loop, LoopDetails, LoopEdge, LoopEdgeCondition, LoopNode, LoopNodeKind,
    LoopNodeRun, LoopResetOutcome, LoopRunStatus, LoopSpec, LoopSpecStatus, LoopStatus,
    RouterRoute, SpecAdminStatusOutcome,
};
use crate::domain::models::{Agent, Trigger};
use crate::domain::queues::{Queue, QueueDetails};
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

fn resolve_prefix_or_error(
    db: &Database,
    raw: &str,
    resolver: fn(&Database, &str) -> anyhow::Result<Option<String>>,
    kind_name: &str,
) -> Result<String, CallToolResult> {
    match resolver(db, raw) {
        Ok(Some(full_id)) => Ok(full_id),
        Ok(None) => Err(error_result(&format!("{kind_name} '{raw}' not found."))),
        Err(e) => Err(error_result(&e.to_string())),
    }
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
/// `(platform, model, prompt_override)` triples in the caller's order — the
/// order consolidation and resize diffs rely on.
fn validate_ensemble_members(
    members: &[EnsembleMemberParams],
) -> Result<Vec<EnsembleMemberSpec>, String> {
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
            let prompt_override = member
                .prompt_override
                .as_deref()
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(str::to_string);
            Ok((platform.to_string(), model, prompt_override))
        })
        .collect()
}

/// This member's effective prompt: its own `prompt_override` if it has one,
/// else the ensemble's shared `prompt_template`.
fn effective_member_prompt<'a>(prompt_override: Option<&'a str>, shared: &'a str) -> &'a str {
    prompt_override.unwrap_or(shared)
}

/// Build a member agent node's `config` — its effective prompt (its own
/// override, or the shared ensemble prompt) plus this member's own
/// platform/model, the same shape `validate_node_config`'s `Agent` arm
/// expects.
fn member_node_config(
    platform: &str,
    model: Option<&str>,
    effective_prompt: &str,
    timeout_minutes: i64,
) -> serde_json::Value {
    serde_json::json!({
        "platform": platform,
        "model": model,
        "prompt_template": effective_prompt,
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
    members: &'a [EnsembleMemberSpec],
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

    for (index, (platform, model, prompt_override)) in spec.members.iter().enumerate() {
        let node_id = uuid::Uuid::new_v4().to_string();
        let effective_prompt =
            effective_member_prompt(prompt_override.as_deref(), spec.prompt_template);
        member_nodes.push(LoopNode {
            id: node_id.clone(),
            spec_id: spec.spec_id.clone(),
            loop_id: spec.loop_id.clone(),
            name: format!("{} [{}]", spec.name, index + 1),
            kind: LoopNodeKind::Agent,
            config: member_node_config(
                platform,
                model.as_deref(),
                effective_prompt,
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
            condition: spec.entry_condition.clone(),
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
            prompt_override: prompt_override.clone(),
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
        entry_condition: spec.entry_condition.clone(),
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
            let resolved = db
                .resolve_spec_id_by_prefix(spec_id)
                .map_err(|e| e.to_string())?;
            let full_id = resolved.as_deref().unwrap_or(spec_id);
            validate_spec_exists(db, full_id)?;
            Ok(GraphTarget::Spec(full_id.to_string()))
        }
        (None, Some(loop_id)) => {
            let resolved = db
                .resolve_loop_id_by_prefix(loop_id)
                .map_err(|e| e.to_string())?;
            let full_id = resolved.as_deref().unwrap_or(loop_id);
            validate_loop_exists(db, full_id)?;
            Ok(GraphTarget::Loop(full_id.to_string()))
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
/// Note: a spec that's a member of a [`Queue`] can still be deleted — the
/// `queue_members` row cascades away with it (see `queues` table). Queues are
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

/// [`validate_edge_condition`] plus `route` support for
/// `loop_add_edge`/`loop_update_edge`: a `"route"` condition requires a
/// non-empty `route` param naming the label. `pass`/`fail`/`always` are
/// unaffected — delegated straight to [`validate_edge_condition`].
fn validate_edge_condition_with_route(
    condition: &str,
    route: Option<&str>,
) -> Result<LoopEdgeCondition, String> {
    if condition.trim() == "route" {
        let label = route
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| {
                "Loop edge condition 'route' requires a non-empty 'route' label.".to_string()
            })?;
        return Ok(LoopEdgeCondition::Route(label.to_string()));
    }
    validate_edge_condition(condition)
        .map_err(|_| "Loop edge condition must be one of: pass, fail, always, route.".to_string())
}

/// If `condition` is a `Route`, validate that its label names a route
/// actually declared by the router node `from_node_id` — never accepted
/// free-form. Every other condition is a no-op.
fn validate_route_edge_target(
    db: &Database,
    from_node_id: &str,
    condition: &LoopEdgeCondition,
) -> Result<(), String> {
    let Some(label) = condition.route_label() else {
        return Ok(());
    };
    let from_node = validate_node_exists(db, from_node_id)?;
    if from_node.kind != LoopNodeKind::Router {
        return Err(format!(
            "Edge condition 'route' requires from_node '{from_node_id}' to be a router node, not '{}'.",
            from_node.kind.display_str()
        ));
    }
    let (routes, _fallback) = parse_router_routes(&from_node.config)?;
    if !routes.iter().any(|route| route.label == label) {
        return Err(format!(
            "Edge names undeclared route '{label}'. Router '{from_node_id}' declares: {}.",
            routes
                .iter()
                .map(|route| route.label.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    Ok(())
}

fn validate_node_kind(kind: &str) -> Result<LoopNodeKind, String> {
    LoopNodeKind::from_str(kind.trim())
        .ok_or_else(|| "Loop node kind must be one of: agent, check, gate, router.".to_string())
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
/// it belongs to an ensemble (member or quorum) — a member's prompt/platform/
/// model (including its optional `prompt_override`) and the quorum's own
/// config are all owned by the ensemble unit, so every edit goes through
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

/// Resolve the [`LoopStatus`] (and id) of the loop that owns a spec-scoped or
/// loop-scoped graph object. A node/edge always has exactly one of
/// `spec_id`/`loop_id` set. `None` means the object belongs to a standalone
/// spec not yet bound to any loop — nothing running to guard against.
fn resolve_owning_loop_status(
    db: &Database,
    spec_id: Option<&str>,
    loop_id: Option<&str>,
) -> Result<Option<(String, LoopStatus)>, String> {
    if let Some(loop_id) = loop_id {
        let lp = db
            .get_loop(loop_id)
            .map_err(|e| e.to_string())?
            .ok_or_else(|| format!("Loop '{loop_id}' not found."))?;
        return Ok(Some((lp.id, lp.status)));
    }
    if let Some(spec_id) = spec_id {
        let spec = validate_spec_exists(db, spec_id)?;
        if let Some(loop_id) = &spec.loop_id {
            let lp = db
                .get_loop(loop_id)
                .map_err(|e| e.to_string())?
                .ok_or_else(|| format!("Loop '{loop_id}' not found."))?;
            return Ok(Some((lp.id, lp.status)));
        }
    }
    Ok(None)
}

/// Reject a topology mutation (retargeting/deleting an edge, deleting a
/// node) while the owning loop is `running`. Deliberately stricter than node
/// CONFIG edits, which are safe because the graph is snapshotted per spec at
/// `run_spec` — a config edit lands on the next spec. Topology is different:
/// an edge retargeted or a node deleted mid-dispatch can send a live run to
/// a node the engine never selected.
fn validate_topology_mutation_allowed(
    db: &Database,
    spec_id: Option<&str>,
    loop_id: Option<&str>,
) -> Result<(), String> {
    if let Some((loop_id, LoopStatus::Running)) = resolve_owning_loop_status(db, spec_id, loop_id)?
    {
        return Err(format!(
            "Loop '{loop_id}' is running; call loop_pause first, then retry this topology change."
        ));
    }
    Ok(())
}

/// `to_node` must exist in the same spec/loop graph as `edge` — a
/// cross-graph retarget would leave the edge's `spec_id`/`loop_id` naming one
/// graph while `to_node` lives in another.
fn validate_edge_retarget_destination(
    db: &Database,
    edge: &LoopEdge,
    to_node: &str,
) -> Result<(), String> {
    let nodes = if let Some(spec_id) = &edge.spec_id {
        db.list_loop_nodes(spec_id).map_err(|e| e.to_string())?
    } else if let Some(loop_id) = &edge.loop_id {
        db.list_loop_nodes_for_loop(loop_id)
            .map_err(|e| e.to_string())?
    } else {
        Vec::new()
    };
    if !nodes.iter().any(|n| n.id == to_node) {
        return Err(format!(
            "Node '{to_node}' does not belong to the same graph as edge '{}'; retargeting across specs/loops is not allowed.",
            edge.id
        ));
    }
    Ok(())
}

/// Whether `node` is the computed entry point of its graph (spec-scoped or
/// loop-scoped) — see [`crate::loop_engine::find_entry_node`]. A graph with
/// no *confirmed* single entry (e.g. `find_entry_node` errors on multiple
/// sources) never blocks deletion here; only a confirmed single entry does.
fn validate_node_not_entry_point(db: &Database, node: &LoopNode) -> Result<(), String> {
    let (nodes, edges) = if let Some(spec_id) = &node.spec_id {
        (
            db.list_loop_nodes(spec_id).map_err(|e| e.to_string())?,
            db.list_loop_edges(spec_id).map_err(|e| e.to_string())?,
        )
    } else if let Some(loop_id) = &node.loop_id {
        (
            db.list_loop_nodes_for_loop(loop_id)
                .map_err(|e| e.to_string())?,
            db.list_loop_edges_for_loop(loop_id)
                .map_err(|e| e.to_string())?,
        )
    } else {
        return Ok(());
    };
    if let Ok(entry_id) = crate::loop_engine::find_entry_node(&nodes, &edges, "graph") {
        if entry_id == node.id {
            return Err(format!(
                "Node '{}' is the entry point of its graph; deleting it would leave the graph \
                 unable to run. Wire a different node as the entry first.",
                node.id
            ));
        }
    }
    Ok(())
}

/// Validated retarget of an existing edge's `to_node` — the shared path used
/// by both the `loop_update_edge` MCP tool and the TUI's edge editor, so an
/// ordinary `pass`/`fail` edge gets the same checks a router's route edges
/// have always had.
pub(crate) fn retarget_loop_edge(
    db: &Database,
    edge_id: &str,
    to_node: &str,
) -> Result<LoopEdge, String> {
    let edge = validate_edge_exists(db, edge_id)?;
    validate_topology_mutation_allowed(db, edge.spec_id.as_deref(), edge.loop_id.as_deref())?;
    validate_node_not_ensemble_owned(db, &edge.from_node)?;
    validate_node_not_ensemble_owned(db, to_node)?;
    validate_edge_retarget_destination(db, &edge, to_node)?;
    db.update_loop_edge_target(&edge.id, to_node)
        .map_err(|e| e.to_string())?;
    Ok(LoopEdge {
        to_node: to_node.to_string(),
        ..edge
    })
}

/// Validated deletion of a single edge — the shared path used by both the
/// `loop_delete_edge` MCP tool and the TUI's edge editor.
pub(crate) fn delete_loop_edge_checked(db: &Database, edge_id: &str) -> Result<LoopEdge, String> {
    let edge = validate_edge_exists(db, edge_id)?;
    validate_topology_mutation_allowed(db, edge.spec_id.as_deref(), edge.loop_id.as_deref())?;
    db.delete_loop_edge(&edge.id).map_err(|e| e.to_string())?;
    Ok(edge)
}

/// Validated deletion of a node — cascades (at the DB layer, via `ON DELETE
/// CASCADE` foreign keys on `loop_edges.from_node`/`to_node`) to every edge
/// naming it. The shared path used by both the `loop_delete_node` MCP tool
/// and the TUI's graph editor.
pub(crate) fn delete_loop_node_checked(db: &Database, node_id: &str) -> Result<LoopNode, String> {
    let node = validate_node_exists(db, node_id)?;
    validate_topology_mutation_allowed(db, node.spec_id.as_deref(), node.loop_id.as_deref())?;
    validate_node_not_ensemble_owned(db, &node.id)?;
    validate_node_not_entry_point(db, &node)?;
    db.delete_loop_node(&node.id).map_err(|e| e.to_string())?;
    Ok(node)
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
        "interrupted" => Ok(LoopSpecStatus::Interrupted),
        _ => Err(
            "Spec status must be one of: pending, running, completed, failed, skipped, interrupted."
                .to_string(),
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

/// Whether an agent node's config names a harness — a non-empty `platform`
/// or `cli` field. Shared by [`validate_node_config`]'s `Agent` arm (the
/// general "this config can run" gate) and `resolve_node_kind_and_config`'s
/// blueprint branch (which needs the same check before merge, to give a
/// blueprint-specific error instead of the generic one).
fn config_has_agent_harness(map: &serde_json::Map<String, serde_json::Value>) -> bool {
    let has_non_empty_str = |field: &str| {
        map.get(field)
            .and_then(serde_json::Value::as_str)
            .is_some_and(|s| !s.trim().is_empty())
    };
    has_non_empty_str("platform") || has_non_empty_str("cli")
}

/// Every config key an `agent` node is actually read for — see
/// `execute_agent_node`/`resolve_node_prompt_template` in `loop_engine.rs`.
/// Notably absent: `prompt`. An agent's prompt is `prompt_template` (or
/// `prompt_preset`); `prompt` is a plain node never reads, and a node that
/// carries it silently runs on the bare fallback template instead — the
/// defect this allowlist exists to catch. `commit_rights` (B37) is accepted
/// on every kind since the engine checks it uniformly regardless of which
/// node in the graph actually moves HEAD. `require_report` (boolean, default
/// `false`) exists because a harness can exit 0 having done nothing — see
/// `agent_finished_execution` in `loop_engine.rs`.
const AGENT_CONFIG_KEYS: &[&str] = &[
    "platform",
    "cli",
    "model",
    "timeout_minutes",
    "resume",
    "resume_prompt",
    "prompt_template",
    "prompt_preset",
    "require_report",
    "commit_rights",
];
/// Every config key a `check` node is read for — see `execute_check_node`.
const CHECK_CONFIG_KEYS: &[&str] = &[
    "command",
    "success_condition",
    "timeout_seconds",
    "commit_rights",
];
/// Every config key a `gate` node is read for — see `execute_gate_node`.
const GATE_CONFIG_KEYS: &[&str] = &["evaluate", "value", "commit_rights"];
/// Every config key a `router` node is read for — see `parse_router_routes`.
const ROUTER_CONFIG_KEYS: &[&str] = &["routes", "fallback", "commit_rights"];

/// The config keys a node's kind is actually read for, or `None` for `Join`
/// (engine-managed — see `validate_node_config`'s `Join` arm — so there is
/// no caller-supplied key to check against). Shared by
/// [`validate_known_config_keys`] (write-time rejection) and
/// [`unknown_config_keys`] (read-only detection of already-stored nodes via
/// the `loop_audit_node_configs` tool), so the two can never name a
/// different accepted set for the same kind.
fn allowed_config_keys(kind: LoopNodeKind) -> Option<&'static [&'static str]> {
    match kind {
        LoopNodeKind::Agent => Some(AGENT_CONFIG_KEYS),
        LoopNodeKind::Check => Some(CHECK_CONFIG_KEYS),
        LoopNodeKind::Gate => Some(GATE_CONFIG_KEYS),
        LoopNodeKind::Router => Some(ROUTER_CONFIG_KEYS),
        LoopNodeKind::Join => None,
    }
}

/// Every key in `map` that `kind` will never read, sorted. Empty for `Join`
/// (see [`allowed_config_keys`]) and for a config that only carries
/// recognized keys. Pure read-only classification — no error message, no
/// early return — so it doubles as both [`validate_known_config_keys`]'s
/// rejection check and `loop_audit_node_configs`'s detection query over
/// nodes that predate this validation.
fn unknown_config_keys(
    kind: LoopNodeKind,
    map: &serde_json::Map<String, serde_json::Value>,
) -> Vec<String> {
    let Some(allowed) = allowed_config_keys(kind) else {
        return Vec::new();
    };
    let mut unknown: Vec<String> = map
        .keys()
        .filter(|key| !allowed.contains(&key.as_str()))
        .cloned()
        .collect();
    unknown.sort_unstable();
    unknown
}

/// Reject any config key a node's kind will never read. This is what makes
/// `{ "platform": "claude", "prompt": "..." }` on an agent node fail loudly
/// at write time instead of being accepted, stored, and silently run on the
/// bare fallback template (`resolve_node_prompt_template`'s default) — the
/// exact incident this check exists to prevent. `prompt` on an agent node
/// gets its own message naming `prompt_template` as the field that actually
/// gets read; every other unrecognized key gets the generic message naming
/// what the kind does accept.
fn validate_known_config_keys(
    kind: LoopNodeKind,
    map: &serde_json::Map<String, serde_json::Value>,
) -> Result<(), String> {
    let unknown = unknown_config_keys(kind, map);
    let Some(bad_key) = unknown.first() else {
        return Ok(());
    };
    // `allowed_config_keys` is `None` only for `Join`, for which
    // `unknown_config_keys` always returns empty — so reaching here means
    // it returned `Some`.
    let allowed = allowed_config_keys(kind).expect("non-empty unknown keys implies Some(allowed)");
    let mut accepted = allowed.to_vec();
    accepted.sort_unstable();

    if kind == LoopNodeKind::Agent && bad_key == "prompt" {
        return Err(format!(
            "Loop node config for kind 'agent' does not read a 'prompt' key; use 'prompt_template' (or 'prompt_preset') instead. Accepted keys: {}.",
            accepted.join(", ")
        ));
    }
    Err(format!(
        "Loop node config for kind '{}' has unrecognized key '{bad_key}'. Accepted keys: {}.",
        kind.as_str(),
        accepted.join(", ")
    ))
}

/// Validate that a loop node's config is a JSON object with the fields its
/// kind needs at execution time. This exists because a double-encoded config
/// (e.g. `"{\"platform\": \"mimo\"}"` instead of `{"platform": "mimo"}`) used
/// to be accepted at creation time and only surfaced as an engine crash
/// ("Agent node ... is missing a platform/cli") mid-run, long after the node
/// was saved. It also rejects any key the node's kind will never read (see
/// [`validate_known_config_keys`]) — the same "surfaced at write time, not
/// run time" guarantee, for the case where the config shape is otherwise
/// valid but carries a key like `prompt` that a typo or a wrong mental model
/// (confusing it with the loop completion hook's own `prompt` field) put
/// there instead of `prompt_template`.
pub(crate) fn validate_node_config(
    kind: LoopNodeKind,
    config: &serde_json::Value,
) -> Result<(), String> {
    let Some(map) = config.as_object() else {
        return Err(format!(
            "Loop node config must be a JSON object, not {}. Pass an object (e.g. {{\"platform\": \"claude\"}}) rather than a JSON-encoded string.",
            json_value_kind_name(config)
        ));
    };

    validate_known_config_keys(kind, map)?;

    let has_non_empty_str = |field: &str| {
        map.get(field)
            .and_then(serde_json::Value::as_str)
            .is_some_and(|s| !s.trim().is_empty())
    };

    match kind {
        LoopNodeKind::Agent => {
            if !config_has_agent_harness(map) {
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
        LoopNodeKind::Router => {
            let (routes, fallback) = parse_router_routes(config)?;
            validate_router_routes(&routes, &fallback)?;
        }
    }

    Ok(())
}

/// Parse a router node's `config` into its declared routes + fallback
/// label. Shared by [`validate_node_config`]'s config-shape check and by
/// edge validation (`loop_add_edge`/`loop_update_edge`/`loop_update_node`),
/// which need to check a route name against what a router actually
/// declares.
fn parse_router_routes(config: &serde_json::Value) -> Result<(Vec<RouterRoute>, String), String> {
    let map = config.as_object().ok_or_else(|| {
        format!(
            "Loop node config must be a JSON object, not {}. Pass an object (e.g. {{\"routes\": [...]}}) rather than a JSON-encoded string.",
            json_value_kind_name(config)
        )
    })?;
    let routes_value = map.get("routes").ok_or_else(|| {
        "Loop node config for kind 'router' must include a 'routes' array.".to_string()
    })?;
    let routes_array = routes_value
        .as_array()
        .ok_or_else(|| "Loop node config field 'routes' must be an array.".to_string())?;
    let routes = routes_array
        .iter()
        .map(|entry| {
            let obj = entry.as_object().ok_or_else(|| {
                "Each router route must be an object with 'label' and 'description' fields."
                    .to_string()
            })?;
            let label = obj
                .get("label")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("")
                .to_string();
            let description = obj
                .get("description")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("")
                .to_string();
            Ok(RouterRoute { label, description })
        })
        .collect::<Result<Vec<_>, String>>()?;
    let fallback = map
        .get("fallback")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("")
        .to_string();
    Ok((routes, fallback))
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
            // Blueprints intentionally carry no platform/cli (see
            // `builtin_blueprint_specs`) — which harness runs a node is the
            // caller's decision, supplied here via `config_overrides`, never
            // a default this resolver falls back to. Catching the missing
            // case here (rather than only via `validate_node_config`'s
            // generic message) lets the error name the blueprint and say
            // *why* the field is missing instead of just that it is.
            if node_kind == LoopNodeKind::Agent {
                let has_harness = merged.as_object().is_some_and(config_has_agent_harness);
                if !has_harness {
                    return Err(format!(
                        "Blueprint '{blueprint_name}' intentionally does not provide a platform (or cli) — which harness runs a node is the caller's decision, not the blueprint's. Pass config_overrides with a non-empty 'platform' (and 'model' if that platform needs one) to select the harness."
                    ));
                }
            }
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

fn validate_queue_exists(db: &Database, queue_id: &str) -> Result<Queue, String> {
    db.get_queue(queue_id)
        .map_err(|e| e.to_string())?
        .ok_or_else(|| format!("Queue '{queue_id}' not found."))
}

/// Refuse to start a queue run when one of the queue's specs is already
/// `running` under a different loop. A queue spec's own `loop_id` stays
/// `None` (queue membership never binds it), so ownership is read off the
/// spec's most recent `loop_runs` row instead — the loop that most recently
/// touched the spec is the only one that could have set it `running`.
///
/// This is a start-time check, not a lock: two `loop_run` calls issued in
/// the same instant, before either has run a single node, can still race.
fn validate_queue_not_consumed(
    db: &Database,
    queue_id: &str,
    requesting_loop_id: &str,
) -> Result<(), String> {
    for spec_id in db
        .list_queue_member_spec_ids(queue_id)
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
                "Queue '{queue_id}' spec '{spec_id}' is already running under loop '{owner}'; wait for it to finish, or pause that loop, before starting a new run against this queue."
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
fn validate_queue_reorder(current: &[String], spec_ids: &[String]) -> Result<(), String> {
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

/// Live queues (R6): refuse to remove a queue member that is currently
/// `running` — the tool-layer half of the same lock `queue_reorder` enforces
/// via [`validate_queue_reorder_locking`]. A spec that isn't a queue member at
/// all, or isn't found, is left for [`Database::remove_queue_member`]'s own
/// "no such spec" error — this only ever blocks a positive `running` match.
fn validate_queue_member_removable(
    db: &Database,
    queue_id: &str,
    spec_id: &str,
) -> Result<(), String> {
    if let Some(spec) = db.get_loop_spec(spec_id).map_err(|e| e.to_string())? {
        if spec.status == LoopSpecStatus::Running {
            return Err(format!(
                "Spec '{spec_id}' is currently running and cannot be removed from queue '{queue_id}'; wait for it to finish, or pause the loop, first."
            ));
        }
    }
    Ok(())
}

/// Live queues (R6): the currently running spec and every already-executed
/// one (`completed`/`failed`/`skipped`) are immutable in the queue's order —
/// only `pending` members may move. Call after [`validate_queue_reorder`] has
/// already confirmed `spec_ids` is a total permutation of `current`: under
/// that guarantee, a locked member "doesn't move" iff it sits at the same
/// index in both slices, since moving it necessarily displaces whatever now
/// occupies its old slot.
fn validate_queue_reorder_locking(
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

fn queue_details_json(details: &QueueDetails, include_descriptions: bool) -> serde_json::Value {
    serde_json::json!({
        "id": details.queue.id,
        "name": details.queue.name,
        "members": details
            .members
            .iter()
            .enumerate()
            .map(|(index, spec)| {
                let mut value = spec_summary_json(spec, include_descriptions);
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

/// Ground truth for "is this loop actually busy right now": any `loop_runs`
/// row still `running` under it, regardless of what the loop's own `status`
/// column says. Status and a run's lifetime are independent — a sibling
/// node's `loop_report_blocker` can flip status to `paused` while a
/// different node under the same loop keeps executing — so `loop_run` and
/// [`Database::reset_loop`] both consult this (the latter internally, since
/// the scheduler's autorun also calls it directly) instead of trusting
/// status alone. One indexed query (`idx_loop_runs_loop_started`) at
/// dispatch time.
///
/// Returns the first still-`running` row found — an actionable pointer for a
/// human operator (wait or kill), not an exhaustive list.
fn find_in_flight_run(db: &Database, loop_id: &str) -> Result<Option<LoopNodeRun>, String> {
    Ok(db
        .list_running_loop_runs(loop_id)
        .map_err(|e| e.to_string())?
        .into_iter()
        .next())
}

/// The actionable refusal text for a `loop_run`/`loop_reset` call blocked by
/// [`find_in_flight_run`] (or [`Database::reset_loop`]'s own equivalent
/// check): names the node and run still executing, and since when, because
/// the caller's next decision is to wait for it or terminate it.
fn in_flight_run_error(
    loop_id: &str,
    run_id: &str,
    node_name: &str,
    started_at: chrono::DateTime<chrono::Utc>,
) -> String {
    format!(
        "Loop '{loop_id}' has node '{node_name}' (run '{run_id}') still executing, running \
         since {} — wait for it to finish, or terminate it, before retrying.",
        started_at.to_rfc3339()
    )
}

/// `node_id`'s display name, falling back to the raw id if the node row is
/// somehow gone (e.g. deleted out from under a still-running attempt) — used
/// to build [`in_flight_run_error`]'s message from a bare `LoopNodeRun`.
fn node_name_for_error(db: &Database, node_id: &str) -> Result<String, String> {
    Ok(db
        .get_loop_node(node_id)
        .map_err(|e| e.to_string())?
        .map(|node| node.name)
        .unwrap_or_else(|| node_id.to_string()))
}

/// Validate every ensemble (F1) reachable by a `loop_run` call — the loop's
/// own top-level graph, plus the own graph of every spec that could actually
/// run (the loop's bound specs, and a queue's members when `queue_id` is
/// given). A spec with no nodes of its own falls back to the loop-level
/// graph at execution time (see `LoopEngine::run_spec`), so it's skipped
/// here rather than double-validated.
fn validate_loop_ensembles_for_run(
    db: &Database,
    loop_id: &str,
    queue_id: Option<&str>,
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
    if let Some(queue_id) = queue_id {
        spec_ids.extend(
            db.list_queue_member_spec_ids(queue_id)
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
        LoopResetOutcome::InFlight {
            run_id,
            node_id,
            started_at,
        } => {
            let node_name = node_name_for_error(db, &node_id).map_err(internal_error)?;
            return Ok(error_result(&in_flight_run_error(
                loop_id, &run_id, &node_name, started_at,
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
    let resolved_run_id = match db
        .resolve_run_id_by_prefix(run_id)
        .map_err(|e| e.to_string())
    {
        Ok(Some(id)) => id,
        Ok(None) => run_id.to_string(),
        Err(e) => return Ok(Err(error_result(&e))),
    };
    let resolved_node_id = match db
        .resolve_loop_node_id_by_prefix(node_id)
        .map_err(|e| e.to_string())
    {
        Ok(Some(id)) => id,
        Ok(None) => node_id.to_string(),
        Err(e) => return Ok(Err(error_result(&e))),
    };
    let run = db.get_loop_run(&resolved_run_id).map_err(internal_error)?;
    let Some(run) = run else {
        return Ok(Err(error_result(&format!(
            "No loop run found with id '{resolved_run_id}'."
        ))));
    };
    if run.node_id != resolved_node_id {
        return Ok(Err(error_result(&format!(
            "Run '{resolved_run_id}' belongs to node '{}', not '{resolved_node_id}'.",
            run.node_id
        ))));
    }
    if run.status != LoopRunStatus::Running {
        return Ok(Err(error_result(&format!(
            "Run '{resolved_run_id}' for node '{resolved_node_id}' is no longer active (status: {}); this report \
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
fn spec_summary_json(spec: &LoopSpec, include_descriptions: bool) -> serde_json::Value {
    if include_descriptions {
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
    } else {
        serde_json::json!({
            "id": spec.id,
            "loop_id": spec.loop_id,
            "name": spec.name,
            "status": spec.status.as_str(),
            "workdir": spec.workdir,
        })
    }
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
            let resolved = db
                .resolve_spec_id_by_prefix(spec_id)
                .map_err(|e| e.to_string())?
                .ok_or_else(|| format!("Spec '{spec_id}' not found."))?;
            validate_spec_exists(db, &resolved)?;
            Ok(GraphTarget::Spec(resolved))
        }
        (None, Some(loop_id)) => {
            let resolved = db
                .resolve_loop_id_by_prefix(loop_id)
                .map_err(|e| e.to_string())?
                .ok_or_else(|| format!("Loop '{loop_id}' not found."))?;
            validate_loop_exists(db, &resolved)?;
            Ok(GraphTarget::Loop(resolved))
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
    let source_id = db
        .resolve_loop_node_id_by_prefix(params.source_node_id.trim())
        .map_err(|e| e.to_string())?
        .ok_or_else(|| format!("Node '{}' not found.", params.source_node_id.trim()))?;
    let source = validate_node_exists(db, &source_id)?;
    if source.kind == LoopNodeKind::Join {
        return Err(
            "Cannot copy a quorum node directly; copy its ensemble with loop_copy_ensemble."
                .to_string(),
        );
    }
    if let Err(e) = validate_node_not_ensemble_owned(db, &source_id) {
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
        wiring.insert("entry_from_node".into(), serde_json::json!(from));
        wiring.insert(
            "entry_condition".into(),
            serde_json::json!(condition.as_str()),
        );
        edges.push(LoopEdge {
            id: uuid::Uuid::new_v4().to_string(),
            spec_id: spec_id.clone(),
            loop_id: loop_id.clone(),
            from_node: from.to_string(),
            to_node: new_id.clone(),
            condition,
        });
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
        source_id,
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
    let source_id = db
        .resolve_ensemble_id_by_prefix(params.source_ensemble_id.trim())
        .map_err(|e| e.to_string())?
        .ok_or_else(|| format!("Ensemble '{}' not found.", params.source_ensemble_id.trim()))?;
    let details = db
        .get_ensemble_details(&source_id)
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
    let members: Vec<EnsembleMemberSpec> = match &params.members {
        Some(explicit) => validate_ensemble_members(explicit)?,
        None => details
            .members
            .iter()
            .map(|m| {
                (
                    m.platform.clone(),
                    m.model.clone(),
                    m.prompt_override.clone(),
                )
            })
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
        None => source.entry_condition.clone(),
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
        entry_condition: entry_condition.clone(),
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
    /// C19: how many separate loop executions this spec has failed with a
    /// genuine verdict — the persisted counter behind the cross-run attempt
    /// budget. Surfaced so an operator can see a spec approaching the limit
    /// before it actually blocks the loop, not just after.
    cross_run_attempts: i64,
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
    let cross_run_attempts = db
        .get_loop_spec_cross_run_attempts(&spec.id)
        .map_err(internal_error)?;
    Ok(SpecRunInfo {
        name: spec.name.clone(),
        current_node,
        blocker,
        cross_run_attempts,
    })
}

fn build_loop_summary_json(db: &Database, lp: &Loop) -> Result<serde_json::Value, McpError> {
    let specs = db.list_loop_specs(&lp.id).map_err(internal_error)?;
    // `Failed` is included so a spec that dead-ended on a failing node with
    // no outgoing edge still surfaces here — dispatch stops on the first
    // `Failed` spec (never advances past it), so it's always the earliest
    // non-terminal spec in position order and `find` still picks it over
    // any untouched `Pending` spec that was never reached.
    let current_spec = specs
        .into_iter()
        .find(|spec| {
            matches!(
                spec.status,
                LoopSpecStatus::Running
                    | LoopSpecStatus::Pending
                    | LoopSpecStatus::Interrupted
                    | LoopSpecStatus::Failed
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
        "blocker": current_spec.as_ref().and_then(|v| v.blocker.clone()),
        "spec_attempts": current_spec.as_ref().map(|v| v.cross_run_attempts),
        "created_at": lp.created_at.to_rfc3339(),
        "workdir": lp.workdir,
        "archived": lp.archived,
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

    /// Resolve the project hash a delete tool should scope itself to: an
    /// explicit `provided` value always wins (the same override the read
    /// tools allow), otherwise it's auto-detected from the caller's session
    /// workdir. Returns `None` if neither is available.
    fn effective_project_hash_for_delete(
        &self,
        parts: Option<&Parts>,
        provided: Option<&str>,
    ) -> Option<String> {
        if let Some(project_hash) = provided {
            return Some(project_hash.to_string());
        }
        let agent_id = self.resolve_sync_agent_id(parts).ok()?;
        resolve_effective_project_hash(&self.db, None, &agent_id)
    }

    /// `Some(error result)` if `node` belongs to a project other than
    /// `effective_project_hash`, `None` if deletion may proceed. Nodes
    /// without a `project_hash` aren't project-scoped, so they're always in
    /// scope.
    fn check_intelligence_delete_scope(
        &self,
        node: &crate::db::intelligence::IntelligenceNodeRecord,
        effective_project_hash: Option<&str>,
    ) -> Option<CallToolResult> {
        let node_project_hash = node.project_hash.as_deref()?;
        if effective_project_hash == Some(node_project_hash) {
            return None;
        }
        Some(error_result(&format!(
            "Intelligence node '{}' belongs to project '{}', which is outside the caller's \
             active project scope. Pass project_hash explicitly to operate on it anyway.",
            node.id, node_project_hash
        )))
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
        description = "List AI models available for use with agents. Pass an optional `platform` (e.g. \"opencode\") to filter to the models that platform can reach; pass `refresh: true` to force a fresh fetch instead of serving a cache that's within its TTL (`canopy models refresh` does the same from the CLI, for one platform or all). Every returned model id is the literal string that platform's CLI accepts for its model field — for a universal gateway that is the provider-prefixed form (opencode/big-pickle), for claude the bare form (claude-opus-4-8) — so an id can be copied verbatim into the model field of agent_add or agent_watch. Includes cache provenance (source: cache|live|stale, fetched_at, age, and whether a refresh is due) — the TTL itself is configurable in config.toml under `[models]`."
    )]
    async fn task_models(
        &self,
        Parameters(params): Parameters<TaskModelsParams>,
    ) -> Result<CallToolResult, McpError> {
        let force_refresh = params.refresh.unwrap_or(false);
        let full = params.full.unwrap_or(false);

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
                return Ok(native_models_result(platform, binary, args, force_refresh, full).await);
            }
        }

        // models.dev-derived path: the all-providers listing, or a platform
        // without native enumeration (e.g. claude, whose bare ids are correct).
        let ttl = configured_models().catalog_ttl();
        let load = tokio::task::spawn_blocking(move || {
            crate::domain::models_db::load_catalog_with_source(force_refresh, ttl)
        })
        .await
        .ok()
        .flatten();

        let Some(load) = load else {
            return Ok(error_result(
                "Model catalog unavailable: could not reach models.dev and no local \
                 cache exists at ~/.canopy/cache/models_catalog.json. Omit the model field to \
                 use the CLI's default, or retry once network access is restored.",
            ));
        };
        let crate::domain::models_db::CatalogLoad { catalog, source } = load;

        let (listing, truncation) = match platform {
            Some(platform) => {
                let providers = crate::domain::models_db::providers_for_cli(platform);
                if providers.is_empty() {
                    return Ok(error_result(&format!(
                        "No known model providers are mapped for platform '{platform}'. \
                         Omit `platform` to list all providers.",
                    )));
                }
                let (formatted, trunc) = format_platform_models(&catalog, providers, full);
                let mut listing = format!(
                    "Models available to platform '{platform}' (providers: {}):\n{formatted}",
                    providers.join(", ")
                );
                if let Some(warning) = platform_model_selection_warning(platform) {
                    listing = format!("{warning}\n\n{listing}");
                }
                (listing, trunc)
            }
            None => {
                let (formatted, trunc) = format_catalog_models(&catalog, full);
                (
                    format!("Available models (use the model id as the model field):\n{formatted}"),
                    trunc,
                )
            }
        };

        Ok(CallToolResult::success(vec![Content::text(
            model_result_footer(&listing, source, catalog.fetched_at, ttl, &truncation),
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

        // Track an explicit id that resolved to nothing: the caller almost
        // certainly mistyped or truncated the id of a node they meant to
        // update, so a bare `created: true` (which also fires for the normal
        // auto-generated-id path) is not a loud enough signal — see CB13.
        let mut unresolved_explicit_id: Option<String> = None;
        let resolved_id = match &params.node_data.id {
            Some(raw_id) if !raw_id.trim().is_empty() => {
                let raw_id = raw_id.trim();
                match self.db.resolve_node_id_by_prefix(raw_id) {
                    Ok(Some(full)) => Some(full),
                    Ok(None) => {
                        unresolved_explicit_id = Some(raw_id.to_string());
                        Some(raw_id.to_string())
                    }
                    Err(e) => return Ok(error_result(&e.to_string())),
                }
            }
            _ => params.node_data.id.clone(),
        };

        let node = crate::db::intelligence::IntelligenceNodeInput {
            id: resolved_id,
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

        let (record, created) = self
            .db
            .upsert_intelligence_node(node)
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;

        let mut out = serde_json::json!({
            "node": intelligence_node_json(&record),
            "created": created,
        });
        if created {
            if let Some(bad_id) = unresolved_explicit_id {
                out["warning"] = serde_json::json!(format!(
                    "Created a NEW node with id '{}'. No existing node matched that id or \
                     prefix, so nothing was updated. If you meant to update an existing node, \
                     the id you passed is wrong — look it up with intelligence_search first.",
                    bad_id
                ));
            }
        }

        Ok(CallToolResult::success(vec![Content::text(
            serde_json::to_string_pretty(&out).unwrap_or_default(),
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
        let search_result = self
            .db
            .search_intelligence_nodes(&params.query, params.kind.as_deref(), limit)
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;

        let out = serde_json::json!({
            "query": params.query,
            "kind": params.kind,
            "count": search_result.results.len(),
            "examined_count": search_result.examined_count,
            "results": search_result.results.iter().map(intelligence_node_json).collect::<Vec<_>>(),
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
        let effective_id = match self.db.resolve_node_id_by_prefix(&params.node_id) {
            Ok(Some(full)) => full,
            Ok(None) => {
                return Ok(error_result(&format!(
                    "Intelligence node '{}' not found.",
                    params.node_id
                )))
            }
            Err(e) => return Ok(error_result(&e.to_string())),
        };
        let graph = match self.db.walk_intelligence_graph(&effective_id, depth) {
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
        name = "intelligence_delete_node",
        description = "Delete an intelligence node and every relation touching it, in one \
         transaction (hard delete — the graph is meant to be able to forget). project_hash is \
         auto-detected from the session workdir like the read tools; pass it explicitly to \
         delete a node belonging to a different project. Returns a clear error, not a silent \
         success, if the node does not exist."
    )]
    async fn intelligence_delete_node(
        &self,
        Parameters(params): Parameters<IntelligenceDeleteNodeParams>,
        OptionalExtension(parts): OptionalExtension<Parts>,
    ) -> Result<CallToolResult, McpError> {
        self.reject_if_nursery(parts.as_ref())?;
        if let Err(e) = validate_non_empty(&params.node_id, "node_id") {
            return Ok(error_result(&e));
        }

        let effective_project_hash =
            self.effective_project_hash_for_delete(parts.as_ref(), params.project_hash.as_deref());

        let effective_id = match self.db.resolve_node_id_by_prefix(&params.node_id) {
            Ok(Some(full)) => full,
            Ok(None) => {
                return Ok(error_result(&format!(
                    "Intelligence node '{}' not found.",
                    params.node_id
                )))
            }
            Err(e) => return Ok(error_result(&e.to_string())),
        };

        let node = match self.db.get_intelligence_node(&effective_id) {
            Ok(Some(node)) => node,
            Ok(None) => {
                return Ok(error_result(&format!(
                    "Intelligence node '{}' not found.",
                    params.node_id
                )))
            }
            Err(e) => return Err(McpError::internal_error(e.to_string(), None)),
        };

        if let Some(scope_error) =
            self.check_intelligence_delete_scope(&node, effective_project_hash.as_deref())
        {
            return Ok(scope_error);
        }

        let relations_removed = match self.db.delete_intelligence_node(&effective_id) {
            Ok(Some(count)) => count,
            Ok(None) => {
                return Ok(error_result(&format!(
                    "Intelligence node '{}' not found.",
                    params.node_id
                )))
            }
            Err(e) => return Err(McpError::internal_error(e.to_string(), None)),
        };

        Ok(CallToolResult::success(vec![Content::text(
            serde_json::to_string_pretty(&serde_json::json!({
                "deleted_node_id": effective_id,
                "relations_removed": relations_removed,
            }))
            .unwrap_or_default(),
        )]))
    }

    #[tool(
        name = "intelligence_delete_relation",
        description = "Delete a single relation (edge) by ID, leaving both endpoint nodes \
         intact. project_hash is auto-detected from the session workdir like the read tools; \
         pass it explicitly to delete a relation touching a different project's nodes. Returns \
         a clear error, not a silent success, if the relation does not exist."
    )]
    async fn intelligence_delete_relation(
        &self,
        Parameters(params): Parameters<IntelligenceDeleteRelationParams>,
        OptionalExtension(parts): OptionalExtension<Parts>,
    ) -> Result<CallToolResult, McpError> {
        self.reject_if_nursery(parts.as_ref())?;

        let effective_project_hash =
            self.effective_project_hash_for_delete(parts.as_ref(), params.project_hash.as_deref());

        let edge = match self.db.get_intelligence_edge(params.edge_id) {
            Ok(Some(edge)) => edge,
            Ok(None) => {
                return Ok(error_result(&format!(
                    "Intelligence relation '{}' not found.",
                    params.edge_id
                )))
            }
            Err(e) => return Err(McpError::internal_error(e.to_string(), None)),
        };

        for node_id in [&edge.from_node_id, &edge.to_node_id] {
            if let Ok(Some(node)) = self.db.get_intelligence_node(node_id) {
                if let Some(scope_error) =
                    self.check_intelligence_delete_scope(&node, effective_project_hash.as_deref())
                {
                    return Ok(scope_error);
                }
            }
        }

        let removed = self
            .db
            .delete_intelligence_edge(params.edge_id)
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;

        if !removed {
            return Ok(error_result(&format!(
                "Intelligence relation '{}' not found.",
                params.edge_id
            )));
        }

        Ok(CallToolResult::success(vec![Content::text(
            serde_json::to_string_pretty(&serde_json::json!({
                "deleted_edge_id": params.edge_id,
            }))
            .unwrap_or_default(),
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
            archived: false,
            paused_by_reconciliation: false,
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
            active_run_queue_id: None,
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
        let loop_id = match resolve_prefix_or_error(
            &self.db,
            params.loop_id.trim(),
            Database::resolve_loop_id_by_prefix,
            "loop",
        ) {
            Ok(id) => id,
            Err(e) => return Ok(e),
        };
        if let Err(e) = validate_non_empty(&loop_id, "Loop ID") {
            return Ok(error_result(&e));
        }
        if let Err(e) = validate_loop_exists(&self.db, &loop_id) {
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
            .update_loop_details(&loop_id, name, description, workdir)
            .map_err(internal_error)?;

        if let Some(trigger) = new_trigger {
            self.db
                .update_loop_trigger(&loop_id, trigger.as_ref())
                .map_err(internal_error)?;
            let _ = self.watcher_engine.stop_loop_watcher(&loop_id).await;
            if let Ok(Some(lp)) = self.db.get_loop(&loop_id) {
                self.activate_loop_trigger(&lp).await;
            }
        }

        if let Some(hook) = new_completion_hook {
            self.db
                .update_loop_completion_hook(&loop_id, hook.as_ref())
                .map_err(internal_error)?;
        }

        Ok(build_loop_update_response(&loop_id))
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
        let loop_id = match resolve_prefix_or_error(
            &self.db,
            params.loop_id.trim(),
            Database::resolve_loop_id_by_prefix,
            "loop",
        ) {
            Ok(id) => id,
            Err(e) => return Ok(e),
        };
        if let Err(e) = validate_loop_exists(&self.db, &loop_id) {
            return Ok(error_result(&e));
        }

        let existing_specs = self.db.list_loop_specs(&loop_id).map_err(internal_error)?;
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
            loop_id: Some(loop_id),
            name: name.to_string(),
            description: Some(description.to_string()),
            position: params.position,
            parallelizable: params.parallelizable,
            status: LoopSpecStatus::Pending,
            started_at: None,
            completed_at: None,
            spec_start_head: None,
            spec_committed_head: None,
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
        let spec_id = match resolve_prefix_or_error(
            &self.db,
            params.spec_id.trim(),
            Database::resolve_spec_id_by_prefix,
            "spec",
        ) {
            Ok(id) => id,
            Err(e) => return Ok(e),
        };
        let spec = match validate_spec_exists(&self.db, &spec_id) {
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
                validate_position_conflict(&self.db, spec.loop_id.as_deref(), &spec_id, position)
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
                &spec_id,
                name,
                description,
                params.position,
                params.parallelizable,
            )
            .map_err(internal_error)?;

        Ok(build_spec_update_response(&spec_id))
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
            spec_committed_head: None,
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

        let include_descriptions = params.include_descriptions.unwrap_or(false);
        let total = specs.len();
        let cap = 200usize;
        let truncated = total > cap;
        let omitted = total.saturating_sub(cap);
        let visible = &specs[..specs.len().min(cap)];

        let mut body = serde_json::json!({
            "specs": visible.iter().map(|s| spec_summary_json(s, include_descriptions)).collect::<Vec<_>>(),
            "total": total,
        });
        if truncated {
            body["truncated"] = serde_json::json!(true);
            body["omitted"] = serde_json::json!(omitted);
        }

        Ok(CallToolResult::success(vec![Content::text(
            serde_json::to_string_pretty(&body).unwrap_or_default(),
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
        let spec_id = match resolve_prefix_or_error(
            &self.db,
            params.spec_id.trim(),
            Database::resolve_spec_id_by_prefix,
            "spec",
        ) {
            Ok(id) => id,
            Err(e) => return Ok(e),
        };
        if let Err(e) = validate_spec_exists(&self.db, &spec_id) {
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
            .update_spec_tag_details(&spec_id, name, description, workdir)
            .map_err(internal_error)?;

        Ok(build_spec_update_response(&spec_id))
    }

    #[tool(
        name = "spec_set_status",
        description = "Administratively transition a standalone spec's status (completed, skipped, or pending). Rejects if the spec is bound to a loop or has an active run."
    )]
    async fn spec_set_status(
        &self,
        Parameters(params): Parameters<SpecSetStatusParams>,
    ) -> Result<CallToolResult, McpError> {
        let spec_id = match resolve_prefix_or_error(
            &self.db,
            params.spec_id.trim(),
            Database::resolve_spec_id_by_prefix,
            "spec",
        ) {
            Ok(id) => id,
            Err(e) => return Ok(e),
        };
        if let Err(e) = validate_spec_exists(&self.db, &spec_id) {
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

        match self.db.set_spec_admin_status(&spec_id, status, reason)
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
        let spec_id = match resolve_prefix_or_error(
            &self.db,
            params.spec_id.trim(),
            Database::resolve_spec_id_by_prefix,
            "spec",
        ) {
            Ok(id) => id,
            Err(e) => return Ok(e),
        };
        let spec = match validate_spec_exists(&self.db, &spec_id) {
            Ok(spec) => spec,
            Err(e) => return Ok(error_result(&e)),
        };
        if let Err(e) = validate_spec_deletable(&spec) {
            return Ok(error_result(&e));
        }

        self.db.delete_loop_spec(&spec_id).map_err(internal_error)?;

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
        let node_id = match resolve_prefix_or_error(
            &self.db,
            params.node_id.trim(),
            Database::resolve_loop_node_id_by_prefix,
            "node",
        ) {
            Ok(id) => id,
            Err(e) => return Ok(e),
        };
        let node = match validate_node_exists(&self.db, &node_id) {
            Ok(node) => node,
            Err(e) => return Ok(error_result(&e)),
        };
        if let Err(e) = validate_node_not_ensemble_owned(&self.db, &node_id) {
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
            if let Err(e) = validate_node_position_conflict(&self.db, &node, &node_id, position) {
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
            // A router's routes must stay consistent with whatever edges
            // already name them: no edge left pointing at a route that was
            // just removed, and (once wiring has started) no declared route
            // left without one — see `validate_router_route_coverage`.
            if effective_kind == LoopNodeKind::Router {
                let (routes, _fallback) = match parse_router_routes(effective_config) {
                    Ok(parsed) => parsed,
                    Err(e) => return Ok(error_result(&e)),
                };
                let edges = match (&node.spec_id, &node.loop_id) {
                    (Some(spec_id), _) => {
                        self.db.list_loop_edges(spec_id).map_err(internal_error)?
                    }
                    (_, Some(loop_id)) => self
                        .db
                        .list_loop_edges_for_loop(loop_id)
                        .map_err(internal_error)?,
                    (None, None) => Vec::new(),
                };
                if let Err(e) = validate_router_edges_declared(&routes, &node_id, &edges) {
                    return Ok(error_result(&e));
                }
                if let Err(e) = validate_router_route_coverage(&routes, &node_id, &edges) {
                    return Ok(error_result(&e));
                }
            }
        }

        self.db
            .update_loop_node_details(&node_id, name, kind, config.as_ref(), params.position)
            .map_err(internal_error)?;

        Ok(build_node_update_response(&node_id))
    }

    #[tool(
        name = "loop_add_edge",
        description = "Connect two nodes with a routing condition, inside a loop spec's graph or (via loop_id instead of spec_id) the loop's top-level graph."
    )]
    async fn loop_add_edge(
        &self,
        Parameters(params): Parameters<LoopAddEdgeParams>,
    ) -> Result<CallToolResult, McpError> {
        let condition = match validate_edge_condition_with_route(
            params.condition.trim(),
            params.route.as_deref(),
        ) {
            Ok(c) => c,
            Err(e) => return Ok(error_result(&e)),
        };
        let from_node = match resolve_prefix_or_error(
            &self.db,
            params.from_node.trim(),
            Database::resolve_loop_node_id_by_prefix,
            "node",
        ) {
            Ok(id) => id,
            Err(e) => return Ok(e),
        };
        let to_node = match resolve_prefix_or_error(
            &self.db,
            params.to_node.trim(),
            Database::resolve_loop_node_id_by_prefix,
            "node",
        ) {
            Ok(id) => id,
            Err(e) => return Ok(e),
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
        let has_from = nodes.iter().any(|node| node.id == from_node);
        let has_to = nodes.iter().any(|node| node.id == to_node);
        if !has_from || !has_to {
            return Ok(error_result(
                "Both loop edge endpoints must belong to the same spec or loop graph as the edge.",
            ));
        }
        if let Err(e) = validate_node_not_ensemble_owned(&self.db, &from_node) {
            return Ok(error_result(&e));
        }
        if let Err(e) = validate_node_not_ensemble_owned(&self.db, &to_node) {
            return Ok(error_result(&e));
        }
        if let Err(e) = validate_route_edge_target(&self.db, &from_node, &condition) {
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
            from_node,
            to_node,
            condition,
        };
        self.db.insert_loop_edge(&edge).map_err(internal_error)?;

        Ok(CallToolResult::success(vec![Content::text(
            serde_json::to_string_pretty(&serde_json::json!({ "ok": true })).unwrap_or_default(),
        )]))
    }

    #[tool(
        name = "loop_update_edge",
        description = "Update the routing condition and/or destination node of an existing loop edge. Omit to_node to leave the edge's target unchanged; provide it to retarget the edge in place (preserving its id and run history) instead of recreating it."
    )]
    async fn loop_update_edge(
        &self,
        Parameters(params): Parameters<LoopUpdateEdgeParams>,
    ) -> Result<CallToolResult, McpError> {
        let edge_id = match resolve_prefix_or_error(
            &self.db,
            params.edge_id.trim(),
            Database::resolve_edge_id_by_prefix,
            "edge",
        ) {
            Ok(id) => id,
            Err(e) => return Ok(e),
        };
        let edge = match validate_edge_exists(&self.db, &edge_id) {
            Ok(e) => e,
            Err(e) => return Ok(error_result(&e)),
        };
        if let Err(e) = validate_node_not_ensemble_owned(&self.db, &edge.from_node) {
            return Ok(error_result(&e));
        }
        if let Err(e) = validate_node_not_ensemble_owned(&self.db, &edge.to_node) {
            return Ok(error_result(&e));
        }
        let condition = match validate_edge_condition_with_route(
            params.condition.trim(),
            params.route.as_deref(),
        ) {
            Ok(c) => c,
            Err(e) => return Ok(error_result(&e)),
        };
        if let Err(e) = validate_route_edge_target(&self.db, &edge.from_node, &condition) {
            return Ok(error_result(&e));
        }

        let to_node = match &params.to_node {
            Some(raw) => {
                let trimmed = raw.trim();
                if trimmed.is_empty() {
                    None
                } else {
                    match resolve_prefix_or_error(
                        &self.db,
                        trimmed,
                        Database::resolve_loop_node_id_by_prefix,
                        "node",
                    ) {
                        Ok(id) => Some(id),
                        Err(e) => return Ok(e),
                    }
                }
            }
            None => None,
        };
        let target_changed = to_node
            .as_deref()
            .is_some_and(|value| value != edge.to_node);
        if target_changed {
            if let Err(e) = retarget_loop_edge(&self.db, &edge.id, to_node.as_deref().unwrap()) {
                return Ok(error_result(&e));
            }
        }

        let condition_changed = edge.condition != condition;
        if condition_changed {
            self.db
                .update_loop_edge_condition(&edge.id, &condition)
                .map_err(internal_error)?;
        }

        if !condition_changed && !target_changed {
            return Ok(success_result(&format!(
                "Loop edge '{}' already uses condition '{}' and target unchanged.",
                edge.id,
                condition.as_str()
            )));
        }

        Ok(success_result(&format!("Loop edge '{}' updated.", edge.id)))
    }

    #[tool(
        name = "loop_delete_edge",
        description = "Delete a single loop edge by id. Rejected while the owning loop is running — pause it first."
    )]
    async fn loop_delete_edge(
        &self,
        Parameters(params): Parameters<LoopDeleteEdgeParams>,
    ) -> Result<CallToolResult, McpError> {
        let edge_id = match resolve_prefix_or_error(
            &self.db,
            params.edge_id.trim(),
            Database::resolve_edge_id_by_prefix,
            "edge",
        ) {
            Ok(id) => id,
            Err(e) => return Ok(e),
        };
        match delete_loop_edge_checked(&self.db, &edge_id) {
            Ok(edge) => Ok(success_result(&format!("Loop edge '{}' deleted.", edge.id))),
            Err(e) => Ok(error_result(&e)),
        }
    }

    #[tool(
        name = "loop_delete_node",
        description = "Delete a loop node by id, cascading to every edge that names it as from_node or to_node. Rejected if the node is the graph's entry point, or while the owning loop is running — pause it first."
    )]
    async fn loop_delete_node(
        &self,
        Parameters(params): Parameters<LoopDeleteNodeParams>,
    ) -> Result<CallToolResult, McpError> {
        let node_id = match resolve_prefix_or_error(
            &self.db,
            params.node_id.trim(),
            Database::resolve_loop_node_id_by_prefix,
            "node",
        ) {
            Ok(id) => id,
            Err(e) => return Ok(e),
        };
        match delete_loop_node_checked(&self.db, &node_id) {
            Ok(node) => Ok(success_result(&format!("Loop node '{}' deleted.", node.id))),
            Err(e) => Ok(error_result(&e)),
        }
    }

    #[tool(
        name = "loop_add_ensemble",
        description = "Create an ensemble in ONE call: N (2-8) parallel agent-node members sharing one prompt by default, plus the quorum that waits for all of them, consolidates their outputs (attributed per member), and routes onward. Members differ by platform/model, and each may set its own prompt_override to review the same input from a different angle instead of sharing the template. (Formerly called 'fusion' — retired to avoid colliding with OpenRouter's fusion technology.)"
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

        let members: Vec<EnsembleMemberSpec> = match &params.members {
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

        let from_node = match resolve_prefix_or_error(
            &self.db,
            params.from_node.trim(),
            Database::resolve_loop_node_id_by_prefix,
            "node",
        ) {
            Ok(id) => id,
            Err(e) => return Ok(e),
        };
        if !node_exists(&from_node) {
            return Ok(error_result(&format!(
                "Loop node '{from_node}' not found in the target graph."
            )));
        }
        if let Err(e) = validate_node_not_ensemble_owned(&self.db, &from_node) {
            return Ok(error_result(&format!(
                "Cannot wire an ensemble's entry from an ensemble-owned node (nested ensembles are not supported): {e}"
            )));
        }

        let on_pass_to = match resolve_prefix_or_error(
            &self.db,
            params.on_pass_to.trim(),
            Database::resolve_loop_node_id_by_prefix,
            "node",
        ) {
            Ok(id) => id,
            Err(e) => return Ok(e),
        };
        if let Err(e) = validate_non_empty(&on_pass_to, "on_pass_to") {
            return Ok(error_result(&e));
        }
        if !node_exists(&on_pass_to) {
            return Ok(error_result(&format!(
                "Loop node '{on_pass_to}' not found in the target graph."
            )));
        }
        if let Err(e) = validate_node_not_ensemble_owned(&self.db, &on_pass_to) {
            return Ok(error_result(&format!(
                "Cannot wire an ensemble's exit into another ensemble's members/quorum (nested ensembles are not supported): {e}"
            )));
        }

        let on_fail_to = match &params.on_fail_to {
            Some(raw) => {
                match resolve_prefix_or_error(
                    &self.db,
                    raw.trim(),
                    Database::resolve_loop_node_id_by_prefix,
                    "node",
                ) {
                    Ok(id) => Some(id),
                    Err(e) => return Ok(e),
                }
            }
            None => None,
        };
        if let Some(ref on_fail_to) = on_fail_to {
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
            entry_from_node: &from_node,
            entry_condition: condition,
            on_pass_to: &on_pass_to,
            on_fail_to: on_fail_to.as_deref(),
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
        description = "Update an ensemble's shared prompt (propagated to every member without its own prompt_override), member list (platform/model/prompt_override — added/removed/replaced by position), quorum config (min_pass, straggler_timeout_minutes, timeout_minutes), and/or exit wiring (on_pass_to/on_fail_to) — all in one call, without touching individual member nodes directly."
    )]
    async fn loop_update_ensemble(
        &self,
        Parameters(params): Parameters<LoopUpdateEnsembleParams>,
    ) -> Result<CallToolResult, McpError> {
        let ensemble_id = match resolve_prefix_or_error(
            &self.db,
            params.ensemble_id.trim(),
            Database::resolve_ensemble_id_by_prefix,
            "ensemble",
        ) {
            Ok(id) => id,
            Err(e) => return Ok(e),
        };
        let Some(mut details) = self
            .db
            .get_ensemble_details(&ensemble_id)
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

            for (index, (platform, model, prompt_override)) in
                members.iter().enumerate().take(old_len.min(new_len))
            {
                let existing = &old_members[index];
                self.db
                    .update_ensemble_member(
                        &ensemble_id,
                        &existing.node_id,
                        platform,
                        model.as_deref(),
                        prompt_override.as_deref(),
                    )
                    .map_err(internal_error)?;
                let effective_prompt =
                    effective_member_prompt(prompt_override.as_deref(), prompt_template);
                let config = member_node_config(
                    platform,
                    model.as_deref(),
                    effective_prompt,
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
                for (i, (platform, model, prompt_override)) in
                    members[old_len..new_len].iter().enumerate()
                {
                    let next_position = start_position + i as i64;
                    let next_member_position = old_len as i64 + i as i64;
                    let node_id = uuid::Uuid::new_v4().to_string();
                    let effective_prompt =
                        effective_member_prompt(prompt_override.as_deref(), prompt_template);
                    let node = LoopNode {
                        id: node_id.clone(),
                        spec_id: details.ensemble.spec_id.clone(),
                        loop_id: details.ensemble.loop_id.clone(),
                        name: format!("{} [{}]", details.ensemble.name, next_member_position + 1),
                        kind: LoopNodeKind::Agent,
                        config: member_node_config(
                            platform,
                            model.as_deref(),
                            effective_prompt,
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
                        condition: details.ensemble.entry_condition.clone(),
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
                        prompt_override: prompt_override.clone(),
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
                .get_ensemble_details(&ensemble_id)
                .map_err(internal_error)?
                .ok_or_else(|| {
                    internal_error(format!("Ensemble '{ensemble_id}' vanished mid-update."))
                })?;
        } else if params.prompt_template.is_some() || params.timeout_minutes.is_some() {
            // Prompt and/or shared timeout changed without a member-list
            // resize: propagate onto every existing member's config as-is,
            // except a member with its own prompt_override keeps rendering
            // that instead of the (possibly just-changed) shared prompt.
            let prompt_template = params
                .prompt_template
                .as_deref()
                .unwrap_or(&details.ensemble.prompt_template);
            let timeout_minutes = params
                .timeout_minutes
                .unwrap_or(details.ensemble.timeout_minutes);
            for member in &details.members {
                let effective_prompt =
                    effective_member_prompt(member.prompt_override.as_deref(), prompt_template);
                let config = member_node_config(
                    &member.platform,
                    member.model.as_deref(),
                    effective_prompt,
                    timeout_minutes,
                );
                self.db
                    .update_loop_node_details(&member.node_id, None, None, Some(&config), None)
                    .map_err(internal_error)?;
            }
        }

        if let Some(prompt_template) = params.prompt_template.as_deref() {
            self.db
                .update_ensemble_prompt(&ensemble_id, prompt_template.trim())
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
                    &ensemble_id,
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
                        &LoopEdgeCondition::Pass,
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
                        &LoopEdgeCondition::Fail,
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
                    &ensemble_id,
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
    // loop.

    async fn do_queue_create(&self, name: &str) -> Result<CallToolResult, McpError> {
        let name = name.trim();
        if let Err(e) = validate_non_empty(name, "Queue name") {
            return Ok(error_result(&e));
        }

        let queue = Queue {
            id: uuid::Uuid::new_v4().to_string(),
            name: name.to_string(),
            created_at: chrono::Utc::now(),
        };
        self.db.insert_queue(&queue).map_err(internal_error)?;

        Ok(build_id_result(&queue.id, "queue_id"))
    }

    async fn do_queue_add_spec(
        &self,
        queue_id: &str,
        spec_id: &str,
        group: Option<&str>,
    ) -> Result<CallToolResult, McpError> {
        let queue_id = queue_id.trim();
        if let Err(e) = validate_queue_exists(&self.db, queue_id) {
            return Ok(error_result(&e));
        }
        let spec_id = spec_id.trim();
        if let Err(e) = validate_spec_exists(&self.db, spec_id) {
            return Ok(error_result(&e));
        }
        let already_member = self
            .db
            .queue_has_member(queue_id, spec_id)
            .map_err(internal_error)?;
        if already_member {
            return Ok(error_result(&format!(
                "Spec '{spec_id}' is already in queue '{queue_id}'."
            )));
        }

        // An all-whitespace or empty `group` is treated as ungrouped.
        let group = group.map(str::trim).filter(|g| !g.is_empty());

        self.db
            .append_queue_member(queue_id, spec_id, group)
            .map_err(internal_error)?;

        let group_note = group
            .map(|g| format!(" in group '{g}'"))
            .unwrap_or_default();
        Ok(success_result(&format!(
            "Spec '{spec_id}' added to queue '{queue_id}'{group_note}."
        )))
    }

    async fn do_queue_list(
        &self,
        queue_id: Option<&str>,
        include_descriptions: bool,
    ) -> Result<CallToolResult, McpError> {
        let queue_id = queue_id.map(str::trim).filter(|s| !s.is_empty());

        let body = match queue_id {
            Some(queue_id) => {
                let details = match self.db.get_queue_details(queue_id) {
                    Ok(Some(details)) => details,
                    Ok(None) => return Ok(error_result(&format!("Queue '{queue_id}' not found."))),
                    Err(e) => return Err(internal_error(e.to_string())),
                };
                let total = details.members.len();
                let cap = 200usize;
                let truncated = total > cap;
                let omitted = total.saturating_sub(cap);
                let visible_members = &details.members[..details.members.len().min(cap)];
                let visible_groups: std::collections::HashMap<_, _> = details
                    .member_groups
                    .iter()
                    .filter(|(id, _)| visible_members.iter().any(|m| &m.id == *id))
                    .map(|(k, v)| (k.clone(), v.clone()))
                    .collect();
                let trimmed_details = QueueDetails {
                    queue: details.queue,
                    members: visible_members.to_vec(),
                    member_groups: visible_groups,
                };
                let mut qobj = queue_details_json(&trimmed_details, include_descriptions);
                qobj["total_members"] = serde_json::json!(total);
                if truncated {
                    qobj["truncated"] = serde_json::json!(true);
                    qobj["omitted"] = serde_json::json!(omitted);
                }
                serde_json::json!({ "queue": qobj })
            }
            None => {
                let queues = self.db.list_queues().map_err(internal_error)?;
                serde_json::json!({
                    "queues": queues
                        .iter()
                        .map(|queue| serde_json::json!({
                            "id": queue.id,
                            "name": queue.name,
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
        if let Err(e) = validate_queue_exists(&self.db, queue_id) {
            return Ok(error_result(&e));
        }
        let spec_id = spec_id.trim();
        if let Err(e) = validate_queue_member_removable(&self.db, queue_id, spec_id) {
            return Ok(error_result(&e));
        }
        let removed = self
            .db
            .remove_queue_member(queue_id, spec_id)
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
        if let Err(e) = validate_queue_exists(&self.db, queue_id) {
            return Ok(error_result(&e));
        }
        let current = self
            .db
            .list_queue_member_spec_ids(queue_id)
            .map_err(internal_error)?;
        if let Err(e) = validate_queue_reorder(&current, spec_ids) {
            return Ok(error_result(&e));
        }
        if let Err(e) = validate_queue_reorder_locking(&self.db, &current, spec_ids) {
            return Ok(error_result(&e));
        }

        self.db
            .reorder_queue_members(queue_id, spec_ids)
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
        let queue_id = match resolve_prefix_or_error(
            &self.db,
            params.queue_id.trim(),
            Database::resolve_queue_id_by_prefix,
            "queue",
        ) {
            Ok(id) => id,
            Err(e) => return Ok(e),
        };
        let spec_id = match resolve_prefix_or_error(
            &self.db,
            params.spec_id.trim(),
            Database::resolve_spec_id_by_prefix,
            "spec",
        ) {
            Ok(id) => id,
            Err(e) => return Ok(e),
        };
        self.do_queue_add_spec(&queue_id, &spec_id, params.group.as_deref())
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
        let resolved_queue_id = match &params.queue_id {
            Some(qid) => {
                match resolve_prefix_or_error(
                    &self.db,
                    qid.trim(),
                    Database::resolve_queue_id_by_prefix,
                    "queue",
                ) {
                    Ok(id) => Some(id),
                    Err(e) => return Ok(e),
                }
            }
            None => None,
        };
        self.do_queue_list(
            resolved_queue_id.as_deref(),
            params.include_descriptions.unwrap_or(false),
        )
        .await
    }

    #[tool(
        name = "queue_remove_spec",
        description = "Remove a spec from a queue."
    )]
    async fn queue_remove_spec(
        &self,
        Parameters(params): Parameters<QueueRemoveSpecParams>,
    ) -> Result<CallToolResult, McpError> {
        let queue_id = match resolve_prefix_or_error(
            &self.db,
            params.queue_id.trim(),
            Database::resolve_queue_id_by_prefix,
            "queue",
        ) {
            Ok(id) => id,
            Err(e) => return Ok(e),
        };
        let spec_id = match resolve_prefix_or_error(
            &self.db,
            params.spec_id.trim(),
            Database::resolve_spec_id_by_prefix,
            "spec",
        ) {
            Ok(id) => id,
            Err(e) => return Ok(e),
        };
        self.do_queue_remove_spec(&queue_id, &spec_id).await
    }

    #[tool(
        name = "queue_reorder",
        description = "Reorder a queue. `spec_ids` must list every queue member exactly once, in the desired order — a total replacement, not a partial swap."
    )]
    async fn queue_reorder(
        &self,
        Parameters(params): Parameters<QueueReorderParams>,
    ) -> Result<CallToolResult, McpError> {
        let queue_id = match resolve_prefix_or_error(
            &self.db,
            params.queue_id.trim(),
            Database::resolve_queue_id_by_prefix,
            "queue",
        ) {
            Ok(id) => id,
            Err(e) => return Ok(e),
        };
        let mut resolved_spec_ids = Vec::with_capacity(params.spec_ids.len());
        for sid in &params.spec_ids {
            match resolve_prefix_or_error(
                &self.db,
                sid.trim(),
                Database::resolve_spec_id_by_prefix,
                "spec",
            ) {
                Ok(id) => resolved_spec_ids.push(id),
                Err(e) => return Ok(e),
            }
        }
        self.do_queue_reorder(&queue_id, &resolved_spec_ids).await
    }

    #[tool(
        name = "loop_get",
        description = "Return a loop with its ordered specs, nodes, and edges."
    )]
    async fn loop_get(
        &self,
        Parameters(params): Parameters<LoopGetParams>,
    ) -> Result<CallToolResult, McpError> {
        let loop_id = match resolve_prefix_or_error(
            &self.db,
            params.loop_id.trim(),
            Database::resolve_loop_id_by_prefix,
            "loop",
        ) {
            Ok(id) => id,
            Err(e) => return Ok(e),
        };
        let lp = match self.db.get_loop_details(&loop_id) {
            Ok(Some(w)) => w,
            Ok(None) => return Ok(error_result(&format!("Loop '{}' not found.", loop_id))),
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
        name = "loop_export",
        description = "Export a loop's design — name, description, nodes, edges, and ensembles — as a portable JSON document, so it can be shared as a file and recreated elsewhere with loop_import. Never includes ids, workdir, specs, or run/status state. platform/model are stripped from every agent node/ensemble member by default; pass with_models: true to keep them (only when exporting your own loop to restore later on your own machine)."
    )]
    async fn loop_export(
        &self,
        Parameters(params): Parameters<LoopExportParams>,
    ) -> Result<CallToolResult, McpError> {
        let loop_id = match resolve_prefix_or_error(
            &self.db,
            params.loop_id.trim(),
            Database::resolve_loop_id_by_prefix,
            "loop",
        ) {
            Ok(id) => id,
            Err(e) => return Ok(e),
        };
        let Some(lp) = self.db.get_loop(&loop_id).map_err(internal_error)? else {
            return Ok(error_result(&format!("Loop '{loop_id}' not found.")));
        };
        let with_models = params.with_models.unwrap_or(false);

        let graph_nodes = self
            .db
            .list_loop_nodes_for_loop(&loop_id)
            .map_err(internal_error)?;
        let graph_edges = self
            .db
            .list_loop_edges_for_loop(&loop_id)
            .map_err(internal_error)?;
        let ensembles = self
            .db
            .list_ensembles_for_loop(&loop_id)
            .map_err(internal_error)?;

        let document = match crate::domain::loop_transfer::build_export_document(
            &lp,
            &graph_nodes,
            &graph_edges,
            &ensembles,
            with_models,
        ) {
            Ok(document) => document,
            Err(e) => return Ok(error_result(&e)),
        };

        Ok(build_json_result(
            &serde_json::to_value(&document).map_err(internal_error)?,
        ))
    }

    #[tool(
        name = "loop_import",
        description = "Create a new loop from an exported document (the object loop_export returns). Always creates a new loop — never updates or overwrites an existing one; if the name is already taken in workdir, a numeric suffix is applied and the response says which name was used. Validates the document exactly as loop_add_node/loop_add_edge/loop_add_ensemble would, all-or-nothing: nothing is written if any part is rejected. The response lists every agent node left without a platform (the document strips it by default) so the caller knows what to fill in before running the loop."
    )]
    async fn loop_import(
        &self,
        Parameters(params): Parameters<LoopImportParams>,
    ) -> Result<CallToolResult, McpError> {
        let workdir = params.workdir.trim();
        if let Err(e) = validate_non_empty(workdir, "Loop workdir") {
            return Ok(error_result(&e));
        }
        if let Err(e) = validate_absolute_dir(workdir) {
            return Ok(error_result(&e));
        }

        let document =
            match crate::domain::loop_transfer::parse_export_document_value(&params.document) {
                Ok(document) => document,
                Err(e) => return Ok(error_result(&e)),
            };

        let desired_name = params
            .name
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .unwrap_or(document.name.trim());
        if let Err(e) = validate_non_empty(desired_name, "Loop name") {
            return Ok(error_result(&e));
        }

        let loop_id = uuid::Uuid::new_v4().to_string();
        let plan = match crate::domain::loop_transfer::build_import_plan(&document, &loop_id) {
            Ok(plan) => plan,
            Err(e) => return Ok(error_result(&e)),
        };
        let missing_platform = crate::domain::loop_transfer::agent_nodes_missing_platform(&plan);

        let existing_names: Vec<String> = self
            .db
            .list_loops(Some(workdir), true)
            .map_err(internal_error)?
            .into_iter()
            .map(|lp| lp.name)
            .collect();
        let final_name =
            crate::domain::loop_transfer::resolve_unique_loop_name(&existing_names, desired_name);

        let lp = Loop {
            archived: false,
            paused_by_reconciliation: false,
            id: loop_id.clone(),
            name: final_name.clone(),
            description: document
                .description
                .clone()
                .filter(|value| !value.trim().is_empty()),
            workdir: workdir.to_string(),
            status: LoopStatus::Draft,
            trigger: None,
            created_at: chrono::Utc::now(),
            started_at: None,
            completed_at: None,
            autorun_at: None,
            auto_continue_at: None,
            auto_continue_action: None,
            active_run_queue_id: None,
            on_completed: None,
        };

        self.db
            .import_loop_graph(&lp, &plan)
            .map_err(internal_error)?;
        if let Err(error) = self.db.register_project_path(std::path::Path::new(workdir)) {
            tracing::debug!("Could not register imported loop's project at {workdir}: {error}");
        }
        self.activate_loop_trigger(&lp).await;

        Ok(build_json_result(&serde_json::json!({
            "loop_id": lp.id,
            "name": lp.name,
            "nodes_missing_platform": missing_platform,
        })))
    }

    #[tool(
        name = "loop_audit_node_configs",
        description = "Scan every loop node in the database for a config key its kind will never read (e.g. 'prompt' on an agent node, which the engine silently ignores in favor of 'prompt_template'). Write-time validation (loop_add_node/loop_update_node) rejects this going forward; this tool finds nodes that predate it and are still silently degraded."
    )]
    async fn loop_audit_node_configs(&self) -> Result<CallToolResult, McpError> {
        let nodes = self.db.list_all_loop_nodes().map_err(internal_error)?;

        let mut flagged = Vec::new();
        for node in &nodes {
            let Some(map) = node.config.as_object() else {
                continue;
            };
            let unknown = unknown_config_keys(node.kind, map);
            if unknown.is_empty() {
                continue;
            }
            // A spec-scoped node's own row has no `loop_id` (only the spec
            // does) — resolve it so a flagged node can be traced back to the
            // loop that owns it without a second round-trip.
            let loop_id = match &node.loop_id {
                Some(loop_id) => Some(loop_id.clone()),
                None => match &node.spec_id {
                    Some(spec_id) => self
                        .db
                        .get_loop_spec(spec_id)
                        .map_err(internal_error)?
                        .and_then(|spec| spec.loop_id),
                    None => None,
                },
            };
            flagged.push(serde_json::json!({
                "node_id": node.id,
                "name": node.name,
                "kind": node.kind.display_str(),
                "spec_id": node.spec_id,
                "loop_id": loop_id,
                "unknown_keys": unknown,
            }));
        }

        Ok(CallToolResult::success(vec![Content::text(
            serde_json::to_string_pretty(&serde_json::json!({
                "flagged_nodes": flagged,
            }))
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
            .list_loops(
                params.workdir.as_deref(),
                params.include_archived.unwrap_or(false),
            )
            .map_err(internal_error)?;

        let out = build_loop_list_json(&self.db, &loops)?;

        Ok(CallToolResult::success(vec![Content::text(
            serde_json::to_string_pretty(&out).unwrap_or_default(),
        )]))
    }

    #[tool(
        name = "loop_node_runs_list",
        description = "List a loop's node run history — what `loop_get` omits (it only carries a loop's *bound* specs, empty for a queue-driven run). Defaults to the most recent runs first, so the run that failed a `failed` loop is normally the very first result without knowing any node id ahead of time. Narrow to one spec or node with `spec_id`/`node_id`. Output/input are omitted here since they can be large; fetch a specific run's full output with loop_node_run_get using its `id`."
    )]
    async fn loop_node_runs_list(
        &self,
        Parameters(params): Parameters<LoopNodeRunsListParams>,
    ) -> Result<CallToolResult, McpError> {
        let loop_id = match resolve_prefix_or_error(
            &self.db,
            params.loop_id.trim(),
            Database::resolve_loop_id_by_prefix,
            "loop",
        ) {
            Ok(id) => id,
            Err(e) => return Ok(e),
        };
        let resolved_spec_id = match &params.spec_id {
            Some(sid) => {
                match resolve_prefix_or_error(
                    &self.db,
                    sid.trim(),
                    Database::resolve_spec_id_by_prefix,
                    "spec",
                ) {
                    Ok(id) => Some(id),
                    Err(e) => return Ok(e),
                }
            }
            None => None,
        };
        let resolved_node_id = match &params.node_id {
            Some(nid) => {
                match resolve_prefix_or_error(
                    &self.db,
                    nid.trim(),
                    Database::resolve_loop_node_id_by_prefix,
                    "node",
                ) {
                    Ok(id) => Some(id),
                    Err(e) => return Ok(e),
                }
            }
            None => None,
        };

        if self
            .db
            .get_loop(&loop_id)
            .map_err(internal_error)?
            .is_none()
        {
            return Ok(error_result(&format!("Loop '{}' not found.", loop_id)));
        }

        let limit = params.limit.unwrap_or(20).clamp(1, 200) as i64;
        let offset = params.offset.unwrap_or(0) as i64;

        let compact = params.compact.unwrap_or(false);

        let runs = self
            .db
            .list_loop_node_runs(
                &loop_id,
                resolved_spec_id.as_deref(),
                resolved_node_id.as_deref(),
                limit,
                offset,
            )
            .map_err(internal_error)?;

        let total = self
            .db
            .count_loop_node_runs_filtered(
                &loop_id,
                resolved_spec_id.as_deref(),
                resolved_node_id.as_deref(),
            )
            .map_err(internal_error)?;

        let out = runs
            .iter()
            .map(|run| loop_node_run_summary_json(&self.db, run, compact))
            .collect::<Vec<_>>();

        let returned = out.len() as i64;
        let remaining = (total - offset).saturating_sub(returned);
        let truncated = remaining > 0;

        let mut body = serde_json::json!({
            "loop_id": loop_id,
            "limit": limit,
            "offset": offset,
            "runs": out,
            "total": total,
        });
        if truncated {
            body["truncated"] = serde_json::json!(true);
            body["omitted"] = serde_json::json!(remaining);
        }

        Ok(CallToolResult::success(vec![Content::text(
            serde_json::to_string_pretty(&body).unwrap_or_default(),
        )]))
    }

    #[tool(
        name = "loop_node_run_get",
        description = "Fetch one node run's full stored input/output by run id (see loop_node_runs_list). This is the second step of failure diagnosis: list to find the offending run, then fetch its output here — the exact stderr/stdout/reported_output the engine recorded, including `infra_attempt`/`infra_crash` markers when present. Secret-shaped substrings (API keys, tokens, private key blocks) are redacted before the output crosses this boundary."
    )]
    async fn loop_node_run_get(
        &self,
        Parameters(params): Parameters<LoopNodeRunGetParams>,
    ) -> Result<CallToolResult, McpError> {
        let run_id = match resolve_prefix_or_error(
            &self.db,
            params.run_id.trim(),
            Database::resolve_run_id_by_prefix,
            "run",
        ) {
            Ok(id) => id,
            Err(e) => return Ok(e),
        };
        let Some(run) = self.db.get_loop_run(&run_id).map_err(internal_error)? else {
            return Ok(error_result(&format!("Node run '{}' not found.", run_id)));
        };
        let node_name = self
            .db
            .get_loop_node(&run.node_id)
            .map_err(internal_error)?
            .map(|node| node.name);

        Ok(CallToolResult::success(vec![Content::text(
            serde_json::to_string_pretty(&loop_node_run_detail_json(&run, node_name.as_deref()))
                .unwrap_or_default(),
        )]))
    }

    #[tool(
        name = "agent_probe",
        description = "Actually invoke a configured platform headlessly with a trivial prompt and report whether a real, usable response comes back — the verdict is based on the response content, not the exit code, so a harness that prints its own error and exits 0 is reported broken rather than healthy, and a harness that silently answers with a different model than the one requested (its own \"falling back\" warning) is reported substituted rather than reachable. Omit `platform` to probe every platform configured in canopy (each with its own default model); pass `platform` alone to probe its default model, or `platform`+`model` together to validate the exact pair a loop node would use. If the platform's CLI has no way to select a model explicitly, the specific pair can't be validated end to end and is reported unknown, never reachable. On failure, reports the harness's own error text (redacted of secrets) so you learn *why* (missing API key vs. wrong model name vs. it never answered), not just that it failed. Spends real tokens/quota per platform probed — call this explicitly, never automatically or on a schedule."
    )]
    async fn agent_probe(
        &self,
        Parameters(params): Parameters<AgentProbeParams>,
    ) -> Result<CallToolResult, McpError> {
        if params.model.is_some() && params.platform.is_none() {
            return Ok(error_result(
                "`model` requires `platform` — omit both to probe every configured platform's \
                 default model.",
            ));
        }

        let Some(home) = dirs::home_dir() else {
            return Err(internal_error("No home directory"));
        };
        let config = crate::domain::canopy_config::CanopyConfig::load(&home.join(".canopy"));

        let targets: Vec<crate::daemon::probe::ProbeTarget> = match params.platform.as_deref() {
            Some(platform) => vec![crate::daemon::probe::ProbeTarget {
                platform: platform.to_string(),
                model: params.model.clone(),
            }],
            None => config
                .clis
                .iter()
                .map(|cli| crate::daemon::probe::ProbeTarget {
                    platform: cli.name.clone(),
                    model: None,
                })
                .collect(),
        };

        if targets.is_empty() {
            return Ok(error_result(
                "No platforms configured in canopy. Run 'canopy setup'.",
            ));
        }

        let timeout_secs = clamp_probe_timeout(params.timeout_seconds);
        let reports = crate::daemon::probe::probe_targets(
            &config,
            &targets,
            None,
            std::time::Duration::from_secs(timeout_secs),
        )
        .await;

        Ok(CallToolResult::success(vec![Content::text(
            serde_json::to_string_pretty(&serde_json::json!({
                "timeout_seconds": timeout_secs,
                "would_fail": crate::daemon::probe::would_fail_count(&reports),
                "unknown": crate::daemon::probe::unknown_count(&reports),
                "probes": reports.iter().map(|r| r.to_json()).collect::<Vec<_>>(),
            }))
            .unwrap_or_default(),
        )]))
    }

    #[tool(
        name = "loop_preflight",
        description = "Probe every distinct platform+model pair a loop's agent nodes, ensemble members, and on_completed hook reference — before spending a real loop_run on a harness that's installed and configured but can't actually produce a response. A platform used by several nodes is probed once, not once per node; the result names every node/hook that references a failing pair. Verdict is based on response content, not exit code (see agent_probe): a pair the harness silently answers with a substituted model, or one whose CLI has no way to select a model explicitly, is never reported reachable — `would_fail` counts confirmed failures and `unknown` counts pairs that couldn't be validated, kept separate so an unvalidated pair is never mistaken for a passing one. Spends real tokens/quota per distinct pair — call this explicitly before loop_run, never automatically."
    )]
    async fn loop_preflight(
        &self,
        Parameters(params): Parameters<LoopPreflightParams>,
    ) -> Result<CallToolResult, McpError> {
        let details = match self.db.get_loop_details(&params.loop_id) {
            Ok(Some(details)) => details,
            Ok(None) => {
                return Ok(error_result(&format!(
                    "Loop '{}' not found.",
                    params.loop_id
                )))
            }
            Err(e) => return Err(internal_error(e.to_string())),
        };

        // Graph-level structural validation (CB8) — same validate_loop_graph
        // used by loop_import, before spending quota on platform probes.
        {
            let router_labels: Vec<Vec<String>> = details
                .graph_nodes
                .iter()
                .map(|n| {
                    if n.kind == crate::domain::loops::LoopNodeKind::Router {
                        n.config
                            .get("routes")
                            .and_then(|v| v.as_array())
                            .map(|arr| {
                                arr.iter()
                                    .filter_map(|entry| {
                                        entry
                                            .get("label")
                                            .and_then(|v| v.as_str())
                                            .map(|s| s.to_string())
                                    })
                                    .collect()
                            })
                            .unwrap_or_default()
                    } else {
                        Vec::new()
                    }
                })
                .collect();
            let node_views: Vec<crate::domain::validation::GraphNodeView> = details
                .graph_nodes
                .iter()
                .enumerate()
                .map(|(idx, n)| crate::domain::validation::GraphNodeView {
                    id: &n.id,
                    kind: n.kind,
                    route_labels: &router_labels[idx],
                })
                .collect();
            let edge_views: Vec<crate::domain::validation::GraphEdgeView> = details
                .graph_edges
                .iter()
                .map(|e| crate::domain::validation::GraphEdgeView {
                    from: &e.from_node,
                    to: &e.to_node,
                    condition: &e.condition,
                })
                .collect();
            if let Err(e) = crate::domain::validation::validate_loop_graph(&node_views, &edge_views)
            {
                // The validator names nodes by id; a live graph's ids are
                // opaque UUIDs, so enrich with the display name where we can
                // (same treatment loop_import gives its message).
                let mut enriched = e;
                for n in &details.graph_nodes {
                    if enriched.contains(n.id.as_str()) {
                        enriched =
                            enriched.replace(n.id.as_str(), &format!("{} ({})", n.name, n.id));
                    }
                }
                return Ok(error_result(&enriched));
            }
        }

        // CM1: validate {{output:NodeName}} references
        {
            let node_names: Vec<String> =
                details.graph_nodes.iter().map(|n| n.name.clone()).collect();
            for node in &details.graph_nodes {
                if let Some(prompt) = node.config.get("prompt_template").and_then(|v| v.as_str()) {
                    let mut rest = prompt;
                    while let Some(start) = rest.find("{{output:") {
                        let after_prefix = &rest[start + 9..];
                        if let Some(end) = after_prefix.find("}}") {
                            let referenced_name = &after_prefix[..end];
                            if !node_names.iter().any(|n| n == referenced_name) {
                                return Ok(error_result(&format!(
                                    "Node '{}' references unknown node '{}' in {{{{output:{}}}}}.",
                                    node.name, referenced_name, referenced_name
                                )));
                            }
                            rest = &after_prefix[end + 2..];
                        } else {
                            break;
                        }
                    }
                }
            }
        }

        let loop_targets = crate::daemon::probe::distinct_targets_for_loop(&details);
        if loop_targets.is_empty() {
            return Ok(success_result(&format!(
                "Loop '{}' has no agent nodes, ensemble members, or on_completed hook to probe.",
                params.loop_id
            )));
        }

        let Some(home) = dirs::home_dir() else {
            return Err(internal_error("No home directory"));
        };
        let config = crate::domain::canopy_config::CanopyConfig::load(&home.join(".canopy"));

        let timeout_secs = clamp_probe_timeout(params.timeout_seconds);
        let targets: Vec<crate::daemon::probe::ProbeTarget> =
            loop_targets.iter().map(|t| t.target.clone()).collect();
        let reports = crate::daemon::probe::probe_targets(
            &config,
            &targets,
            Some(&details.lp.workdir),
            std::time::Duration::from_secs(timeout_secs),
        )
        .await;

        let probes: Vec<serde_json::Value> = reports
            .iter()
            .zip(loop_targets.iter())
            .map(|(report, loop_target)| {
                let mut value = report.to_json();
                if let serde_json::Value::Object(map) = &mut value {
                    map.insert(
                        "used_by".to_string(),
                        serde_json::json!(loop_target.used_by),
                    );
                }
                value
            })
            .collect();

        Ok(CallToolResult::success(vec![Content::text(
            serde_json::to_string_pretty(&serde_json::json!({
                "loop_id": params.loop_id,
                "timeout_seconds": timeout_secs,
                "pairs_checked": reports.len(),
                "would_fail": crate::daemon::probe::would_fail_count(&reports),
                "unknown": crate::daemon::probe::unknown_count(&reports),
                "probes": probes,
            }))
            .unwrap_or_default(),
        )]))
    }

    #[tool(
        name = "loop_run",
        description = "Run a loop in the background, spec by spec. With `queue_id`, runs the queue's pending specs (in queue order) through the loop's graph instead of the loop's own bound specs. `workdir` overrides the loop's workdir for this run only."
    )]
    async fn loop_run(
        &self,
        Parameters(params): Parameters<LoopRunParams>,
    ) -> Result<CallToolResult, McpError> {
        let loop_id = match resolve_prefix_or_error(
            &self.db,
            params.loop_id.trim(),
            Database::resolve_loop_id_by_prefix,
            "loop",
        ) {
            Ok(id) => id,
            Err(e) => return Ok(e),
        };
        let lp = match self.db.get_loop(&loop_id) {
            Ok(Some(w)) => w,
            Ok(None) => return Ok(error_result(&format!("Loop '{}' not found.", loop_id))),
            Err(e) => return Err(internal_error(e.to_string())),
        };

        if let Err(message) = loop_run_status_guard(&loop_id, lp.status) {
            return Ok(error_result(&message));
        }

        // C19: a loop paused with an active blocker (whether from
        // `loop_report_blocker` or from a spec exceeding its cross-run
        // attempt budget) needs a human, not another relaunch —
        // `loop_run_status_guard` alone still accepts `Paused` (that's the
        // normal resume-after-`loop_pause` path), so this is a second,
        // narrower check on top of it. `loop_reset` always clears the
        // loop's own status back to `draft` regardless of which specs it
        // targeted, so this only ever refuses the exact window FR4 asks
        // for: still-`paused`-and-blocked, not yet reset.
        if lp.status == LoopStatus::Paused {
            let summary = build_loop_summary_json(&self.db, &lp)?;
            if let Some(blocker) = summary.get("blocker").and_then(|v| v.as_str()) {
                return Ok(error_result(&format!(
                    "Loop '{}' is blocked and cannot be relaunched via loop_run: {blocker}. \
                     Resolve it, then loop_reset the affected spec (naming it explicitly \
                     clears its cross-run attempt count) before relaunching.",
                    loop_id
                )));
            }
        }

        if let Some(run) = find_in_flight_run(&self.db, &loop_id).map_err(internal_error)? {
            let node_name = node_name_for_error(&self.db, &run.node_id).map_err(internal_error)?;
            return Ok(error_result(&in_flight_run_error(
                &loop_id,
                &run.id,
                &node_name,
                run.started_at,
            )));
        }

        let queue_id = params
            .queue_id
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty());
        if let Some(queue_id) = queue_id {
            if let Err(e) = validate_queue_exists(&self.db, queue_id) {
                return Ok(error_result(&e));
            }
            if let Err(e) = validate_queue_not_consumed(&self.db, queue_id, &loop_id) {
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

        if let Err(e) = validate_loop_ensembles_for_run(&self.db, &loop_id, queue_id) {
            return Ok(error_result(&e));
        }

        match self.loop_engine.empty_launch_check(&loop_id, queue_id) {
            Ok(Some(message)) => return Ok(error_result(&message)),
            Ok(None) => {}
            Err(e) => return Err(internal_error(e.to_string())),
        }

        Arc::clone(&self.loop_engine).start_background_run(
            loop_id.clone(),
            queue_id.map(str::to_string),
            workdir.map(str::to_string),
        );
        Ok(success_result(&format!(
            "Loop '{}' launched in background.",
            loop_id
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
        description = "Reset a completed/failed loop back to pending so loop_run can relaunch it. Without `specs`, resets every non-completed spec, leaving already-completed ones untouched so loop_run resumes at the first pending spec. With `specs`, resets exactly those spec IDs, even if they were completed. If the loop's last run was against a queue, its queue members are what get reset (same semantics), since a queue run's own bound specs are typically empty. Rejects a `running` loop — call loop_pause first. Note: a `failed` loop with a pending loop_schedule_autorun resets and resumes itself automatically when the schedule fires — call this manually only to reset sooner, reset a `completed` loop, or reset specific spec IDs."
    )]
    async fn loop_reset(
        &self,
        Parameters(params): Parameters<LoopResetParams>,
    ) -> Result<CallToolResult, McpError> {
        let loop_id = match resolve_prefix_or_error(
            &self.db,
            params.loop_id.trim(),
            Database::resolve_loop_id_by_prefix,
            "loop",
        ) {
            Ok(id) => id,
            Err(e) => return Ok(e),
        };
        perform_loop_reset(&self.db, &loop_id, params.specs.as_deref())
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
        description = "Schedule a one-shot resume for a loop at a future ISO 8601 time, or cancel a pending one. When the scheduler reaches that time: a `failed` loop is auto-reset (same transition as loop_reset) and resumed — useful for a loop that failed on a quota to reschedule its own resumption at the exact reset time; a `completed` loop is left alone (the schedule is cleared but the loop is not re-run — use loop_reset + loop_run to re-run a finished loop); any other fireable status launches normally. If the loop's last run was against a queue, the resume targets that same queue (its pending members, in queue order) instead of the loop's own bound specs. The schedule always clears after firing (one-shot). Omit both `at` and `quota_reset_message` to cancel any pending autorun instead of scheduling one — valid regardless of the loop's current status, and a no-op (not an error) if nothing was scheduled. After a quota failure, prefer `quota_reset_message` (the raw CLI text, e.g. \"resets 1pm (America/Bogota)\") over computing `at` yourself — the engine parses the stated local time/timezone and converts it deterministically, avoiding scheduling errors from doing that arithmetic by hand."
    )]
    async fn loop_schedule_autorun(
        &self,
        Parameters(LoopScheduleAutorunParams {
            loop_id: raw_loop_id,
            at,
            quota_reset_message,
        }): Parameters<LoopScheduleAutorunParams>,
    ) -> Result<CallToolResult, McpError> {
        let loop_id = match resolve_prefix_or_error(
            &self.db,
            raw_loop_id.trim(),
            Database::resolve_loop_id_by_prefix,
            "loop",
        ) {
            Ok(id) => id,
            Err(e) => return Ok(e),
        };
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
            loop_id: raw_loop_id,
            at,
            action,
        }): Parameters<LoopScheduleContinueParams>,
    ) -> Result<CallToolResult, McpError> {
        let loop_id = match resolve_prefix_or_error(
            &self.db,
            raw_loop_id.trim(),
            Database::resolve_loop_id_by_prefix,
            "loop",
        ) {
            Ok(id) => id,
            Err(e) => return Ok(e),
        };
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
        let loop_id = match resolve_prefix_or_error(
            &self.db,
            params.loop_id.trim(),
            Database::resolve_loop_id_by_prefix,
            "loop",
        ) {
            Ok(id) => id,
            Err(e) => return Ok(e),
        };
        let paused = self
            .loop_engine
            .request_pause(&loop_id)
            .map_err(internal_error)?;
        if paused {
            Ok(success_result(&format!(
                "Loop '{}' marked to pause.",
                loop_id
            )))
        } else {
            Ok(error_result(&format!(
                "Loop '{}' is not running or does not exist.",
                loop_id
            )))
        }
    }

    #[tool(
        name = "loop_archive",
        description = "Archive a loop: it leaves the main loop_list/sidebar view but its row, specs, and full run history are untouched (never deleted, never moved to another table) and it can be restored with loop_restore at any time. Refuses a `running` loop — pause it first. Permanent deletion is a separate, deliberate act on an already-archived loop, not something F4/this tool does."
    )]
    async fn loop_archive(
        &self,
        Parameters(params): Parameters<LoopArchiveParams>,
    ) -> Result<CallToolResult, McpError> {
        let loop_id = match resolve_prefix_or_error(
            &self.db,
            params.loop_id.trim(),
            Database::resolve_loop_id_by_prefix,
            "loop",
        ) {
            Ok(id) => id,
            Err(e) => return Ok(e),
        };
        match self.db.archive_loop(&loop_id).map_err(internal_error)? {
            ArchiveLoopOutcome::Archived => Ok(success_result(&format!(
                "Loop '{}' archived. Its specs and run history are intact; restore it with loop_restore.",
                loop_id
            ))),
            ArchiveLoopOutcome::AlreadyArchived => Ok(error_result(&format!(
                "Loop '{}' is already archived.",
                loop_id
            ))),
            ArchiveLoopOutcome::Running => Ok(error_result(&format!(
                "Loop '{}' is running — pause it before archiving.",
                loop_id
            ))),
            ArchiveLoopOutcome::NotFound => Ok(error_result(&format!(
                "Loop '{}' not found.",
                loop_id
            ))),
        }
    }

    #[tool(
        name = "loop_restore",
        description = "Restore an archived loop back to the main loop_list/sidebar view. The loop's row, specs, and run history were never touched by archiving, so this is a plain flag flip."
    )]
    async fn loop_restore(
        &self,
        Parameters(params): Parameters<LoopRestoreParams>,
    ) -> Result<CallToolResult, McpError> {
        let loop_id = match resolve_prefix_or_error(
            &self.db,
            params.loop_id.trim(),
            Database::resolve_loop_id_by_prefix,
            "loop",
        ) {
            Ok(id) => id,
            Err(e) => return Ok(e),
        };
        let restored = self.db.restore_loop(&loop_id).map_err(internal_error)?;
        if restored {
            Ok(success_result(&format!(
                "Loop '{}' restored to the main list.",
                loop_id
            )))
        } else {
            Ok(error_result(&format!(
                "Loop '{}' not found or not archived.",
                loop_id
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
        let loop_id = match resolve_prefix_or_error(
            &self.db,
            params.loop_id.trim(),
            Database::resolve_loop_id_by_prefix,
            "loop",
        ) {
            Ok(id) => id,
            Err(e) => return Ok(e),
        };
        let lp = match self.db.get_loop(&loop_id) {
            Ok(Some(w)) => w,
            Ok(None) => return Ok(error_result(&format!("Loop '{}' not found.", loop_id))),
            Err(e) => return Err(internal_error(e.to_string())),
        };
        if lp.status != LoopStatus::Paused {
            return Ok(error_result(&format!("Loop '{}' is not paused.", loop_id)));
        }

        match params.action.trim() {
            "retry_current_node" => {
                handle_retry_current_node(&self.db, &loop_id)?;
            }
            "skip_next_spec" => handle_skip_next_spec(&self.db, &loop_id)?,
            _ => {
                return Ok(error_result(
                    "loop_continue action must be retry_current_node or skip_next_spec.",
                ));
            }
        }

        Arc::clone(&self.loop_engine).resume_background(loop_id.clone());

        Ok(success_result(&format!(
            "Loop '{}' resumed with action '{}'.",
            loop_id, params.action
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

    /// Point a project at a new workdir after its directory was renamed or
    /// moved, keeping its sessions and history instead of orphaning them.
    #[tool(
        name = "project_remap",
        description = "Remap a project whose directory was renamed or moved to its new path, \
        re-keying its sessions/loops/history instead of losing them. MOVE if nothing is \
        registered at the new path, MERGE (folding into it, removing the stale row) if a \
        project already exists there. Refuses a new_path that doesn't exist unless force=true. \
        Set dry_run=true to preview without changing anything."
    )]
    async fn project_remap(
        &self,
        Parameters(params): Parameters<ProjectRemapParams>,
    ) -> Result<CallToolResult, McpError> {
        let new_path = std::path::Path::new(&params.new_path);
        let resolved =
            match crate::db::project::resolve_remap_path(new_path, params.force.unwrap_or(false)) {
                Ok(resolved) => resolved,
                Err(e) => return Ok(error_result(&e.to_string())),
            };

        let outcome = if params.dry_run.unwrap_or(false) {
            self.db.remap_preview(&params.project_hash, &resolved)
        } else {
            self.db.remap_project(&params.project_hash, &resolved)
        };

        match outcome {
            Ok(outcome) => {
                let kind = match outcome.kind {
                    crate::domain::project::RemapKind::Move => "move",
                    crate::domain::project::RemapKind::Merge => "merge",
                };
                let out = serde_json::json!({
                    "kind": kind,
                    "dry_run": params.dry_run.unwrap_or(false),
                    "old_hash": outcome.old_hash,
                    "new_hash": outcome.new_hash,
                    "new_path": outcome.new_path,
                    "rows_moved": outcome.counts.total(),
                    "counts": {
                        "interactive_sessions": outcome.counts.interactive_sessions,
                        "terminal_sessions": outcome.counts.terminal_sessions,
                        "loops": outcome.counts.loops,
                        "loop_specs": outcome.counts.loop_specs,
                        "sync_messages": outcome.counts.sync_messages,
                        "sync_locks": outcome.counts.sync_locks,
                        "last_prompts": outcome.counts.last_prompts,
                        "scheduled_sends": outcome.counts.scheduled_sends,
                        "failed_scheduled_sends": outcome.counts.failed_scheduled_sends,
                        "agents": outcome.counts.agents,
                        "intelligence_nodes": outcome.counts.intelligence_nodes,
                    },
                });
                Ok(CallToolResult::success(vec![Content::text(
                    serde_json::to_string_pretty(&out).unwrap_or_default(),
                )]))
            }
            Err(e) => Ok(error_result(&e.to_string())),
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

        let store = crate::rag::vector_store::VectorStore::new(
            dimensions,
            Some(config.rag_vector_cache_entries),
        )
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

/// Resolve `agent_probe`/`loop_preflight`'s `timeout_seconds` param to an
/// actual bound: the configured default when omitted, clamped to
/// `[MIN_PROBE_TIMEOUT_SECS, MAX_PROBE_TIMEOUT_SECS]` otherwise — a probe is
/// a liveness check, not a capability benchmark, so callers can't ask for an
/// unbounded wait.
fn clamp_probe_timeout(requested: Option<u64>) -> u64 {
    requested
        .unwrap_or(crate::daemon::probe::DEFAULT_PROBE_TIMEOUT_SECS)
        .clamp(
            crate::daemon::probe::MIN_PROBE_TIMEOUT_SECS,
            crate::daemon::probe::MAX_PROBE_TIMEOUT_SECS,
        )
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

fn platform_model_selection_warning(platform: &str) -> Option<String> {
    let home = dirs::home_dir()?;
    let config = crate::domain::canopy_config::CanopyConfig::load(&home.join(".canopy"));
    let cli = config.get_cli(platform)?;
    if cli.model_flag.is_some() {
        return None;
    }
    Some(format!(
        "Platform '{}' does not support model selection (no model_flag configured).\n\
         This platform addresses named agents, not models.\n\
         Omit the `model` field when using this platform — the CLI will use its own default.\n\
         \n\
         The ids listed below are NOT valid input for the `model` field.",
        platform
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
    full: bool,
) -> CallToolResult {
    let platform_owned = platform.to_string();
    let ttl = configured_models().native_ttl();
    let load = tokio::task::spawn_blocking(move || {
        crate::domain::models_db::load_native_models(
            &platform_owned,
            &binary,
            &args,
            force_refresh,
            ttl,
        )
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

    let (listing, truncation) = format_native_models(&catalog.ids, full);
    let mut listing = format!(
        "Models available to platform '{platform}' (enumerated from the CLI — ids are \
         passable verbatim):\n{listing}"
    );
    // FR4: a platform can expose a `models_list_cmd` while still addressing
    // named agents rather than models (no `model_flag`) — the 2026-08-13
    // `mistral` shape. In that case the ids above are not valid `model`
    // input regardless of where they were enumerated from, so the same
    // warning the models.dev path prints must lead here too.
    if let Some(warning) = platform_model_selection_warning(platform) {
        listing = format!("{warning}\n\n{listing}");
    }
    CallToolResult::success(vec![Content::text(model_result_footer(
        &listing,
        source,
        catalog.fetched_at,
        ttl,
        &truncation,
    ))])
}

/// This daemon's `[models]` TTL configuration, read fresh from
/// `~/.canopy/config.toml` on every call (like `rag_max_file_bytes`) so a
/// config edit is picked up on the next `agent_models` call without
/// restarting the daemon. Falls back to the compiled defaults when no home
/// directory or config file can be found.
fn configured_models() -> crate::domain::canopy_config::ModelsConfig {
    dirs::home_dir()
        .map(|home| crate::domain::canopy_config::CanopyConfig::load(&home.join(".canopy")).models)
        .unwrap_or_default()
}

/// The shared provenance/footer block for `agent_models`, used by both the
/// models.dev and native-enumeration paths. States the cache's age and
/// whether a refresh is due, not just its timestamp — and when the source was
/// unreachable, folds how stale the served cache is into that same notice.
fn model_result_footer(
    listing: &str,
    source: crate::domain::models_db::CatalogSource,
    fetched_at: std::time::SystemTime,
    ttl: std::time::Duration,
    truncation: &crate::daemon::handler_formatting::ModelTruncation,
) -> String {
    let age = fetched_at.elapsed().unwrap_or_default();
    let age_str = format_duration_short(age);
    let ttl_str = format_duration_short(ttl);
    let refresh_due = age >= ttl;

    let provenance_note = if source == crate::domain::models_db::CatalogSource::Stale {
        format!(
            " (source unreachable — serving a cache that is already {age_str} old, past \
             the {ttl_str} refresh interval; retry with refresh: true once it's reachable)"
        )
    } else if refresh_due {
        " (refresh due — pass refresh: true to update now)".to_string()
    } else {
        String::new()
    };

    let truncation_notice = truncation
        .notice()
        .map(|n| format!("\n{n}"))
        .unwrap_or_default();
    format!(
        "{listing}\n\n\
         Source: {}{provenance_note} · age: {age_str} (refresh interval {ttl_str}) · \
         fetched_at: {}\n\
         WARNING: This lists the PROVIDER'S CATALOG, not what your account can use.\n\
         Actual availability depends on your API key tier, account type, and region.\n\
         A model listed here may still fail at runtime — use `agent_probe` or `loop_preflight`\n\
         to validate a specific platform+model pair before relying on it.{truncation_notice}",
        source.as_str(),
        format_system_time(fetched_at),
    )
}

/// Format a `SystemTime` as an RFC 3339 / ISO 8601 UTC timestamp for the
/// `agent_models` cache metadata.
fn format_system_time(time: std::time::SystemTime) -> String {
    chrono::DateTime::<chrono::Utc>::from(time).to_rfc3339()
}

/// Render a `Duration` as a short human-readable age/TTL (`45s`, `12m`,
/// `2h15m`, `3d4h`) for the `agent_models` footer — coarse on purpose, since
/// the footer only needs to convey rough freshness, not precise timing.
fn format_duration_short(d: std::time::Duration) -> String {
    let secs = d.as_secs();
    if secs < 60 {
        return format!("{secs}s");
    }
    let mins = secs / 60;
    if mins < 60 {
        return format!("{mins}m");
    }
    let hours = mins / 60;
    let rem_mins = mins % 60;
    if hours < 24 {
        return if rem_mins == 0 {
            format!("{hours}h")
        } else {
            format!("{hours}h{rem_mins}m")
        };
    }
    let days = hours / 24;
    let rem_hours = hours % 24;
    if rem_hours == 0 {
        format!("{days}d")
    } else {
        format!("{days}d{rem_hours}h")
    }
}

/// Find and skip `loop_id`'s currently `running` spec — its own bound spec,
/// or, for a queue-driven run, the queue member currently in flight. A queue
/// member's `loop_id` column stays `None` (queue membership never binds it),
/// so `list_loop_specs(loop_id)` alone can't see it (B18): the loop's
/// persisted `active_run_queue_id` is what names the queue to look in instead.
pub(crate) fn handle_skip_next_spec(db: &Database, loop_id: &str) -> Result<(), McpError> {
    let bound_running = db
        .list_loop_specs(loop_id)
        .map_err(internal_error)?
        .into_iter()
        .find(|spec| spec.status == LoopSpecStatus::Running);

    let current_spec = match bound_running {
        Some(spec) => spec,
        None => queue_running_spec(db, loop_id)?.ok_or_else(|| {
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
    if bound_running.is_none() && queue_running_spec(db, loop_id)?.is_none() {
        return Err(McpError::invalid_params(
            "No running spec found to retry from this paused loop.",
            None,
        ));
    }
    Ok(())
}

/// The `running` member of `loop_id`'s currently active queue run, if any —
/// `None` if the loop isn't drawing from a queue, or no member is `running`.
fn queue_running_spec(db: &Database, loop_id: &str) -> Result<Option<LoopSpec>, McpError> {
    let Some(queue_id) = db
        .get_loop(loop_id)
        .map_err(internal_error)?
        .and_then(|lp| lp.active_run_queue_id)
    else {
        return Ok(None);
    };
    for spec_id in db
        .list_queue_member_spec_ids(&queue_id)
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
        "archived": lp.lp.archived,
        "graph": {
            "nodes": lp.graph_nodes.iter().map(|node| loop_node_json(node, &lp.graph_edges)).collect::<Vec<_>>(),
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
            "prompt_override": member.prompt_override,
            // Which prompt this member actually renders — "override" (its own
            // prompt_override) or "shared" (the ensemble's prompt_template) —
            // so a client can tell the two apart without diffing member config
            // against the ensemble row itself.
            "prompt_source": if member.prompt_override.is_some() { "override" } else { "shared" },
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
        // The HEAD this attempt's own `commit_rights: true` node last left
        // behind (C15), if any — `{{spec_committed_head}}` in a check node.
        // `null` means no node this run trusts to commit has moved HEAD yet.
        "spec_committed_head": spec.spec.spec_committed_head,
        "started_at": spec.spec.started_at.map(|value| value.to_rfc3339()),
        "completed_at": spec.spec.completed_at.map(|value| value.to_rfc3339()),
        "nodes": spec.nodes.iter().map(|node| loop_node_json(node, &spec.edges)).collect::<Vec<_>>(),
        "edges": spec.edges.iter().map(loop_edge_json).collect::<Vec<_>>(),
        "ensembles": ensembles,
        "runs": runs,
    }))
}

fn loop_node_json(node: &LoopNode, edges: &[LoopEdge]) -> serde_json::Value {
    serde_json::json!({
        "id": node.id,
        "spec_id": node.spec_id,
        "loop_id": node.loop_id,
        "name": node.name,
        "kind": node.kind.display_str(),
        "config": node.config,
        "position": node.position,
        "created_at": node.created_at.to_rfc3339(),
        "routes": router_routes_json(node, edges),
        "prompt_source": agent_prompt_source_json(node),
    })
}

/// For an agent node, which branch of `resolve_node_prompt_template`'s
/// precedence it will actually run on — `"explicit"`, `"preset"`, or
/// `"default_fallback"` (see `loop_engine::agent_prompt_source`) — so
/// `loop_get` makes a node quietly running on the bare fallback template
/// (no `prompt_template`, no `prompt_preset`) visible without requiring a
/// run first. `None` for every other node kind, same convention as
/// [`router_routes_json`].
fn agent_prompt_source_json(node: &LoopNode) -> Option<&'static str> {
    if node.kind != LoopNodeKind::Agent {
        return None;
    }
    Some(crate::loop_engine::agent_prompt_source(&node.config))
}

/// For a router node, every declared route alongside the edge (if any) that
/// currently serves it — `loop_get`'s view into "which edge serves each
/// route". `None` for every other node kind (or if the router's `config`
/// somehow fails to parse — `loop_get` should never fail outright over a
/// display concern).
fn router_routes_json(node: &LoopNode, edges: &[LoopEdge]) -> Option<serde_json::Value> {
    if node.kind != LoopNodeKind::Router {
        return None;
    }
    let (routes, fallback) = parse_router_routes(&node.config).ok()?;
    Some(serde_json::json!(routes
        .iter()
        .map(|route| {
            let serving_edge = edges.iter().find(|edge| {
                edge.from_node == node.id
                    && edge.condition.route_label() == Some(route.label.as_str())
            });
            serde_json::json!({
                "label": route.label,
                "description": route.description,
                "fallback": route.label == fallback,
                "edge_id": serving_edge.map(|edge| edge.id.clone()),
                "to_node": serving_edge.map(|edge| edge.to_node.clone()),
            })
        })
        .collect::<Vec<_>>()))
}

fn loop_edge_json(edge: &LoopEdge) -> serde_json::Value {
    serde_json::json!({
        "id": edge.id,
        "spec_id": edge.spec_id,
        "loop_id": edge.loop_id,
        "from_node": edge.from_node,
        "to_node": edge.to_node,
        "condition": edge.condition.as_str(),
        "route": edge.condition.route_label(),
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

/// One row of `loop_node_runs_list` — deliberately excludes `input`/`output`
/// (can be large; a caller wanting them calls `loop_node_run_get` with this
/// row's `id`) but resolves `node_name` so a caller isn't left matching a
/// bare node id back to the graph by hand.
fn loop_node_run_summary_json(
    db: &Database,
    run: &LoopNodeRun,
    compact: bool,
) -> serde_json::Value {
    let node_name = db
        .get_loop_node(&run.node_id)
        .ok()
        .flatten()
        .map(|node| node.name);
    if compact {
        let spec_name = db
            .get_loop_spec(&run.spec_id)
            .ok()
            .flatten()
            .map(|s| s.name);
        serde_json::json!({
            "node_name": node_name,
            "status": run.status.as_str(),
            "iteration": run.iteration,
            "spec_name": spec_name,
            "started_at": run.started_at.to_rfc3339(),
            "completed_at": run.completed_at.map(|value| value.to_rfc3339()),
        })
    } else {
        serde_json::json!({
            "id": run.id,
            "spec_id": run.spec_id,
            "node_id": run.node_id,
            "node_name": node_name,
            "status": run.status.as_str(),
            "iteration": run.iteration,
            "started_at": run.started_at.to_rfc3339(),
            "completed_at": run.completed_at.map(|value| value.to_rfc3339()),
            "session_id": run.session_id,
        })
    }
}

/// The `loop_node_run_get` response: everything `loop_node_run_summary_json`
/// carries, plus the full `input`/`output` the engine stored — redacted
/// (see [`redact_sensitive_value`]) since either can carry secret-shaped
/// content echoed by the wrapped CLI. `output` preserves whatever the engine
/// wrote verbatim otherwise, including the B19 `infra_attempt`/`infra_crash`
/// markers a caller needs to tell an infra retry from a semantic failure.
fn loop_node_run_detail_json(run: &LoopNodeRun, node_name: Option<&str>) -> serde_json::Value {
    serde_json::json!({
        "id": run.id,
        "loop_id": run.loop_id,
        "spec_id": run.spec_id,
        "node_id": run.node_id,
        "node_name": node_name,
        "status": run.status.as_str(),
        "iteration": run.iteration,
        "input": run.input.as_ref().map(redact_sensitive_value),
        "output": run.output.as_ref().map(redact_sensitive_value),
        "started_at": run.started_at.to_rfc3339(),
        "completed_at": run.completed_at.map(|value| value.to_rfc3339()),
        "session_id": run.session_id,
    })
}

/// Secret-shaped substrings a node run's stored input/output can carry
/// through whatever CLI it wrapped (an API key echoed into a shell command,
/// a leaked token in stderr, a pasted private key). Compiled once and
/// applied uniformly to every string leaf in the run's JSON — see
/// [`redact_sensitive_value`] — rather than to specific fields like `stdout`
/// or `command`, since sensitive content can land in any of them alike and a
/// per-field allowlist would miss the next field name that carries it.
static SECRET_PATTERNS: std::sync::LazyLock<Vec<regex::Regex>> = std::sync::LazyLock::new(|| {
    [
        // Anthropic/OpenAI-style API keys: sk-..., sk-ant-...
        r"sk-[A-Za-z0-9_-]{16,}",
        // GitHub personal/app/oauth/refresh tokens.
        r"gh[pousr]_[A-Za-z0-9]{20,}",
        // AWS access key IDs.
        r"AKIA[0-9A-Z]{16}",
        // Bearer tokens in an Authorization header or similar.
        r"(?i)bearer\s+[A-Za-z0-9._-]{16,}",
        // key/value assignments: api_key=..., "password": "...", token: ...
        r#"(?i)(api[_-]?key|secret|password|access[_-]?token|token)["']?\s*[:=]\s*["']?[A-Za-z0-9._\-/+]{8,}"#,
        // PEM private key blocks.
        r"-----BEGIN [A-Z ]*PRIVATE KEY-----[\s\S]*?-----END [A-Z ]*PRIVATE KEY-----",
    ]
    .iter()
    .map(|pattern| regex::Regex::new(pattern).expect("valid redaction regex"))
    .collect()
});

pub(crate) fn redact_secrets(text: &str) -> String {
    let mut result = text.to_string();
    for pattern in SECRET_PATTERNS.iter() {
        result = pattern.replace_all(&result, "[REDACTED]").into_owned();
    }
    result
}

/// Recursively redacts every string leaf of a node run's stored `input`/
/// `output` before it crosses the MCP boundary via `loop_node_run_get`.
fn redact_sensitive_value(value: &serde_json::Value) -> serde_json::Value {
    match value {
        serde_json::Value::String(s) => serde_json::Value::String(redact_secrets(s)),
        serde_json::Value::Array(items) => {
            serde_json::Value::Array(items.iter().map(redact_sensitive_value).collect())
        }
        serde_json::Value::Object(map) => serde_json::Value::Object(
            map.iter()
                .map(|(k, v)| (k.clone(), redact_sensitive_value(v)))
                .collect(),
        ),
        other => other.clone(),
    }
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
        build_node_update_response, build_spec_update_response, effective_member_prompt,
        find_in_flight_run, handle_retry_current_node, handle_skip_next_spec, header_str,
        in_flight_run_error, json_value_kind_name, loop_details_json, loop_run_status_guard,
        loop_trigger_json, member_node_config, missing_sync_identity_error, node_copy_note,
        node_name_for_error, perform_loop_reset, plan_ensemble_copy, plan_node_copy,
        rag_result_json, resolve_graph_target, resolve_node_kind_and_config, resolve_reported_run,
        spec_summary_json, unknown_config_keys, validate_absolute_dir, validate_at_least_one_bool,
        validate_blueprint_exists, validate_edge_condition, validate_edge_condition_with_route,
        validate_ensemble_members, validate_node_config, validate_node_kind,
        validate_node_not_ensemble_owned, validate_non_empty, validate_not_join_kind,
        validate_queue_exists, validate_queue_member_removable, validate_queue_not_consumed,
        validate_queue_reorder, validate_queue_reorder_locking, validate_route_edge_target,
        validate_spec_deletable, validate_spec_exists, validate_spec_set_status_target,
        validate_spec_status, validate_spec_workdir, BuiltEnsembleUnit, EnsembleMemberParams,
        EnsembleUnitSpec, TaskTriggerHandler, MISSING_SYNC_IDENTITY_MESSAGE,
    };
    use crate::daemon::params::{
        LoopCompletionHookParams, LoopCopyEnsembleParams, LoopCopyNodeParams,
        LoopScheduleAutorunParams, LoopScheduleContinueParams, LoopTriggerParams,
    };
    use crate::db::Database;
    use crate::domain::blueprints::Blueprint;
    use crate::domain::loops::{
        Ensemble, EnsembleMember, Loop, LoopEdge, LoopEdgeCondition, LoopNode, LoopNodeKind,
        LoopNodeRun, LoopRunStatus, LoopSpec, LoopSpecStatus, LoopStatus,
    };
    use crate::domain::models::Trigger;
    use crate::domain::queues::Queue;
    use crate::shared::sync_identity::CANOPY_AGENT_ID_HEADER;
    use tempfile::tempdir;

    fn ensemble_member_params(platform: &str) -> EnsembleMemberParams {
        EnsembleMemberParams {
            platform: platform.to_string(),
            model: None,
            prompt_override: None,
        }
    }

    fn ensemble_member_params_with_prompt(
        platform: &str,
        prompt_override: &str,
    ) -> EnsembleMemberParams {
        EnsembleMemberParams {
            platform: platform.to_string(),
            model: None,
            prompt_override: Some(prompt_override.to_string()),
        }
    }

    /// The `#[tool]` macro locates a handler's parameter wrapper by the
    /// literal ident `Parameters`, then generates
    /// `schema_for_type::<Parameters<T>>()` for the advertised MCP
    /// `input_schema`. Swapping in this crate's own `Parameters<T>` (for
    /// enriched deserialization-failure messages) must not silently fall
    /// back to an empty schema — clients still need the real shape to build
    /// correct calls in the first place.
    #[test]
    fn tool_router_still_advertises_real_input_schemas_for_swapped_parameters_type() {
        let tools = TaskTriggerHandler::tool_router().list_all();

        let create_seed = tools
            .iter()
            .find(|tool| tool.name == "create_seed")
            .expect("create_seed tool should be registered");
        let props = create_seed
            .input_schema
            .get("properties")
            .and_then(|p| p.as_object())
            .expect("create_seed input_schema should have properties");
        assert!(props.contains_key("name"));
        assert!(props.contains_key("directives"));

        let intelligence_upsert = tools
            .iter()
            .find(|tool| tool.name == "intelligence_upsert")
            .expect("intelligence_upsert tool should be registered");
        let props = intelligence_upsert
            .input_schema
            .get("properties")
            .and_then(|p| p.as_object())
            .expect("intelligence_upsert input_schema should have properties");
        assert!(props.contains_key("node_data"));
    }

    /// Regression guard for the class of bug where a nested-object parameter
    /// is advertised as a bare `$ref` into `$defs` with no sibling `type` at
    /// the property level. A client that builds tool arguments from a
    /// shallow read of the property schema (never resolving `$ref`) sees no
    /// declared type there and falls back to sending the value as a string.
    /// This has already happened twice — `loop_add_node.config` and
    /// `intelligence_upsert.node_data` — so this test walks every
    /// registered tool's whole surface instead of asserting on one tool.
    #[test]
    fn every_tool_property_self_declares_its_type() {
        let tools = TaskTriggerHandler::tool_router().list_all();
        let mut violations = Vec::new();

        for tool in &tools {
            let Some(properties) = tool
                .input_schema
                .get("properties")
                .and_then(|p| p.as_object())
            else {
                continue;
            };

            for (prop_name, prop_schema) in properties {
                // A `$ref` with a sibling `type` counts via `has_type`;
                // a bare `$ref` with no sibling `type` does not, which is
                // exactly the shape this guard rejects.
                let has_type = prop_schema.get("type").is_some();
                let has_combinator =
                    prop_schema.get("anyOf").is_some() || prop_schema.get("oneOf").is_some();
                if !has_type && !has_combinator {
                    violations.push(format!("{}.{prop_name}", tool.name));
                }
            }
        }

        assert!(
            violations.is_empty(),
            "properties with no declared type at the property level — a client that doesn't \
             resolve $ref can't tell these apart from an untyped string parameter: {violations:?}"
        );
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

    /// `prompt_override` is normalized like `model`: trimmed, and a
    /// whitespace-only override collapses to `None` (use the shared
    /// prompt), not an override of empty string.
    #[test]
    fn validate_ensemble_members_trims_prompt_override_and_blanks_to_none() {
        let members = vec![
            ensemble_member_params_with_prompt("claude", "  review for security  "),
            {
                let mut m = ensemble_member_params("codex");
                m.prompt_override = Some("   ".to_string());
                m
            },
        ];
        let result = validate_ensemble_members(&members).unwrap();
        assert_eq!(result[0].2.as_deref(), Some("review for security"));
        assert_eq!(result[1].2, None);
    }

    #[test]
    fn effective_member_prompt_prefers_override_over_shared() {
        assert_eq!(
            effective_member_prompt(Some("angle-specific prompt"), "shared prompt"),
            "angle-specific prompt"
        );
        assert_eq!(
            effective_member_prompt(None, "shared prompt"),
            "shared prompt"
        );
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
            spec_committed_head: None,
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
            spec_committed_head: None,
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
            archived: false,
            paused_by_reconciliation: false,
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
            active_run_queue_id: None,
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

    /// `Interrupted` is treated like any other non-completed status: a plain
    /// `loop_reset` (no explicit `specs`) returns it to `pending`, same as it
    /// would `failed` — an interruption isn't the special case here, only
    /// `completed` is (preserved unless named explicitly).
    #[test]
    fn loop_reset_resets_interrupted_spec_to_pending() {
        let (_dir, db, loop_id) = loop_reset_fixture(LoopStatus::Failed);
        db.insert_loop_spec(&spec_with_status(
            &loop_id,
            "spec-interrupted",
            1,
            LoopSpecStatus::Interrupted,
        ))
        .unwrap();

        let result = perform_loop_reset(&db, &loop_id, None).unwrap();
        assert!(!result.is_error.unwrap_or(false));

        let specs = db.list_loop_specs(&loop_id).unwrap();
        let interrupted = specs.iter().find(|s| s.id == "spec-interrupted").unwrap();
        assert_eq!(interrupted.status, LoopSpecStatus::Pending);
        assert!(interrupted.started_at.is_none());
        assert!(interrupted.completed_at.is_none());
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

    /// A loop whose last run was against a queue has empty (or irrelevant)
    /// bound specs — the queue's *members* are what actually need resetting.
    /// `loop_reset` must find them via the loop's persisted
    /// `active_run_queue_id`, reset every non-completed one back to pending
    /// (completed members untouched), and report the real count — not "0
    /// spec(s) reset", the false report from the incident this spec fixes.
    #[test]
    fn loop_reset_queue_run_resets_pending_queue_members_and_reports_count() {
        let dir = tempdir().unwrap();
        let db = Database::new(&dir.path().join("test.db")).unwrap();
        let loop_id = "loop-queue-reset-test".to_string();
        db.insert_loop(&Loop {
            archived: false,
            paused_by_reconciliation: false,
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
            active_run_queue_id: Some("queue-1".to_string()),
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
            spec_committed_head: None,
            workdir: None,
            completed_via: None,
            completed_via_reason: None,
            completed_via_at: None,
        };
        db.insert_loop_spec(&standalone("queue-done", 1, LoopSpecStatus::Completed))
            .unwrap();
        db.insert_loop_spec(&standalone("queue-failed", 2, LoopSpecStatus::Failed))
            .unwrap();
        db.insert_loop_spec(&standalone("queue-pending", 3, LoopSpecStatus::Pending))
            .unwrap();
        db.insert_queue(&Queue {
            id: "queue-1".to_string(),
            name: "queue-1".to_string(),
            created_at: chrono::Utc::now(),
        })
        .unwrap();
        for spec_id in ["queue-done", "queue-failed", "queue-pending"] {
            db.append_queue_member("queue-1", spec_id, None).unwrap();
        }

        let result = perform_loop_reset(&db, &loop_id, None).unwrap();
        assert!(!result.is_error.unwrap_or(false));
        let text = format!("{:?}", result.content);
        assert!(text.contains("2 spec(s) reset"), "{text}");

        let lp = db.get_loop(&loop_id).unwrap().unwrap();
        assert_eq!(lp.status, LoopStatus::Draft);

        let done = db.get_loop_spec("queue-done").unwrap().unwrap();
        let failed = db.get_loop_spec("queue-failed").unwrap().unwrap();
        let pending = db.get_loop_spec("queue-pending").unwrap().unwrap();
        assert_eq!(
            done.status,
            LoopSpecStatus::Completed,
            "completed queue member must be left untouched"
        );
        assert_eq!(failed.status, LoopSpecStatus::Pending);
        assert!(failed.completed_at.is_none());
        assert_eq!(pending.status, LoopSpecStatus::Pending);
    }

    /// A `Running`-status loop with no actual in-flight run is now
    /// resettable — the old status-only guard would have refused this
    /// (status alone said "running"), but the ground truth is the
    /// `loop_runs` table, and here it has nothing running under this loop.
    /// This is the intended flip side of
    /// `loop_reset_rejects_loop_with_in_flight_run_even_when_status_is_paused`:
    /// status and a run's lifetime are independent in both directions.
    #[test]
    fn loop_reset_allows_running_status_loop_with_no_in_flight_run() {
        let (_dir, db, loop_id) = loop_reset_fixture(LoopStatus::Running);

        let result = perform_loop_reset(&db, &loop_id, None).unwrap();
        assert!(!result.is_error.unwrap_or(false));

        let lp = db.get_loop(&loop_id).unwrap().unwrap();
        assert_eq!(lp.status, LoopStatus::Draft);
    }

    /// The ground truth is the `loop_runs` table, not `lp.status` — a loop
    /// left `paused` by a sibling node's blocker report while a *different*
    /// node keeps executing must still refuse the reset, naming the node and
    /// run still in flight so the caller can wait or kill it deliberately
    /// (the 2026-08-05 incident this guards against: a reset silently killed
    /// and reset the still-running node's spec, and its late completion
    /// routed an edge and failed the loop out from under the fresh dispatch
    /// the reset then launched).
    #[test]
    fn loop_reset_rejects_loop_with_in_flight_run_even_when_status_is_paused() {
        let (_dir, db, loop_id) = loop_reset_fixture(LoopStatus::Paused);
        db.insert_loop_spec(&spec_with_status(
            &loop_id,
            "spec-a",
            1,
            LoopSpecStatus::Running,
        ))
        .unwrap();
        db.insert_loop_node(&LoopNode {
            id: "node-resilience".to_string(),
            spec_id: Some("spec-a".to_string()),
            loop_id: None,
            name: "resilience".to_string(),
            kind: LoopNodeKind::Check,
            config: serde_json::json!({"command": "sleep 30"}),
            position: 1,
            created_at: chrono::Utc::now(),
        })
        .unwrap();
        db.insert_loop_run(&LoopNodeRun {
            id: "run-live".to_string(),
            loop_id: loop_id.clone(),
            spec_id: "spec-a".to_string(),
            node_id: "node-resilience".to_string(),
            status: LoopRunStatus::Running,
            input: None,
            output: None,
            started_at: chrono::Utc::now(),
            completed_at: None,
            iteration: 1,
            pid: None,
            boot_id: None,
            session_id: None,
        })
        .unwrap();

        let result = perform_loop_reset(&db, &loop_id, None).unwrap();
        assert!(result.is_error.unwrap_or(false));
        let text = format!("{:?}", result.content);
        assert!(text.contains("resilience"), "{text}");
        assert!(text.contains("run-live"), "{text}");

        // Nothing touched: status, spec, and the run row all untouched.
        let lp = db.get_loop(&loop_id).unwrap().unwrap();
        assert_eq!(lp.status, LoopStatus::Paused);
        let spec = db.get_loop_spec("spec-a").unwrap().unwrap();
        assert_eq!(spec.status, LoopSpecStatus::Running);
        let run = db.get_loop_run("run-live").unwrap().unwrap();
        assert_eq!(run.status, LoopRunStatus::Running);
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

    // ── find_in_flight_run: loop_run's dispatch guard ────────────────────

    #[test]
    fn find_in_flight_run_none_when_nothing_running() {
        let (_dir, db, loop_id) = loop_reset_fixture(LoopStatus::Paused);
        assert!(find_in_flight_run(&db, &loop_id).unwrap().is_none());
    }

    /// `loop_run` must refuse a duplicate dispatch while a node executes,
    /// regardless of the loop's own status — a `paused` loop can still have
    /// a live run when a sibling node's blocker report flipped status
    /// without terminating it (see `loop_reset_rejects_loop_with_in_flight_run_even_when_status_is_paused`
    /// for the same ground truth on the reset side).
    #[test]
    fn find_in_flight_run_finds_running_row_regardless_of_loop_status() {
        let (_dir, db, loop_id) = loop_reset_fixture(LoopStatus::Paused);
        db.insert_loop_spec(&spec_with_status(
            &loop_id,
            "spec-a",
            1,
            LoopSpecStatus::Running,
        ))
        .unwrap();
        db.insert_loop_node(&LoopNode {
            id: "node-resilience".to_string(),
            spec_id: Some("spec-a".to_string()),
            loop_id: None,
            name: "resilience".to_string(),
            kind: LoopNodeKind::Check,
            config: serde_json::json!({"command": "sleep 30"}),
            position: 1,
            created_at: chrono::Utc::now(),
        })
        .unwrap();
        db.insert_loop_run(&LoopNodeRun {
            id: "run-live".to_string(),
            loop_id: loop_id.clone(),
            spec_id: "spec-a".to_string(),
            node_id: "node-resilience".to_string(),
            status: LoopRunStatus::Running,
            input: None,
            output: None,
            started_at: chrono::Utc::now(),
            completed_at: None,
            iteration: 1,
            pid: None,
            boot_id: None,
            session_id: None,
        })
        .unwrap();

        let run = find_in_flight_run(&db, &loop_id)
            .unwrap()
            .expect("the still-running row must be found even though status is paused");
        assert_eq!(run.id, "run-live");

        let node_name = node_name_for_error(&db, &run.node_id).unwrap();
        let message = in_flight_run_error(&loop_id, &run.id, &node_name, run.started_at);
        assert!(message.contains("resilience"), "{message}");
        assert!(message.contains("run-live"), "{message}");
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

    fn router_config(routes: &serde_json::Value, fallback: &str) -> serde_json::Value {
        serde_json::json!({ "routes": routes, "fallback": fallback })
    }

    fn two_routes_json() -> serde_json::Value {
        serde_json::json!([
            { "label": "retry", "description": "Retry the current step." },
            { "label": "escalate", "description": "Hand off to a human." },
        ])
    }

    #[test]
    fn validate_node_config_router_accepts_valid_shape() {
        let config = router_config(&two_routes_json(), "retry");
        assert!(validate_node_config(LoopNodeKind::Router, &config).is_ok());
    }

    #[test]
    fn validate_node_config_router_rejects_fewer_than_two_routes() {
        let config = router_config(
            &serde_json::json!([{ "label": "retry", "description": "Retry." }]),
            "retry",
        );
        let error = validate_node_config(LoopNodeKind::Router, &config).unwrap_err();
        assert!(error.contains("at least 2 routes"), "{error}");
    }

    #[test]
    fn validate_node_config_router_rejects_no_fallback() {
        let config = serde_json::json!({ "routes": two_routes_json() });
        let error = validate_node_config(LoopNodeKind::Router, &config).unwrap_err();
        assert!(error.contains("fallback"), "{error}");
    }

    #[test]
    fn validate_node_config_router_rejects_missing_routes_field() {
        let error = validate_node_config(LoopNodeKind::Router, &serde_json::json!({})).unwrap_err();
        assert!(error.contains("'routes'"), "{error}");
    }

    /// The exact incident shape (loop `824de730-7fec-4031-800a-7933d2cf94c1`,
    /// node `d0458fe0-24b8-4a29-8a69-7bb0e742e046`): an agent node config
    /// with `prompt` instead of `prompt_template` must fail loudly, naming
    /// `prompt_template` as the field that's actually read — not be accepted
    /// and silently run on the bare fallback template.
    #[test]
    fn validate_node_config_agent_rejects_prompt_key_naming_prompt_template() {
        let config = serde_json::json!({ "platform": "claude", "prompt": "do the thing" });
        let error = validate_node_config(LoopNodeKind::Agent, &config).unwrap_err();
        assert!(error.contains("'prompt'"), "{error}");
        assert!(
            error.contains("prompt_template"),
            "error must name the correct key: {error}"
        );
    }

    #[test]
    fn validate_node_config_rejects_unknown_key_for_every_kind() {
        let agent_error = validate_node_config(
            LoopNodeKind::Agent,
            &serde_json::json!({ "platform": "claude", "unexpected_field": true }),
        )
        .unwrap_err();
        assert!(agent_error.contains("unexpected_field"), "{agent_error}");
        assert!(agent_error.contains("'agent'"), "{agent_error}");

        let check_error = validate_node_config(
            LoopNodeKind::Check,
            &serde_json::json!({ "command": "true", "unexpected_field": true }),
        )
        .unwrap_err();
        assert!(check_error.contains("unexpected_field"), "{check_error}");

        let gate_error = validate_node_config(
            LoopNodeKind::Gate,
            &serde_json::json!({ "evaluate": "output_contains", "value": "ok", "unexpected_field": true }),
        )
        .unwrap_err();
        assert!(gate_error.contains("unexpected_field"), "{gate_error}");

        let router_error = validate_node_config(
            LoopNodeKind::Router,
            &serde_json::json!({ "routes": two_routes_json(), "fallback": "retry", "unexpected_field": true }),
        )
        .unwrap_err();
        assert!(router_error.contains("unexpected_field"), "{router_error}");
    }

    /// `commit_rights` (B37) is engine-checked on every node kind
    /// uniformly, so it must be accepted everywhere, not just on agent
    /// nodes.
    #[test]
    fn validate_node_config_accepts_commit_rights_on_every_kind() {
        assert!(validate_node_config(
            LoopNodeKind::Agent,
            &serde_json::json!({ "platform": "claude", "commit_rights": true })
        )
        .is_ok());
        assert!(validate_node_config(
            LoopNodeKind::Check,
            &serde_json::json!({ "command": "true", "commit_rights": true })
        )
        .is_ok());
    }

    /// `require_report` is an agent-only key (unlike `commit_rights`, which
    /// is engine-checked on every kind) — accepted on `agent`, rejected as
    /// unrecognized everywhere else, since only an agent node's process exit
    /// can be judged against a self-report.
    #[test]
    fn validate_node_config_accepts_require_report_on_agent_only() {
        assert!(validate_node_config(
            LoopNodeKind::Agent,
            &serde_json::json!({ "platform": "claude", "require_report": true })
        )
        .is_ok());

        let check_error = validate_node_config(
            LoopNodeKind::Check,
            &serde_json::json!({ "command": "true", "require_report": true }),
        )
        .unwrap_err();
        assert!(check_error.contains("require_report"), "{check_error}");
    }

    /// Neither `prompt_template` nor `prompt_preset` is still a VALID agent
    /// config (the bare fallback template is deliberately kept reachable —
    /// see the spec's "keep the fallback" constraint) — this only rejects
    /// keys the engine never reads, not this legitimate default-reliant
    /// shape. Visibility into "this node runs on the default" is a separate
    /// concern, handled by `agent_prompt_source`/`loop_node_json`'s
    /// `prompt_source` field, not by rejecting the config outright.
    #[test]
    fn validate_node_config_agent_without_prompt_template_or_preset_is_still_valid() {
        assert!(validate_node_config(
            LoopNodeKind::Agent,
            &serde_json::json!({ "platform": "claude" })
        )
        .is_ok());
    }

    #[test]
    fn unknown_config_keys_is_empty_for_join_regardless_of_content() {
        let config = serde_json::json!({ "anything": "goes", "ensemble_id": "e1" });
        let map = config.as_object().unwrap();
        assert!(unknown_config_keys(LoopNodeKind::Join, map).is_empty());
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
            "implementer",
            "cargo-gates",
            "reviewer-committer",
            "commit-check",
            "resilience",
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
            .get_blueprint_by_name("implementer")
            .unwrap()
            .expect("builtin should exist");
        let error = super::validate_blueprint_deletable(&builtin).unwrap_err();
        assert!(error.contains("implementer"));
        assert!(error.contains("cannot be deleted"));
    }

    #[test]
    fn loop_add_node_from_blueprint_with_override_merges_config_and_override_wins() {
        let dir = tempdir().unwrap();
        let db = Database::new(&dir.path().join("test.db")).unwrap();

        // The builtin carries no platform (see `builtin_blueprint_specs`),
        // so the caller must supply one via `config_overrides` — this also
        // exercises the required-override path with a successful call.
        let mut overrides = serde_json::Map::new();
        overrides.insert("platform".to_string(), serde_json::json!("claude"));
        overrides.insert("model".to_string(), serde_json::json!("opus"));

        let (kind, config) =
            resolve_node_kind_and_config(&db, None, None, Some("implementer"), Some(overrides))
                .expect("blueprint resolution should succeed");

        assert_eq!(kind, LoopNodeKind::Agent);
        assert_eq!(config["platform"], "claude");
        assert_eq!(config["model"], "opus");
        // The templated key (prompt_preset) survives the shallow merge.
        assert_eq!(config["prompt_preset"], "implementer");
    }

    /// The caller-must-supply-a-harness contract, from the other side: an
    /// agent blueprint's config omits `platform`/`cli` by design (see
    /// `builtin_blueprint_specs`), so creating a node from one without
    /// `config_overrides` supplying it must fail — never silently fall back
    /// to a default harness — with a message that names the missing field
    /// and says the omission is intentional.
    #[test]
    fn loop_add_node_from_agent_blueprint_without_harness_override_fails_with_useful_message() {
        let dir = tempdir().unwrap();
        let db = Database::new(&dir.path().join("test.db")).unwrap();

        let error =
            resolve_node_kind_and_config(&db, None, None, Some("implementer"), None).unwrap_err();

        assert!(error.contains("implementer"), "{error}");
        assert!(error.contains("platform"), "{error}");
        assert!(
            error.contains("intentionally"),
            "error should explain the blueprint intentionally omits a harness: {error}"
        );
    }

    /// A custom blueprint that pins its own platform (the pre-existing,
    /// still-supported pattern) needs no override at all — only the
    /// harness-free builtins require one.
    #[test]
    fn loop_add_node_from_custom_blueprint_with_platform_needs_no_override() {
        let dir = tempdir().unwrap();
        let db = Database::new(&dir.path().join("test.db")).unwrap();
        db.insert_blueprint(&Blueprint {
            id: uuid::Uuid::new_v4().to_string(),
            name: "my-pinned-implementer".to_string(),
            kind: LoopNodeKind::Agent,
            config: serde_json::json!({"platform": "claude", "prompt": "go implement it"}),
            builtin: false,
            created_at: chrono::Utc::now(),
        })
        .unwrap();

        let (kind, config) =
            resolve_node_kind_and_config(&db, None, None, Some("my-pinned-implementer"), None)
                .expect("a custom blueprint with its own platform should not require an override");

        assert_eq!(kind, LoopNodeKind::Agent);
        assert_eq!(config["platform"], "claude");
    }

    #[test]
    fn loop_add_node_with_unknown_blueprint_lists_available_names() {
        let dir = tempdir().unwrap();
        let db = Database::new(&dir.path().join("test.db")).unwrap();

        let error = validate_blueprint_exists(&db, "does-not-exist").unwrap_err();

        assert!(error.contains("does-not-exist"));
        assert!(error.contains("implementer"));
        assert!(error.contains("cargo-gates"));
        assert!(error.contains("reviewer-committer"));
        assert!(error.contains("commit-check"));
        assert!(error.contains("resilience"));
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
            spec_committed_head: None,
            workdir: None,
            completed_via: None,
            completed_via_reason: None,
            completed_via_at: None,
        }
    }

    fn queue_test_db() -> (tempfile::TempDir, Database) {
        let dir = tempdir().unwrap();
        let db = Database::new(&dir.path().join("test.db")).unwrap();
        (dir, db)
    }

    fn insert_queue(db: &Database, id: &str) {
        db.insert_queue(&Queue {
            id: id.to_string(),
            name: format!("{id}-name"),
            created_at: chrono::Utc::now(),
        })
        .unwrap();
    }

    /// Build a fully-wired `TaskTriggerHandler` over an in-memory-ish temp DB
    /// so the queue `#[tool]` methods (and their shared `do_queue_*` helpers)
    /// can be exercised end-to-end.
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

    #[test]
    fn queue_crud_and_ordering_round_trips() {
        let (_dir, db) = queue_test_db();
        for id in ["spec-a", "spec-b", "spec-c"] {
            db.insert_loop_spec(&standalone_spec(id)).unwrap();
        }
        insert_queue(&db, "queue-1");

        db.append_queue_member("queue-1", "spec-a", None).unwrap();
        db.append_queue_member("queue-1", "spec-b", None).unwrap();
        db.append_queue_member("queue-1", "spec-c", None).unwrap();

        assert_eq!(
            db.list_queue_member_spec_ids("queue-1").unwrap(),
            vec!["spec-a", "spec-b", "spec-c"]
        );

        let details = db.get_queue_details("queue-1").unwrap().unwrap();
        assert_eq!(details.queue.id, "queue-1");
        assert_eq!(
            details
                .members
                .iter()
                .map(|spec| spec.id.clone())
                .collect::<Vec<_>>(),
            vec!["spec-a", "spec-b", "spec-c"]
        );

        assert!(db.remove_queue_member("queue-1", "spec-b").unwrap());
        assert_eq!(
            db.list_queue_member_spec_ids("queue-1").unwrap(),
            vec!["spec-a", "spec-c"]
        );
        assert!(!db.remove_queue_member("queue-1", "spec-b").unwrap());

        assert!(db.list_queues().unwrap().iter().any(|p| p.id == "queue-1"));
    }

    #[test]
    fn queue_reorder_is_total_and_deterministic() {
        let (_dir, db) = queue_test_db();
        for id in ["spec-a", "spec-b", "spec-c"] {
            db.insert_loop_spec(&standalone_spec(id)).unwrap();
        }
        insert_queue(&db, "queue-1");
        for id in ["spec-a", "spec-b", "spec-c"] {
            db.append_queue_member("queue-1", id, None).unwrap();
        }

        let current = db.list_queue_member_spec_ids("queue-1").unwrap();
        let order = vec![
            "spec-c".to_string(),
            "spec-a".to_string(),
            "spec-b".to_string(),
        ];
        assert!(validate_queue_reorder(&current, &order).is_ok());

        db.reorder_queue_members("queue-1", &order).unwrap();
        assert_eq!(db.list_queue_member_spec_ids("queue-1").unwrap(), order);
    }

    #[test]
    fn queue_reorder_rejects_partial_list() {
        let current = vec![
            "spec-a".to_string(),
            "spec-b".to_string(),
            "spec-c".to_string(),
        ];
        let order = vec!["spec-a".to_string(), "spec-b".to_string()];

        let error = validate_queue_reorder(&current, &order).unwrap_err();
        assert!(error.contains("exactly once; got 2"), "{error}");
    }

    #[test]
    fn queue_reorder_rejects_unknown_spec() {
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

        let error = validate_queue_reorder(&current, &order).unwrap_err();
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
            archived: false,
            paused_by_reconciliation: false,
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
            active_run_queue_id: None,
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
    fn queue_not_consumed_allows_start_when_every_member_is_pending() {
        let (_dir, db) = queue_test_db();
        db.insert_loop_spec(&standalone_spec("spec-a")).unwrap();
        db.insert_loop_spec(&standalone_spec("spec-b")).unwrap();
        insert_queue(&db, "queue-1");
        db.append_queue_member("queue-1", "spec-a", None).unwrap();
        db.append_queue_member("queue-1", "spec-b", None).unwrap();

        assert!(validate_queue_not_consumed(&db, "queue-1", "loop-requesting").is_ok());
    }

    #[test]
    fn queue_not_consumed_blocks_when_a_member_runs_under_another_loop() {
        let (_dir, db) = queue_test_db();
        db.insert_loop_spec(&running_spec("spec-a")).unwrap();
        insert_queue(&db, "queue-1");
        db.append_queue_member("queue-1", "spec-a", None).unwrap();
        insert_test_loop(&db, "loop-other");
        insert_test_node(&db, "node-1", "spec-a");
        db.insert_loop_run(&loop_run_row(
            "run-1",
            "loop-other",
            "spec-a",
            LoopRunStatus::Running,
        ))
        .unwrap();

        let error = validate_queue_not_consumed(&db, "queue-1", "loop-requesting").unwrap_err();

        assert!(error.contains("spec-a"), "{error}");
        assert!(error.contains("loop-other"), "{error}");
    }

    #[test]
    fn queue_not_consumed_allows_the_owning_loop_to_resume_its_own_running_spec() {
        // A paused queue run's active spec stays `running` between node
        // executions. Resuming the SAME loop against the SAME queue must not
        // be mistaken for a conflicting run.
        let (_dir, db) = queue_test_db();
        db.insert_loop_spec(&running_spec("spec-a")).unwrap();
        insert_queue(&db, "queue-1");
        db.append_queue_member("queue-1", "spec-a", None).unwrap();
        insert_test_loop(&db, "loop-owner");
        insert_test_node(&db, "node-1", "spec-a");
        db.insert_loop_run(&loop_run_row(
            "run-1",
            "loop-owner",
            "spec-a",
            LoopRunStatus::Pass,
        ))
        .unwrap();

        assert!(validate_queue_not_consumed(&db, "queue-1", "loop-owner").is_ok());
    }

    /// B18 (Requirement 2): `skip_next_spec` on a queue-driven paused loop
    /// must find its in-flight member through the loop's persisted
    /// `active_run_queue_id` — the member's own `loop_id` column stays `None`
    /// (queue membership never binds it), so `list_loop_specs(loop_id)` alone
    /// can't see it. Before this fix `handle_skip_next_spec` always errored
    /// "No running spec found" for a queue-driven pause.
    #[test]
    fn skip_next_spec_finds_and_skips_the_running_queue_member() {
        let (_dir, db) = queue_test_db();
        insert_test_loop(&db, "loop-owner");
        db.set_loop_active_run_queue("loop-owner", Some("queue-1"))
            .unwrap();

        db.insert_loop_spec(&running_spec("spec-a")).unwrap();
        db.insert_loop_spec(&standalone_spec("spec-b")).unwrap();
        insert_queue(&db, "queue-1");
        db.append_queue_member("queue-1", "spec-a", None).unwrap();
        db.append_queue_member("queue-1", "spec-b", None).unwrap();

        handle_skip_next_spec(&db, "loop-owner").unwrap();

        let spec_a = db.get_loop_spec("spec-a").unwrap().unwrap();
        let spec_b = db.get_loop_spec("spec-b").unwrap().unwrap();
        assert_eq!(spec_a.status, LoopSpecStatus::Skipped);
        assert_eq!(spec_b.status, LoopSpecStatus::Pending);
    }

    #[test]
    fn skip_next_spec_prefers_the_loop_bound_spec_over_queue_context() {
        // A loop with its own bound `running` spec must use that, even if a
        // stale `active_run_queue_id` is still sitting on the loop from an
        // earlier, unrelated queue run.
        let (_dir, db) = queue_test_db();
        insert_test_loop(&db, "loop-owner");
        db.set_loop_active_run_queue("loop-owner", Some("queue-1"))
            .unwrap();

        let mut bound = running_spec("spec-bound");
        bound.loop_id = Some("loop-owner".to_string());
        db.insert_loop_spec(&bound).unwrap();
        db.insert_loop_spec(&running_spec("spec-queue")).unwrap();
        insert_queue(&db, "queue-1");
        db.append_queue_member("queue-1", "spec-queue", None)
            .unwrap();

        handle_skip_next_spec(&db, "loop-owner").unwrap();

        let bound_after = db.get_loop_spec("spec-bound").unwrap().unwrap();
        let queue_after = db.get_loop_spec("spec-queue").unwrap().unwrap();
        assert_eq!(bound_after.status, LoopSpecStatus::Skipped);
        assert_eq!(queue_after.status, LoopSpecStatus::Running);
    }

    #[test]
    fn skip_next_spec_errors_when_no_spec_is_running_anywhere() {
        let (_dir, db) = queue_test_db();
        insert_test_loop(&db, "loop-owner");

        let error = handle_skip_next_spec(&db, "loop-owner").unwrap_err();
        assert!(error.message.contains("No running spec found"));
    }

    /// B35: `retry_current_node` must error when no spec is running in
    /// either the loop's bound specs or its queue — same validation shape as
    /// `skip_next_spec`.
    #[test]
    fn retry_current_node_errors_when_no_running_spec() {
        let (_dir, db) = queue_test_db();
        insert_test_loop(&db, "loop-owner");
        let error = handle_retry_current_node(&db, "loop-owner").unwrap_err();
        assert!(error.message.contains("No running spec found"));
    }

    /// B35: `retry_current_node` must find the running spec through the
    /// loop's persisted `active_run_queue_id` (queue member has `loop_id: None`).
    #[test]
    fn retry_current_node_finds_running_queue_member() {
        let (_dir, db) = queue_test_db();
        insert_test_loop(&db, "loop-owner");
        db.set_loop_active_run_queue("loop-owner", Some("queue-1"))
            .unwrap();

        db.insert_loop_spec(&running_spec("spec-a")).unwrap();
        db.insert_loop_spec(&standalone_spec("spec-b")).unwrap();
        insert_queue(&db, "queue-1");
        db.append_queue_member("queue-1", "spec-a", None).unwrap();
        db.append_queue_member("queue-1", "spec-b", None).unwrap();

        // Should succeed without error — a running spec exists.
        assert!(handle_retry_current_node(&db, "loop-owner").is_ok());
    }

    #[test]
    fn queue_reorder_rejects_duplicate_spec() {
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

        let error = validate_queue_reorder(&current, &order).unwrap_err();
        assert!(error.contains("more than once"), "{error}");
    }

    #[test]
    fn queue_reorder_locking_refuses_ordering_that_moves_a_running_member() {
        // R6: the currently running spec is immutable in the queue's order.
        // Swapping it with a pending member must be refused, even though the
        // result is still a valid total permutation.
        let (_dir, db) = queue_test_db();
        db.insert_loop_spec(&running_spec("spec-a")).unwrap();
        db.insert_loop_spec(&standalone_spec("spec-b")).unwrap();
        db.insert_loop_spec(&standalone_spec("spec-c")).unwrap();
        insert_queue(&db, "queue-1");
        for id in ["spec-a", "spec-b", "spec-c"] {
            db.append_queue_member("queue-1", id, None).unwrap();
        }
        let current = db.list_queue_member_spec_ids("queue-1").unwrap();

        // Moves spec-a (running) from position 0 to position 1.
        let order = vec![
            "spec-b".to_string(),
            "spec-a".to_string(),
            "spec-c".to_string(),
        ];
        assert!(validate_queue_reorder(&current, &order).is_ok());

        let error = validate_queue_reorder_locking(&db, &current, &order).unwrap_err();
        assert!(error.contains("spec-a"), "{error}");
        assert!(error.contains("running"), "{error}");
    }

    #[test]
    fn queue_reorder_locking_refuses_ordering_that_moves_a_completed_member() {
        let (_dir, db) = queue_test_db();
        let mut done = standalone_spec("spec-a");
        done.status = LoopSpecStatus::Completed;
        db.insert_loop_spec(&done).unwrap();
        db.insert_loop_spec(&standalone_spec("spec-b")).unwrap();
        insert_queue(&db, "queue-1");
        for id in ["spec-a", "spec-b"] {
            db.append_queue_member("queue-1", id, None).unwrap();
        }
        let current = db.list_queue_member_spec_ids("queue-1").unwrap();

        let order = vec!["spec-b".to_string(), "spec-a".to_string()];
        let error = validate_queue_reorder_locking(&db, &current, &order).unwrap_err();
        assert!(error.contains("spec-a"), "{error}");
        assert!(error.contains("completed"), "{error}");
    }

    #[test]
    fn queue_reorder_locking_allows_permuting_pending_members_only() {
        let (_dir, db) = queue_test_db();
        db.insert_loop_spec(&running_spec("spec-a")).unwrap();
        db.insert_loop_spec(&standalone_spec("spec-b")).unwrap();
        db.insert_loop_spec(&standalone_spec("spec-c")).unwrap();
        insert_queue(&db, "queue-1");
        for id in ["spec-a", "spec-b", "spec-c"] {
            db.append_queue_member("queue-1", id, None).unwrap();
        }
        let current = db.list_queue_member_spec_ids("queue-1").unwrap();

        // spec-a (running) stays at position 0; only the pending tail moves.
        let order = vec![
            "spec-a".to_string(),
            "spec-c".to_string(),
            "spec-b".to_string(),
        ];
        assert!(validate_queue_reorder_locking(&db, &current, &order).is_ok());
    }

    #[test]
    fn queue_remove_spec_refuses_the_currently_running_spec() {
        let (_dir, db) = queue_test_db();
        db.insert_loop_spec(&running_spec("spec-a")).unwrap();
        insert_queue(&db, "queue-1");
        db.append_queue_member("queue-1", "spec-a", None).unwrap();

        let error = validate_queue_member_removable(&db, "queue-1", "spec-a").unwrap_err();
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
        assert!(validate_queue_member_removable(&db, "queue-1", "spec-a").is_ok());
    }

    #[test]
    fn queue_add_spec_rejects_nonexistent_spec() {
        let (_dir, db) = queue_test_db();
        insert_queue(&db, "queue-1");

        let error = validate_spec_exists(&db, "ghost-spec").unwrap_err();
        assert!(error.contains("not found"), "{error}");
    }

    #[test]
    fn queue_operations_reject_nonexistent_queue() {
        let (_dir, db) = queue_test_db();

        let error = validate_queue_exists(&db, "does-not-exist").unwrap_err();
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
            archived: false,
            paused_by_reconciliation: false,
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
            active_run_queue_id: None,
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
            archived: false,
            paused_by_reconciliation: false,
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
            active_run_queue_id: None,
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
            archived: false,
            paused_by_reconciliation: false,
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
            active_run_queue_id: None,
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
            archived: false,
            paused_by_reconciliation: false,
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
            active_run_queue_id: None,
            on_completed: None,
        }
    }

    /// B41: a `failed` loop with a pending autorun is exactly the state a
    /// scheduled wake-up needs to be cancellable from — cancelling must
    /// succeed and report the time that was cleared.
    #[tokio::test]
    async fn loop_schedule_autorun_cancels_pending_schedule_on_failed_loop() {
        use crate::daemon::params_extract::Parameters;

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
        use crate::daemon::params_extract::Parameters;

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
        use crate::daemon::params_extract::Parameters;

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
        use crate::daemon::params_extract::Parameters;

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
        use crate::daemon::params_extract::Parameters;

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
        use crate::daemon::params_extract::Parameters;

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
        use crate::daemon::params_extract::Parameters;

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
        use crate::daemon::params_extract::Parameters;

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
        use crate::daemon::params_extract::Parameters;

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
            ("claude".to_string(), None, None),
            ("codex".to_string(), Some("o1".to_string()), None),
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

    // ── validate_edge_condition_with_route ───────────────────────────

    #[test]
    fn validate_edge_condition_with_route_builds_route_condition() {
        let c = validate_edge_condition_with_route("route", Some("escalate")).unwrap();
        assert_eq!(c, LoopEdgeCondition::Route("escalate".to_string()));
    }

    #[test]
    fn validate_edge_condition_with_route_requires_non_empty_label() {
        let err = validate_edge_condition_with_route("route", None).unwrap_err();
        assert!(err.contains("non-empty 'route' label"), "{err}");

        let err = validate_edge_condition_with_route("route", Some("  ")).unwrap_err();
        assert!(err.contains("non-empty 'route' label"), "{err}");
    }

    #[test]
    fn validate_edge_condition_with_route_leaves_simple_conditions_untouched() {
        assert_eq!(
            validate_edge_condition_with_route("pass", None).unwrap(),
            LoopEdgeCondition::Pass
        );
        assert_eq!(
            validate_edge_condition_with_route("fail", None).unwrap(),
            LoopEdgeCondition::Fail
        );
        assert_eq!(
            validate_edge_condition_with_route("always", None).unwrap(),
            LoopEdgeCondition::Always
        );
        assert!(validate_edge_condition_with_route("sideways", None).is_err());
    }

    // ── validate_route_edge_target ────────────────────────────────────

    #[test]
    fn validate_route_edge_target_is_noop_for_non_route_conditions() {
        let dir = tempdir().unwrap();
        let db = Database::new(&dir.path().join("test.db")).unwrap();
        // No node inserted at all — a non-route condition must never look it
        // up, let alone fail over a missing node.
        assert!(validate_route_edge_target(&db, "missing-node", &LoopEdgeCondition::Pass).is_ok());
    }

    /// Insert a standalone spec (no owning loop) so a test can hang nodes
    /// off it without needing a full `Loop` row too.
    fn insert_standalone_spec(db: &Database, id: &str) {
        db.insert_loop_spec(&LoopSpec {
            id: id.to_string(),
            loop_id: None,
            name: id.to_string(),
            description: None,
            position: 1,
            parallelizable: false,
            status: LoopSpecStatus::Pending,
            started_at: None,
            completed_at: None,
            spec_start_head: None,
            spec_committed_head: None,
            workdir: None,
            completed_via: None,
            completed_via_reason: None,
            completed_via_at: None,
        })
        .unwrap();
    }

    #[test]
    fn validate_route_edge_target_rejects_non_router_from_node() {
        let dir = tempdir().unwrap();
        let db = Database::new(&dir.path().join("test.db")).unwrap();
        insert_standalone_spec(&db, "spec-1");
        db.insert_loop_node(&LoopNode {
            id: "agent-1".to_string(),
            spec_id: Some("spec-1".to_string()),
            loop_id: None,
            name: "Agent".to_string(),
            kind: LoopNodeKind::Agent,
            config: serde_json::json!({ "platform": "claude" }),
            position: 1,
            created_at: chrono::Utc::now(),
        })
        .unwrap();

        let err = validate_route_edge_target(
            &db,
            "agent-1",
            &LoopEdgeCondition::Route("retry".to_string()),
        )
        .unwrap_err();
        assert!(err.contains("router node"), "{err}");
    }

    #[test]
    fn validate_route_edge_target_rejects_undeclared_route() {
        let dir = tempdir().unwrap();
        let db = Database::new(&dir.path().join("test.db")).unwrap();
        insert_standalone_spec(&db, "spec-1");
        db.insert_loop_node(&LoopNode {
            id: "router-1".to_string(),
            spec_id: Some("spec-1".to_string()),
            loop_id: None,
            name: "Router".to_string(),
            kind: LoopNodeKind::Router,
            config: router_config(&two_routes_json(), "retry"),
            position: 1,
            created_at: chrono::Utc::now(),
        })
        .unwrap();

        let err = validate_route_edge_target(
            &db,
            "router-1",
            &LoopEdgeCondition::Route("nonexistent".to_string()),
        )
        .unwrap_err();
        assert!(err.contains("undeclared route 'nonexistent'"), "{err}");
    }

    #[test]
    fn validate_route_edge_target_accepts_declared_route() {
        let dir = tempdir().unwrap();
        let db = Database::new(&dir.path().join("test.db")).unwrap();
        insert_standalone_spec(&db, "spec-1");
        db.insert_loop_node(&LoopNode {
            id: "router-1".to_string(),
            spec_id: Some("spec-1".to_string()),
            loop_id: None,
            name: "Router".to_string(),
            kind: LoopNodeKind::Router,
            config: router_config(&two_routes_json(), "retry"),
            position: 1,
            created_at: chrono::Utc::now(),
        })
        .unwrap();

        assert!(validate_route_edge_target(
            &db,
            "router-1",
            &LoopEdgeCondition::Route("escalate".to_string()),
        )
        .is_ok());
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
        assert_eq!(
            validate_spec_status("interrupted").unwrap(),
            LoopSpecStatus::Interrupted
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

    // ── validate_queue_reorder ───────────────────────────────────────

    #[test]
    fn validate_queue_reorder_accepts_valid_permutation() {
        let current = vec!["a".to_string(), "b".to_string(), "c".to_string()];
        let reordered = vec!["c".to_string(), "a".to_string(), "b".to_string()];
        assert!(validate_queue_reorder(&current, &reordered).is_ok());
    }

    #[test]
    fn validate_queue_reorder_accepts_same_order() {
        let current = vec!["a".to_string(), "b".to_string()];
        let reordered = vec!["a".to_string(), "b".to_string()];
        assert!(validate_queue_reorder(&current, &reordered).is_ok());
    }

    #[test]
    fn validate_queue_reorder_rejects_length_mismatch() {
        let current = vec!["a".to_string(), "b".to_string()];
        let reordered = vec!["a".to_string()];
        let err = validate_queue_reorder(&current, &reordered).unwrap_err();
        assert!(err.contains("2"), "{err}");
        assert!(err.contains("1"), "{err}");
    }

    #[test]
    fn validate_queue_reorder_rejects_duplicate() {
        let current = vec!["a".to_string(), "b".to_string(), "c".to_string()];
        let reordered = vec!["a".to_string(), "a".to_string(), "b".to_string()];
        let err = validate_queue_reorder(&current, &reordered).unwrap_err();
        assert!(err.contains("a"), "{err}");
        assert!(err.contains("more than once"), "{err}");
    }

    #[test]
    fn validate_queue_reorder_rejects_unknown_id() {
        let current = vec!["a".to_string(), "b".to_string()];
        let reordered = vec!["a".to_string(), "x".to_string()];
        let err = validate_queue_reorder(&current, &reordered).unwrap_err();
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
            spec_committed_head: None,
            workdir: Some("/tmp/project".to_string()),
            completed_via: None,
            completed_via_reason: None,
            completed_via_at: None,
        };
        let json = spec_summary_json(&spec, true);
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
            spec_committed_head: None,
            workdir: None,
            completed_via: Some("loop_reset".to_string()),
            completed_via_reason: None,
            completed_via_at: None,
        };
        let json = spec_summary_json(&spec, true);
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
            archived: false,
            paused_by_reconciliation: false,
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
            active_run_queue_id: None,
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
            LoopSpecStatus::Interrupted,
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
    use crate::domain::queues::Queue;
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
            spec_committed_head: None,
            workdir: None,
            completed_via: None,
            completed_via_reason: None,
            completed_via_at: None,
        }
    }

    fn insert_test_loop(db: &Database, id: &str) {
        db.insert_loop(&Loop {
            archived: false,
            paused_by_reconciliation: false,
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
            active_run_queue_id: None,
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

    // ── validate_queue_reorder edge cases ──────────────────────────

    #[test]
    fn validate_queue_reorder_empty_current_and_ids() {
        assert!(validate_queue_reorder(&[], &[]).is_ok());
    }

    #[test]
    fn validate_queue_reorder_single_element() {
        let current = vec!["a".to_string()];
        let reordered = vec!["a".to_string()];
        assert!(validate_queue_reorder(&current, &reordered).is_ok());
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

    // ── validate_queue_reorder_locking: pending spec is movable ────

    #[test]
    fn validate_queue_reorder_locking_allows_moving_pending_members() {
        let dir = tempdir().unwrap();
        let db = Database::new(&dir.path().join("test.db")).unwrap();
        db.insert_loop_spec(&standalone_spec("spec-a")).unwrap();
        db.insert_loop_spec(&standalone_spec("spec-b")).unwrap();
        db.insert_loop_spec(&standalone_spec("spec-c")).unwrap();
        db.insert_queue(&Queue {
            id: "queue-1".to_string(),
            name: "queue-1".to_string(),
            created_at: chrono::Utc::now(),
        })
        .unwrap();
        for id in ["spec-a", "spec-b", "spec-c"] {
            db.append_queue_member("queue-1", id, None).unwrap();
        }
        let current = db.list_queue_member_spec_ids("queue-1").unwrap();

        // All pending — any permutation is allowed.
        let order = vec![
            "spec-c".to_string(),
            "spec-a".to_string(),
            "spec-b".to_string(),
        ];
        assert!(validate_queue_reorder_locking(&db, &current, &order).is_ok());
    }

    // ── validate_queue_member_removable: non-running spec ──────────

    #[test]
    fn validate_queue_member_removable_pending_spec_ok() {
        let dir = tempdir().unwrap();
        let db = Database::new(&dir.path().join("test.db")).unwrap();
        db.insert_loop_spec(&standalone_spec("spec-a")).unwrap();
        db.insert_queue(&Queue {
            id: "queue-1".to_string(),
            name: "queue-1".to_string(),
            created_at: chrono::Utc::now(),
        })
        .unwrap();
        assert!(validate_queue_member_removable(&db, "queue-1", "spec-a").is_ok());
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

    // ── validate_queue_not_consumed: empty queue ────────────────────

    #[test]
    fn validate_queue_not_consumed_empty_queue() {
        let dir = tempdir().unwrap();
        let db = Database::new(&dir.path().join("test.db")).unwrap();
        db.insert_queue(&Queue {
            id: "queue-empty".to_string(),
            name: "empty".to_string(),
            created_at: chrono::Utc::now(),
        })
        .unwrap();
        assert!(validate_queue_not_consumed(&db, "queue-empty", "loop-1").is_ok());
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
            spec_committed_head: None,
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
                prompt_override: None,
            },
            EnsembleMemberParams {
                platform: "\t\n".to_string(),
                model: None,
                prompt_override: None,
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
                prompt_override: None,
            },
            EnsembleMemberParams {
                platform: "mimo".to_string(),
                model: Some("   ".to_string()),
                prompt_override: None,
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
            spec_committed_head: None,
            workdir: None,
            completed_via: None,
            completed_via_reason: None,
            completed_via_at: None,
        };
        let json = spec_summary_json(&spec, true);
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

    // ── validate_queue_reorder_locking: skipped spec is locked ─────

    #[test]
    fn validate_queue_reorder_locking_refuses_moving_skipped_spec() {
        let dir = tempdir().unwrap();
        let db = Database::new(&dir.path().join("test.db")).unwrap();
        let mut skipped = standalone_spec("spec-a");
        skipped.status = LoopSpecStatus::Skipped;
        db.insert_loop_spec(&skipped).unwrap();
        db.insert_loop_spec(&standalone_spec("spec-b")).unwrap();
        db.insert_queue(&Queue {
            id: "queue-1".to_string(),
            name: "queue-1".to_string(),
            created_at: chrono::Utc::now(),
        })
        .unwrap();
        db.append_queue_member("queue-1", "spec-a", None).unwrap();
        db.append_queue_member("queue-1", "spec-b", None).unwrap();
        let current = db.list_queue_member_spec_ids("queue-1").unwrap();

        let order = vec!["spec-b".to_string(), "spec-a".to_string()];
        let error = validate_queue_reorder_locking(&db, &current, &order).unwrap_err();
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
        assert!(matches!(
            validate_spec_status("interrupted").unwrap(),
            LoopSpecStatus::Interrupted
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
        let json = spec_summary_json(&spec, true);
        assert_eq!(json["id"], "spec-1");
        assert_eq!(json["name"], "spec-1");
        assert_eq!(json["status"], "pending");
        assert_eq!(json["parallelizable"], false);
    }

    #[test]
    fn spec_summary_json_with_completed_via() {
        let mut spec = standalone_spec("spec-1");
        spec.completed_via = Some("test".to_string());
        let json = spec_summary_json(&spec, true);
        assert_eq!(json["completed_via"], "test");
    }

    #[test]
    fn spec_summary_json_with_workdir() {
        let mut spec = standalone_spec("spec-1");
        spec.workdir = Some("/tmp/project".to_string());
        let json = spec_summary_json(&spec, true);
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
            archived: false,
            paused_by_reconciliation: false,
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
            active_run_queue_id: None,
            on_completed: None,
        };
        let json = loop_trigger_json(&lp);
        assert_eq!(json["type"], "manual");
        assert!(json.get("schedule").is_none());
    }

    #[test]
    fn loop_trigger_json_cron() {
        let lp = Loop {
            archived: false,
            paused_by_reconciliation: false,
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
            active_run_queue_id: None,
            on_completed: None,
        };
        let json = loop_trigger_json(&lp);
        assert_eq!(json["type"], "cron");
        assert_eq!(json["schedule"], "0 9 * * *");
    }

    // ── validate_queue_reorder ─────────────────────────────────────

    #[test]
    fn validate_queue_reorder_wrong_count() {
        let current = vec!["a".to_string(), "b".to_string()];
        let spec_ids = vec!["a".to_string()];
        assert!(validate_queue_reorder(&current, &spec_ids).is_err());
    }

    #[test]
    fn validate_queue_reorder_duplicate() {
        let current = vec!["a".to_string(), "b".to_string()];
        let spec_ids = vec!["a".to_string(), "a".to_string()];
        assert!(validate_queue_reorder(&current, &spec_ids).is_err());
    }

    #[test]
    fn validate_queue_reorder_unknown_spec() {
        let current = vec!["a".to_string(), "b".to_string()];
        let spec_ids = vec!["a".to_string(), "c".to_string()];
        assert!(validate_queue_reorder(&current, &spec_ids).is_err());
    }

    #[test]
    fn validate_queue_reorder_valid() {
        let current = vec!["a".to_string(), "b".to_string()];
        let spec_ids = vec!["b".to_string(), "a".to_string()];
        assert!(validate_queue_reorder(&current, &spec_ids).is_ok());
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
    use crate::domain::queues::Queue;
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
            spec_committed_head: None,
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
            archived: false,
            paused_by_reconciliation: false,
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
            active_run_queue_id: None,
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
        let json = super::loop_node_json(&node, &[]);
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
        let json = super::loop_node_json(&node, &[]);
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
                    prompt_override: Some("review for security issues only".into()),
                },
                EnsembleMember {
                    ensemble_id: "ens1".into(),
                    node_id: "m2".into(),
                    position: 1,
                    platform: "codex".into(),
                    model: Some("o1".into()),
                    prompt_override: None,
                },
            ],
        };
        let json = super::ensemble_details_json(&details);
        assert_eq!(json["id"], "ens1");
        assert_eq!(json["min_pass"], 2);
        assert_eq!(json["effective_straggler_timeout_minutes"], 10);
        assert_eq!(json["on_fail_to"], "cleanup");
        assert_eq!(json["members"].as_array().unwrap().len(), 2);
        let members = json["members"].as_array().unwrap();
        assert_eq!(members[0]["prompt_source"], "override");
        assert_eq!(
            members[0]["prompt_override"],
            "review for security issues only"
        );
        assert_eq!(members[1]["prompt_source"], "shared");
        assert!(members[1]["prompt_override"].is_null());
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
                prompt_override: None,
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
        use crate::daemon::handler_formatting::ModelTruncation;
        use crate::domain::models_db::CatalogSource;
        let f = super::model_result_footer(
            "Models:",
            CatalogSource::Live,
            std::time::SystemTime::now(),
            std::time::Duration::from_secs(24 * 60 * 60),
            &ModelTruncation::default(),
        );
        assert!(f.contains("Source: live"));
        assert!(!f.contains("unreachable"));
        assert!(f.contains("age:"));
        assert!(f.contains("refresh interval"));
    }

    #[test]
    fn footer_stale_source_says_how_stale_in_the_same_line() {
        use crate::daemon::handler_formatting::ModelTruncation;
        use crate::domain::models_db::CatalogSource;
        let fetched_at = std::time::SystemTime::now() - std::time::Duration::from_secs(30 * 3600);
        let f = super::model_result_footer(
            "Models:",
            CatalogSource::Stale,
            fetched_at,
            std::time::Duration::from_secs(24 * 60 * 60),
            &ModelTruncation::default(),
        );
        let source_line = f
            .lines()
            .find(|l| l.starts_with("Source:"))
            .expect("footer must have a Source line");
        assert!(source_line.contains("unreachable"));
        // How stale must be on the *same* line as the unreachable notice, not
        // just somewhere in the footer. 30h renders as "1d6h".
        assert!(source_line.contains("1d6h"));
    }

    #[test]
    fn footer_fresh_cache_does_not_say_refresh_due() {
        use crate::daemon::handler_formatting::ModelTruncation;
        use crate::domain::models_db::CatalogSource;
        let f = super::model_result_footer(
            "Models:",
            CatalogSource::Cache,
            std::time::SystemTime::now() - std::time::Duration::from_secs(60),
            std::time::Duration::from_secs(24 * 60 * 60),
            &ModelTruncation::default(),
        );
        assert!(!f.contains("refresh due"));
        assert!(f.contains("age: 1m"));
    }

    #[test]
    fn footer_states_refresh_due_when_age_exceeds_ttl() {
        use crate::daemon::handler_formatting::ModelTruncation;
        use crate::domain::models_db::CatalogSource;
        // A source other than Stale whose age has still crept past the TTL
        // (e.g. the TTL was lowered after the cache was written) must still
        // surface a due-for-refresh notice, not just the raw age.
        let fetched_at = std::time::SystemTime::now() - std::time::Duration::from_secs(2 * 3600);
        let f = super::model_result_footer(
            "Models:",
            CatalogSource::Live,
            fetched_at,
            std::time::Duration::from_secs(3600),
            &ModelTruncation::default(),
        );
        assert!(f.contains("refresh due"));
    }

    #[test]
    fn format_duration_short_renders_expected_units() {
        use std::time::Duration;
        assert_eq!(super::format_duration_short(Duration::from_secs(45)), "45s");
        assert_eq!(super::format_duration_short(Duration::from_secs(90)), "1m");
        assert_eq!(
            super::format_duration_short(Duration::from_secs(2 * 3600 + 15 * 60)),
            "2h15m"
        );
        assert_eq!(
            super::format_duration_short(Duration::from_secs(24 * 3600)),
            "1d"
        );
        assert_eq!(
            super::format_duration_short(Duration::from_secs(3 * 24 * 3600 + 4 * 3600)),
            "3d4h"
        );
    }

    // ── build_ensemble_unit: with and without on_fail_to ───────────

    #[test]
    fn ensemble_unit_with_fail_to() {
        let members = vec![
            ("claude".into(), None, None),
            ("codex".into(), Some("o1".into()), None),
        ];
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
        let members = vec![("claude".into(), None, None)];
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
            ("p1".into(), None, None),
            ("p2".into(), None, None),
            ("p3".into(), None, None),
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

    // ── validate_queue_not_consumed edge cases ──────────────────────

    #[test]
    fn queue_not_consumed_empty_queue() {
        let dir = tempdir().unwrap();
        let db = Database::new(&dir.path().join("test.db")).unwrap();
        db.insert_queue(&Queue {
            id: "queue-e".into(),
            name: "empty".into(),
            created_at: chrono::Utc::now(),
        })
        .unwrap();
        assert!(validate_queue_not_consumed(&db, "queue-e", "loop-1").is_ok());
    }

    #[test]
    fn queue_not_consumed_pending_spec() {
        let dir = tempdir().unwrap();
        let db = Database::new(&dir.path().join("test.db")).unwrap();
        db.insert_loop_spec(&standalone_spec("s1")).unwrap();
        db.insert_queue(&Queue {
            id: "queue-p".into(),
            name: "queue-p".into(),
            created_at: chrono::Utc::now(),
        })
        .unwrap();
        db.append_queue_member("queue-p", "s1", None).unwrap();
        assert!(validate_queue_not_consumed(&db, "queue-p", "loop-other").is_ok());
    }

    #[test]
    fn queue_not_consumed_own_loop() {
        let dir = tempdir().unwrap();
        let db = Database::new(&dir.path().join("test.db")).unwrap();
        db.insert_loop_spec(&running_spec("s-owned")).unwrap();
        db.insert_loop(&make_loop("loop-owner", LoopStatus::Running))
            .unwrap();
        db.insert_queue(&Queue {
            id: "queue-o".into(),
            name: "queue-o".into(),
            created_at: chrono::Utc::now(),
        })
        .unwrap();
        db.append_queue_member("queue-o", "s-owned", None).unwrap();
        insert_test_node(&db, "node-1", "s-owned");
        db.insert_loop_run(&loop_run_row(
            "run1",
            "loop-owner",
            "s-owned",
            LoopRunStatus::Running,
        ))
        .unwrap();
        assert!(validate_queue_not_consumed(&db, "queue-o", "loop-owner").is_ok());
    }

    // ── validate_queue_member_removable edge cases ──────────────────

    #[test]
    fn queue_member_removable_pending() {
        let dir = tempdir().unwrap();
        let db = Database::new(&dir.path().join("test.db")).unwrap();
        db.insert_loop_spec(&standalone_spec("s1")).unwrap();
        db.insert_queue(&Queue {
            id: "queue-r".into(),
            name: "queue-r".into(),
            created_at: chrono::Utc::now(),
        })
        .unwrap();
        assert!(validate_queue_member_removable(&db, "queue-r", "s1").is_ok());
    }

    #[test]
    fn queue_member_removable_nonexistent() {
        let dir = tempdir().unwrap();
        let db = Database::new(&dir.path().join("test.db")).unwrap();
        db.insert_queue(&Queue {
            id: "queue-r".into(),
            name: "queue-r".into(),
            created_at: chrono::Utc::now(),
        })
        .unwrap();
        assert!(validate_queue_member_removable(&db, "queue-r", "ghost").is_ok());
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

    // ── validate_queue_reorder_locking edge cases ───────────────────

    #[test]
    fn reorder_locking_all_pending() {
        let dir = tempdir().unwrap();
        let db = Database::new(&dir.path().join("test.db")).unwrap();
        db.insert_loop_spec(&standalone_spec("a")).unwrap();
        db.insert_loop_spec(&standalone_spec("b")).unwrap();
        db.insert_loop_spec(&standalone_spec("c")).unwrap();
        db.insert_queue(&Queue {
            id: "p1".into(),
            name: "p1".into(),
            created_at: chrono::Utc::now(),
        })
        .unwrap();
        for id in ["a", "b", "c"] {
            db.append_queue_member("p1", id, None).unwrap();
        }
        let current = db.list_queue_member_spec_ids("p1").unwrap();
        let order = vec!["c".into(), "a".into(), "b".into()];
        assert!(validate_queue_reorder_locking(&db, &current, &order).is_ok());
    }

    #[test]
    fn reorder_locking_failed_spec_locked() {
        let dir = tempdir().unwrap();
        let db = Database::new(&dir.path().join("test.db")).unwrap();
        let mut f = standalone_spec("f");
        f.status = LoopSpecStatus::Failed;
        db.insert_loop_spec(&f).unwrap();
        db.insert_loop_spec(&standalone_spec("p")).unwrap();
        db.insert_queue(&Queue {
            id: "p1".into(),
            name: "p1".into(),
            created_at: chrono::Utc::now(),
        })
        .unwrap();
        db.append_queue_member("p1", "f", None).unwrap();
        db.append_queue_member("p1", "p", None).unwrap();
        let current = db.list_queue_member_spec_ids("p1").unwrap();
        let order = vec!["p".into(), "f".into()];
        let err = validate_queue_reorder_locking(&db, &current, &order).unwrap_err();
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
        db.insert_queue(&Queue {
            id: "p1".into(),
            name: "p1".into(),
            created_at: chrono::Utc::now(),
        })
        .unwrap();
        db.append_queue_member("p1", "s", None).unwrap();
        db.append_queue_member("p1", "p", None).unwrap();
        let current = db.list_queue_member_spec_ids("p1").unwrap();
        let order = vec!["p".into(), "s".into()];
        let err = validate_queue_reorder_locking(&db, &current, &order).unwrap_err();
        assert!(err.contains("skipped"), "{err}");
    }

    #[test]
    fn reorder_locking_mixed_pending_and_running() {
        let dir = tempdir().unwrap();
        let db = Database::new(&dir.path().join("test.db")).unwrap();
        db.insert_loop_spec(&running_spec("r")).unwrap();
        db.insert_loop_spec(&standalone_spec("p1")).unwrap();
        db.insert_loop_spec(&standalone_spec("p2")).unwrap();
        db.insert_queue(&Queue {
            id: "p1".into(),
            name: "p1".into(),
            created_at: chrono::Utc::now(),
        })
        .unwrap();
        db.append_queue_member("p1", "r", None).unwrap();
        db.append_queue_member("p1", "p1", None).unwrap();
        db.append_queue_member("p1", "p2", None).unwrap();
        let current = db.list_queue_member_spec_ids("p1").unwrap();
        let order = vec!["r".into(), "p2".into(), "p1".into()];
        assert!(validate_queue_reorder_locking(&db, &current, &order).is_ok());
    }

    // ── validate_queue_reorder: all permutations ────────────────────

    #[test]
    fn queue_reorder_all_perms_of_three() {
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
            assert!(validate_queue_reorder(&current, &reordered).is_ok());
        }
    }

    #[test]
    fn queue_reorder_empty() {
        assert!(validate_queue_reorder(&[], &[]).is_ok());
    }

    #[test]
    fn queue_reorder_large_queue() {
        let current: Vec<String> = (0..100).map(|i| format!("s{i}")).collect();
        let mut reordered = current.clone();
        reordered.reverse();
        assert!(validate_queue_reorder(&current, &reordered).is_ok());
    }

    // ── validate_ensemble_members: boundaries ──────────────────────

    #[test]
    fn ensemble_exactly_min() {
        let m: Vec<EnsembleMemberParams> = (0..2)
            .map(|i| EnsembleMemberParams {
                platform: format!("p{i}"),
                model: None,
                prompt_override: None,
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
                prompt_override: None,
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
                prompt_override: None,
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
                prompt_override: None,
            },
            EnsembleMemberParams {
                platform: "mimo".into(),
                model: None,
                prompt_override: None,
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
            LoopSpecStatus::Interrupted,
        ] {
            let mut spec = standalone_spec("s");
            spec.status = status;
            let json = spec_summary_json(&spec, true);
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

    #[test]
    fn spec_summary_json_compact_omits_description() {
        let mut spec = standalone_spec("spec-1");
        spec.description = Some("A long description".to_string());
        let json = spec_summary_json(&spec, false);
        assert!(
            json.get("description").is_none(),
            "compact mode must omit description"
        );
        assert_eq!(json["id"], "spec-1");
        assert_eq!(json["name"], "spec-1");
        assert_eq!(json["status"], "pending");
    }

    #[test]
    fn queue_details_json_compact_members_have_position_and_group() {
        let spec = standalone_spec("spec-1");
        let queue = Queue {
            id: "q-1".to_string(),
            name: "test-queue".to_string(),
            created_at: chrono::Utc::now(),
        };
        let mut member_groups = std::collections::HashMap::new();
        member_groups.insert("spec-1".to_string(), Some("group-a".to_string()));
        let details = QueueDetails {
            queue,
            members: vec![spec],
            member_groups,
        };
        let json = queue_details_json(&details, false);
        let members = json["members"].as_array().unwrap();
        assert_eq!(members.len(), 1);
        assert_eq!(members[0]["queue_position"], 1);
        assert_eq!(members[0]["group"], "group-a");
        assert!(
            members[0].get("description").is_none(),
            "compact mode must omit description"
        );
    }

    #[test]
    fn loop_node_run_summary_json_compact_mode_omits_ids() {
        let dir = tempdir().unwrap();
        let db = Database::new(&dir.path().join("test.db")).unwrap();
        db.insert_loop_spec(&standalone_spec("spec-1")).unwrap();
        insert_test_node(&db, "node-1", "spec-1");
        let run = loop_run_row("run-1", "loop-1", "spec-1", LoopRunStatus::Pass);

        let compact = loop_node_run_summary_json(&db, &run, true);
        assert!(compact.get("id").is_none(), "compact must omit id");
        assert!(
            compact.get("spec_id").is_none(),
            "compact must omit spec_id"
        );
        assert!(
            compact.get("node_id").is_none(),
            "compact must omit node_id"
        );
        assert!(
            compact.get("session_id").is_none(),
            "compact must omit session_id"
        );
        assert_eq!(compact["spec_name"], "spec-1");
        assert_eq!(compact["node_name"], "node-1");
        assert_eq!(compact["status"], "pass");
        assert_eq!(compact["iteration"], 1);

        let full = loop_node_run_summary_json(&db, &run, false);
        assert_eq!(full["id"], "run-1");
        assert_eq!(full["spec_id"], "spec-1");
        assert_eq!(full["node_id"], "node-1");
        assert!(
            full.get("spec_name").is_none(),
            "full mode does not carry spec_name"
        );
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
    use crate::daemon::params_extract::Parameters;
    use crate::db::Database;
    use crate::domain::models::{Agent, Cli, RunLog, RunStatus, TriggerType};
    use crate::executor::Executor;
    use crate::loop_engine::LoopEngine;
    use crate::rag::ingestion::IngestionManager;
    use crate::sync_manager::SyncManager;
    use crate::watchers::WatcherEngine;
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

    /// The raw (unescaped) text of a result's first content block — unlike
    /// [`text`] (which Debug-formats the whole `Vec<Content>`, escaping
    /// embedded quotes), this is safe to `serde_json::from_str` or otherwise
    /// parse structurally.
    fn raw_text(result: &CallToolResult) -> String {
        result.content[0].as_text().unwrap().text.clone()
    }

    /// Parse a `build_id_result`-shaped response (`{"<key>": "<id>"}`) and
    /// return the id.
    fn extract_id(result: &CallToolResult, key: &str) -> String {
        let value: serde_json::Value = serde_json::from_str(&raw_text(result))
            .unwrap_or_else(|e| panic!("expected JSON body, got {:?}: {e}", raw_text(result)));
        value[key]
            .as_str()
            .unwrap_or_else(|| panic!("expected string field '{key}' in {value}"))
            .to_string()
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

    // Note: These tests fail in CI due to HOME env var contention between
    // concurrent async tests. Marked as ignored until test isolation is improved.
    #[tokio::test]
    #[ignore]
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
                full: None,
            }))
            .await
            .unwrap();

        assert!(is_err(&result));
        assert!(text(&result).contains("is not configured in canopy"));
    }

    #[tokio::test]
    #[ignore]
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
                full: None,
            }))
            .await
            .unwrap();

        assert!(!is_err(&result), "{}", text(&result));
        let out = text(&result);
        assert!(out.contains("model-a"));
        assert!(out.contains("model-b"));
        assert!(out.contains("Source:"));
        assert!(
            out.contains("PROVIDER'S CATALOG") || out.contains("WARNING"),
            "footer must warn that this is the provider catalog, not account availability: {out}"
        );
    }

    #[tokio::test]
    async fn task_models_warns_for_platform_without_model_flag() {
        let home = tempdir().unwrap();
        let canopy_dir = home.path().join(".canopy");
        std::fs::create_dir_all(&canopy_dir).unwrap();
        let config = crate::domain::canopy_config::CanopyConfig {
            clis: vec![crate::domain::cli_config::CliConfig {
                name: "mistral".to_string(),
                binary: "/bin/echo".to_string(),
                ..Default::default()
            }],
            ..Default::default()
        };
        config.save(&canopy_dir).unwrap();
        // Seed a minimal models.dev catalog so the models.dev path succeeds
        // without a network fetch — `load_catalog_with_source` serves a fresh
        // cache with no network when the file exists and is within TTL.
        let cache_dir = canopy_dir.join("cache");
        std::fs::create_dir_all(&cache_dir).unwrap();
        let now_secs = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let catalog_json = format!(
            r#"{{"models":[{{"id":"mistral-medium-latest","name":"Mistral Medium","provider":"mistral"}}],"fetched_at":{now_secs}}}"#
        );
        std::fs::write(cache_dir.join("models_catalog.json"), catalog_json).unwrap();
        let _home_guard = HomeVar::set(home.path());

        let (_dir, _db, handler) = endpoint_test_handler();
        let result = handler
            .task_models(Parameters(TaskModelsParams {
                platform: Some("mistral".to_string()),
                refresh: None,
                full: None,
            }))
            .await
            .unwrap();

        assert!(!is_err(&result), "{}", text(&result));
        let out = text(&result);
        assert!(
            out.contains("does not support model selection") || out.contains("NOT valid input"),
            "must warn that this platform does not support model selection: {out}"
        );
    }

    #[tokio::test]
    async fn task_models_warns_for_native_enumeration_without_model_flag() {
        use std::os::unix::fs::PermissionsExt;

        // FR4: a platform can have a `models_list_cmd` (so `agent_models`
        // takes the native-enumeration path) while still having no
        // `model_flag` — the enumerated ids are not valid `model` input and
        // the output must say so, exactly as the models.dev path does.
        let home = tempdir().unwrap();
        let canopy_dir = home.path().join(".canopy");
        std::fs::create_dir_all(&canopy_dir).unwrap();
        let script = home.path().join("list-agents.sh");
        std::fs::write(&script, "#!/bin/sh\necho agent-one\necho agent-two\n").unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();

        let config = crate::domain::canopy_config::CanopyConfig {
            clis: vec![crate::domain::cli_config::CliConfig {
                name: "named-agent-cli".to_string(),
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
                platform: Some("named-agent-cli".to_string()),
                refresh: Some(true),
                full: None,
            }))
            .await
            .unwrap();

        assert!(!is_err(&result), "{}", text(&result));
        let out = text(&result);
        assert!(out.contains("agent-one"), "enumeration still shown: {out}");
        assert!(
            out.contains("does not support model selection") || out.contains("NOT valid input"),
            "native-enumeration output must also warn when there is no model_flag: {out}"
        );
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

    // ── loop_create / loop_update / loop graph tool handlers ─────

    fn valid_spec_description() -> String {
        "Functional Requirements: does the thing.\n\
         Non-Functional Requirements: is fast.\n\
         Objective: ship the feature.\n\
         Constraints: none extra.\n\
         Guidelines: follow house style.\n\
         In Scope: this change.\n\
         Out of Scope: everything else."
            .to_string()
    }

    fn agent_node_config(platform: &str) -> serde_json::Map<String, serde_json::Value> {
        let mut map = serde_json::Map::new();
        map.insert(
            "platform".to_string(),
            serde_json::Value::String(platform.to_string()),
        );
        map
    }

    #[tokio::test]
    async fn loop_create_and_update_round_trip() {
        let (dir, db, handler) = endpoint_test_handler();
        let workdir = dir.path().to_string_lossy().to_string();

        let created = handler
            .loop_create(Parameters(LoopCreateParams {
                name: "My Loop".to_string(),
                description: None,
                workdir: workdir.clone(),
                trigger: None,
            }))
            .await
            .unwrap();
        assert!(!is_err(&created), "{}", text(&created));
        let loop_id = extract_id(&created, "loop_id");
        assert!(db.get_loop(&loop_id).unwrap().is_some());

        let updated = handler
            .loop_update(Parameters(LoopUpdateParams {
                loop_id: loop_id.clone(),
                name: Some("Renamed Loop".to_string()),
                description: None,
                workdir: None,
                trigger: None,
                on_completed: None,
            }))
            .await
            .unwrap();
        assert!(!is_err(&updated), "{}", text(&updated));
        assert_eq!(db.get_loop(&loop_id).unwrap().unwrap().name, "Renamed Loop");
    }

    #[tokio::test]
    async fn loop_create_rejects_empty_name_and_relative_workdir() {
        let (_dir, _db, handler) = endpoint_test_handler();

        let empty_name = handler
            .loop_create(Parameters(LoopCreateParams {
                name: "  ".to_string(),
                description: None,
                workdir: "/tmp".to_string(),
                trigger: None,
            }))
            .await
            .unwrap();
        assert!(is_err(&empty_name));

        let bad_workdir = handler
            .loop_create(Parameters(LoopCreateParams {
                name: "Loop".to_string(),
                description: None,
                workdir: "relative/dir".to_string(),
                trigger: None,
            }))
            .await
            .unwrap();
        assert!(is_err(&bad_workdir));
        assert!(text(&bad_workdir).contains("absolute") || text(&bad_workdir).contains("exist"));
    }

    fn check_node_config(command: &str) -> serde_json::Map<String, serde_json::Value> {
        let mut map = serde_json::Map::new();
        map.insert(
            "command".to_string(),
            serde_json::Value::String(command.to_string()),
        );
        map
    }

    /// Builds a loop with implementer -> gate -> committer (agent, check,
    /// agent), returning the loop id and each node's id in graph order.
    async fn build_simple_loop(
        handler: &TaskTriggerHandler,
        workdir: &str,
        loop_name: &str,
    ) -> (String, String, String, String) {
        let created = handler
            .loop_create(Parameters(LoopCreateParams {
                name: loop_name.to_string(),
                description: Some("A shareable design".to_string()),
                workdir: workdir.to_string(),
                trigger: None,
            }))
            .await
            .unwrap();
        let loop_id = extract_id(&created, "loop_id");

        let n1 = handler
            .loop_add_node(Parameters(LoopAddNodeParams {
                spec_id: None,
                loop_id: Some(loop_id.clone()),
                name: "implementer".to_string(),
                kind: Some("agent".to_string()),
                config: Some(agent_node_config("claude")),
                blueprint: None,
                config_overrides: None,
            }))
            .await
            .unwrap();
        let n1_id = extract_id(&n1, "node_id");

        let n2 = handler
            .loop_add_node(Parameters(LoopAddNodeParams {
                spec_id: None,
                loop_id: Some(loop_id.clone()),
                name: "gate".to_string(),
                kind: Some("check".to_string()),
                config: Some(check_node_config("cargo test")),
                blueprint: None,
                config_overrides: None,
            }))
            .await
            .unwrap();
        let n2_id = extract_id(&n2, "node_id");

        let n3 = handler
            .loop_add_node(Parameters(LoopAddNodeParams {
                spec_id: None,
                loop_id: Some(loop_id.clone()),
                name: "committer".to_string(),
                kind: Some("agent".to_string()),
                config: Some(agent_node_config("claude")),
                blueprint: None,
                config_overrides: None,
            }))
            .await
            .unwrap();
        let n3_id = extract_id(&n3, "node_id");

        for (from, to, condition) in [(&n1_id, &n2_id, "always"), (&n2_id, &n3_id, "always")] {
            let edge = handler
                .loop_add_edge(Parameters(LoopAddEdgeParams {
                    spec_id: None,
                    loop_id: Some(loop_id.clone()),
                    from_node: from.clone(),
                    to_node: to.clone(),
                    condition: condition.to_string(),
                    route: None,
                }))
                .await
                .unwrap();
            assert!(!is_err(&edge), "{}", text(&edge));
        }

        (loop_id, n1_id, n2_id, n3_id)
    }

    #[tokio::test]
    async fn loop_export_returns_error_for_unknown_loop() {
        let (_dir, _db, handler) = endpoint_test_handler();
        let result = handler
            .loop_export(Parameters(LoopExportParams {
                loop_id: "missing".to_string(),
                with_models: None,
            }))
            .await
            .unwrap();
        assert!(is_err(&result));
        assert!(text(&result).contains("not found"));
    }

    #[tokio::test]
    async fn loop_export_strips_platform_by_default_and_keeps_it_with_with_models() {
        let (dir, _db, handler) = endpoint_test_handler();
        let workdir = dir.path().to_string_lossy().to_string();
        let (loop_id, ..) = build_simple_loop(&handler, &workdir, "Export Loop").await;

        let stripped = handler
            .loop_export(Parameters(LoopExportParams {
                loop_id: loop_id.clone(),
                with_models: None,
            }))
            .await
            .unwrap();
        assert!(!is_err(&stripped), "{}", text(&stripped));
        let doc: serde_json::Value = serde_json::from_str(&raw_text(&stripped)).unwrap();
        assert_eq!(doc["format_version"], 1);
        let implementer = doc["nodes"]
            .as_array()
            .unwrap()
            .iter()
            .find(|n| n["name"] == "implementer")
            .unwrap();
        assert!(implementer["config"].get("platform").is_none());

        let with_models = handler
            .loop_export(Parameters(LoopExportParams {
                loop_id,
                with_models: Some(true),
            }))
            .await
            .unwrap();
        let doc: serde_json::Value = serde_json::from_str(&raw_text(&with_models)).unwrap();
        let implementer = doc["nodes"]
            .as_array()
            .unwrap()
            .iter()
            .find(|n| n["name"] == "implementer")
            .unwrap();
        assert_eq!(implementer["config"]["platform"], "claude");
    }

    /// Decision 2's enforced consequence: `loop_add_node` doesn't itself
    /// forbid two nodes sharing a name, so export must catch it — naming
    /// the offending node(s) rather than producing an ambiguous file.
    #[tokio::test]
    async fn loop_export_rejects_duplicate_node_names() {
        let (dir, _db, handler) = endpoint_test_handler();
        let workdir = dir.path().to_string_lossy().to_string();
        let created = handler
            .loop_create(Parameters(LoopCreateParams {
                name: "Dup Loop".to_string(),
                description: None,
                workdir,
                trigger: None,
            }))
            .await
            .unwrap();
        let loop_id = extract_id(&created, "loop_id");
        for _ in 0..2 {
            handler
                .loop_add_node(Parameters(LoopAddNodeParams {
                    spec_id: None,
                    loop_id: Some(loop_id.clone()),
                    name: "dup".to_string(),
                    kind: Some("check".to_string()),
                    config: Some(check_node_config("true")),
                    blueprint: None,
                    config_overrides: None,
                }))
                .await
                .unwrap();
        }

        let result = handler
            .loop_export(Parameters(LoopExportParams {
                loop_id,
                with_models: None,
            }))
            .await
            .unwrap();
        assert!(is_err(&result));
        assert!(text(&result).contains("dup"));
        assert!(text(&result).to_lowercase().contains("duplicate"));
    }

    #[tokio::test]
    async fn loop_import_creates_new_loop_and_reports_missing_platform() {
        let (dir, db, handler) = endpoint_test_handler();
        let workdir = dir.path().to_string_lossy().to_string();
        let (source_loop_id, ..) = build_simple_loop(&handler, &workdir, "Source Loop").await;

        let exported = handler
            .loop_export(Parameters(LoopExportParams {
                loop_id: source_loop_id,
                with_models: None,
            }))
            .await
            .unwrap();
        let document: serde_json::Value = serde_json::from_str(&raw_text(&exported)).unwrap();

        let imported = handler
            .loop_import(Parameters(LoopImportParams {
                document,
                workdir: workdir.clone(),
                name: None,
            }))
            .await
            .unwrap();
        assert!(!is_err(&imported), "{}", text(&imported));
        let body: serde_json::Value = serde_json::from_str(&raw_text(&imported)).unwrap();
        // The source loop itself is still named "Source Loop" in this same
        // workdir, so decision 4's collision handling suffixes the import.
        assert_eq!(body["name"], "Source Loop (2)");
        let new_loop_id = body["loop_id"].as_str().unwrap().to_string();
        assert_ne!(new_loop_id, "");

        let missing: Vec<&str> = body["nodes_missing_platform"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();
        assert!(missing.contains(&"implementer"));
        assert!(missing.contains(&"committer"));

        let nodes = db.list_loop_nodes_for_loop(&new_loop_id).unwrap();
        assert_eq!(nodes.len(), 3);
        let edges = db.list_loop_edges_for_loop(&new_loop_id).unwrap();
        assert_eq!(edges.len(), 2);
        // Import always creates a new loop, never touching the source.
        assert_eq!(db.list_loops(Some(&workdir), true).unwrap().len(), 2);
    }

    #[tokio::test]
    async fn loop_import_name_param_overrides_document_name() {
        let (dir, _db, handler) = endpoint_test_handler();
        let workdir = dir.path().to_string_lossy().to_string();
        let (source_loop_id, ..) = build_simple_loop(&handler, &workdir, "Source Loop").await;
        let exported = handler
            .loop_export(Parameters(LoopExportParams {
                loop_id: source_loop_id,
                with_models: None,
            }))
            .await
            .unwrap();
        let document: serde_json::Value = serde_json::from_str(&raw_text(&exported)).unwrap();

        let imported = handler
            .loop_import(Parameters(LoopImportParams {
                document,
                workdir,
                name: Some("Custom Name".to_string()),
            }))
            .await
            .unwrap();
        let body: serde_json::Value = serde_json::from_str(&raw_text(&imported)).unwrap();
        assert_eq!(body["name"], "Custom Name");
    }

    /// Decision 4: import never overwrites — a name collision in the target
    /// workdir gets a numeric suffix instead of a refusal or an overwrite.
    #[tokio::test]
    async fn loop_import_dedupes_colliding_name_with_numeric_suffix() {
        let (dir, db, handler) = endpoint_test_handler();
        let workdir = dir.path().to_string_lossy().to_string();
        let (source_loop_id, ..) = build_simple_loop(&handler, &workdir, "Source Loop").await;
        let exported = handler
            .loop_export(Parameters(LoopExportParams {
                loop_id: source_loop_id,
                with_models: None,
            }))
            .await
            .unwrap();
        let document: serde_json::Value = serde_json::from_str(&raw_text(&exported)).unwrap();

        let first = handler
            .loop_import(Parameters(LoopImportParams {
                document: document.clone(),
                workdir: workdir.clone(),
                name: Some("Collide".to_string()),
            }))
            .await
            .unwrap();
        let first_body: serde_json::Value = serde_json::from_str(&raw_text(&first)).unwrap();
        assert_eq!(first_body["name"], "Collide");

        let second = handler
            .loop_import(Parameters(LoopImportParams {
                document,
                workdir: workdir.clone(),
                name: Some("Collide".to_string()),
            }))
            .await
            .unwrap();
        let second_body: serde_json::Value = serde_json::from_str(&raw_text(&second)).unwrap();
        assert_eq!(second_body["name"], "Collide (2)");
        assert_ne!(second_body["loop_id"], first_body["loop_id"]);
        assert_eq!(db.list_loops(Some(&workdir), true).unwrap().len(), 3);
    }

    /// Decision 5's all-or-nothing guarantee: a document whose edge names a
    /// nonexistent node is rejected before anything is written.
    #[tokio::test]
    async fn loop_import_rejects_bad_edge_reference_and_writes_nothing() {
        let (dir, db, handler) = endpoint_test_handler();
        let workdir = dir.path().to_string_lossy().to_string();
        let document = serde_json::json!({
            "format_version": 1,
            "name": "Broken Loop",
            "nodes": [
                {"name": "only", "kind": "check", "position": 1, "config": {"command": "true"}}
            ],
            "edges": [
                {"from_node": "only", "to_node": "ghost", "condition": "always"}
            ],
            "ensembles": []
        });

        let result = handler
            .loop_import(Parameters(LoopImportParams {
                document,
                workdir: workdir.clone(),
                name: None,
            }))
            .await
            .unwrap();
        assert!(is_err(&result));
        assert!(text(&result).contains("ghost"));
        assert!(db.list_loops(Some(&workdir), true).unwrap().is_empty());
    }

    #[tokio::test]
    async fn loop_import_rejects_missing_format_version() {
        let (dir, db, handler) = endpoint_test_handler();
        let workdir = dir.path().to_string_lossy().to_string();
        let document = serde_json::json!({
            "name": "No Version",
            "nodes": [],
            "edges": [],
            "ensembles": []
        });

        let result = handler
            .loop_import(Parameters(LoopImportParams {
                document,
                workdir: workdir.clone(),
                name: None,
            }))
            .await
            .unwrap();
        assert!(is_err(&result));
        assert!(text(&result).contains("format_version"));
        assert!(db.list_loops(Some(&workdir), true).unwrap().is_empty());
    }

    /// Requirement 5, exercised end to end through the MCP surface: export,
    /// import, export again — identical document except the name — for a
    /// loop that includes an ensemble, which must survive as an ensemble
    /// rather than expanded member nodes.
    #[tokio::test]
    async fn loop_export_import_round_trip_with_models_preserves_ensemble() {
        let (dir, _db, handler) = endpoint_test_handler();
        let workdir = dir.path().to_string_lossy().to_string();

        let created = handler
            .loop_create(Parameters(LoopCreateParams {
                name: "Ensemble Loop".to_string(),
                description: Some("Has an ensemble".to_string()),
                workdir: workdir.clone(),
                trigger: None,
            }))
            .await
            .unwrap();
        let loop_id = extract_id(&created, "loop_id");

        let kickoff = handler
            .loop_add_node(Parameters(LoopAddNodeParams {
                spec_id: None,
                loop_id: Some(loop_id.clone()),
                name: "kickoff".to_string(),
                kind: Some("check".to_string()),
                config: Some(check_node_config("true")),
                blueprint: None,
                config_overrides: None,
            }))
            .await
            .unwrap();
        let kickoff_id = extract_id(&kickoff, "node_id");

        let downstream = handler
            .loop_add_node(Parameters(LoopAddNodeParams {
                spec_id: None,
                loop_id: Some(loop_id.clone()),
                name: "downstream".to_string(),
                kind: Some("agent".to_string()),
                config: Some(agent_node_config("claude")),
                blueprint: None,
                config_overrides: None,
            }))
            .await
            .unwrap();
        let downstream_id = extract_id(&downstream, "node_id");

        let ensemble = handler
            .loop_add_ensemble(Parameters(LoopAddEnsembleParams {
                spec_id: None,
                loop_id: Some(loop_id.clone()),
                name: "Proposers".to_string(),
                prompt_template: Some("draft it".to_string()),
                blueprint: None,
                members: Some(vec![
                    EnsembleMemberParams {
                        platform: "openrouter".to_string(),
                        model: Some("model-a".to_string()),
                        prompt_override: None,
                    },
                    EnsembleMemberParams {
                        platform: "openrouter".to_string(),
                        model: Some("model-b".to_string()),
                        prompt_override: None,
                    },
                ]),
                condition: "always".to_string(),
                from_node: kickoff_id.clone(),
                on_pass_to: downstream_id.clone(),
                on_fail_to: None,
                min_pass: None,
                timeout_minutes: None,
                straggler_timeout_minutes: None,
            }))
            .await
            .unwrap();
        assert!(!is_err(&ensemble), "{}", text(&ensemble));

        let first_export = handler
            .loop_export(Parameters(LoopExportParams {
                loop_id,
                with_models: Some(true),
            }))
            .await
            .unwrap();
        let first_doc: serde_json::Value = serde_json::from_str(&raw_text(&first_export)).unwrap();
        assert_eq!(first_doc["ensembles"].as_array().unwrap().len(), 1);
        // The plain node list must exclude the ensemble's member/join nodes.
        assert_eq!(first_doc["nodes"].as_array().unwrap().len(), 2);

        let imported = handler
            .loop_import(Parameters(LoopImportParams {
                document: first_doc.clone(),
                workdir: workdir.clone(),
                name: None,
            }))
            .await
            .unwrap();
        assert!(!is_err(&imported), "{}", text(&imported));
        let imported_body: serde_json::Value = serde_json::from_str(&raw_text(&imported)).unwrap();
        let new_loop_id = imported_body["loop_id"].as_str().unwrap().to_string();
        assert_eq!(imported_body["name"], "Ensemble Loop (2)");
        // Every member carried its platform/model through with_models — no
        // node should be flagged.
        assert!(imported_body["nodes_missing_platform"]
            .as_array()
            .unwrap()
            .is_empty());

        let second_export = handler
            .loop_export(Parameters(LoopExportParams {
                loop_id: new_loop_id,
                with_models: Some(true),
            }))
            .await
            .unwrap();
        let mut second_doc: serde_json::Value =
            serde_json::from_str(&raw_text(&second_export)).unwrap();
        // Identical except the name (decision 4 renamed it on collision).
        second_doc["name"] = first_doc["name"].clone();
        assert_eq!(first_doc, second_doc);
    }

    #[tokio::test]
    async fn loop_preflight_rejects_unknown_named_output_reference() {
        // A structurally valid two-node chain where the downstream node's
        // prompt references `{{output:Ghost}}` — a node that isn't in the
        // graph. Preflight must reject it before spending any probe quota,
        // not leave the marker to surface unsubstituted at runtime (CM1).
        let (dir, _db, handler) = endpoint_test_handler();
        let workdir = dir.path().to_string_lossy().to_string();
        let created = handler
            .loop_create(Parameters(LoopCreateParams {
                name: "Preflight Named Output Loop".to_string(),
                description: None,
                workdir,
                trigger: None,
            }))
            .await
            .unwrap();
        let loop_id = extract_id(&created, "loop_id");

        let alpha = handler
            .loop_add_node(Parameters(LoopAddNodeParams {
                spec_id: None,
                loop_id: Some(loop_id.clone()),
                name: "alpha".to_string(),
                kind: Some("agent".to_string()),
                config: Some(agent_node_config("claude")),
                blueprint: None,
                config_overrides: None,
            }))
            .await
            .unwrap();
        let alpha_id = extract_id(&alpha, "node_id");

        let mut beta_config = agent_node_config("claude");
        beta_config.insert(
            "prompt_template".to_string(),
            serde_json::Value::String("Follow the plan: {{output:Ghost}}".to_string()),
        );
        let beta = handler
            .loop_add_node(Parameters(LoopAddNodeParams {
                spec_id: None,
                loop_id: Some(loop_id.clone()),
                name: "beta".to_string(),
                kind: Some("agent".to_string()),
                config: Some(beta_config),
                blueprint: None,
                config_overrides: None,
            }))
            .await
            .unwrap();
        let beta_id = extract_id(&beta, "node_id");

        let edge = handler
            .loop_add_edge(Parameters(LoopAddEdgeParams {
                spec_id: None,
                loop_id: Some(loop_id.clone()),
                from_node: alpha_id,
                to_node: beta_id,
                condition: "always".to_string(),
                route: None,
            }))
            .await
            .unwrap();
        assert!(!is_err(&edge), "{}", text(&edge));

        let result = handler
            .loop_preflight(Parameters(LoopPreflightParams {
                loop_id: loop_id.clone(),
                timeout_seconds: None,
            }))
            .await
            .unwrap();
        assert!(
            is_err(&result),
            "preflight should reject the unknown named-output reference: {}",
            text(&result)
        );
        let msg = text(&result);
        assert!(
            msg.contains("references unknown node 'Ghost'"),
            "preflight error should name the missing node, got: {msg}"
        );
    }

    #[tokio::test]
    async fn loop_preflight_reports_graph_validation_errors() {
        // Two agent nodes with no edge between them => multiple entry points.
        let (dir, _db, handler) = endpoint_test_handler();
        let workdir = dir.path().to_string_lossy().to_string();
        let created = handler
            .loop_create(Parameters(LoopCreateParams {
                name: "Preflight Graph Loop".to_string(),
                description: None,
                workdir,
                trigger: None,
            }))
            .await
            .unwrap();
        let loop_id = extract_id(&created, "loop_id");

        handler
            .loop_add_node(Parameters(LoopAddNodeParams {
                spec_id: None,
                loop_id: Some(loop_id.clone()),
                name: "alpha".to_string(),
                kind: Some("agent".to_string()),
                config: Some(agent_node_config("claude")),
                blueprint: None,
                config_overrides: None,
            }))
            .await
            .unwrap();
        handler
            .loop_add_node(Parameters(LoopAddNodeParams {
                spec_id: None,
                loop_id: Some(loop_id.clone()),
                name: "beta".to_string(),
                kind: Some("agent".to_string()),
                config: Some(agent_node_config("claude")),
                blueprint: None,
                config_overrides: None,
            }))
            .await
            .unwrap();

        let result = handler
            .loop_preflight(Parameters(LoopPreflightParams {
                loop_id: loop_id.clone(),
                timeout_seconds: None,
            }))
            .await
            .unwrap();
        assert!(
            is_err(&result),
            "preflight should report graph error: {}",
            text(&result)
        );
        let msg = text(&result).to_lowercase();
        assert!(
            msg.contains("entry"),
            "preflight error should mention entry points, got: {msg}"
        );
        assert!(
            msg.contains("alpha") && msg.contains("beta"),
            "preflight error should name the concrete nodes, got: {msg}"
        );
    }

    #[tokio::test]
    async fn loop_update_rejects_unknown_loop_and_requires_a_field() {
        let (dir, db, handler) = endpoint_test_handler();
        let missing = handler
            .loop_update(Parameters(LoopUpdateParams {
                loop_id: "ghost".to_string(),
                name: Some("x".to_string()),
                description: None,
                workdir: None,
                trigger: None,
                on_completed: None,
            }))
            .await
            .unwrap();
        assert!(is_err(&missing));

        let lp = insert_test_loop(&db, dir.path());
        let no_fields = handler
            .loop_update(Parameters(LoopUpdateParams {
                loop_id: lp.id.clone(),
                name: None,
                description: None,
                workdir: None,
                trigger: None,
                on_completed: None,
            }))
            .await
            .unwrap();
        assert!(is_err(&no_fields));
        assert!(text(&no_fields).contains("at least one field"));
    }

    fn insert_test_loop(db: &Database, workdir: &std::path::Path) -> Loop {
        let lp = Loop {
            archived: false,
            paused_by_reconciliation: false,
            id: uuid::Uuid::new_v4().to_string(),
            name: "Test Loop".to_string(),
            description: None,
            workdir: workdir.to_string_lossy().to_string(),
            status: LoopStatus::Draft,
            trigger: None,
            created_at: chrono::Utc::now(),
            started_at: None,
            completed_at: None,
            autorun_at: None,
            auto_continue_at: None,
            auto_continue_action: None,
            active_run_queue_id: None,
            on_completed: None,
        };
        db.insert_loop(&lp).unwrap();
        lp
    }

    fn insert_test_spec(db: &Database, loop_id: &str, position: i64) -> LoopSpec {
        let spec = LoopSpec {
            id: uuid::Uuid::new_v4().to_string(),
            loop_id: Some(loop_id.to_string()),
            name: "Test Spec".to_string(),
            description: Some(valid_spec_description()),
            position,
            parallelizable: false,
            status: LoopSpecStatus::Pending,
            started_at: None,
            completed_at: None,
            spec_start_head: None,
            spec_committed_head: None,
            workdir: None,
            completed_via: None,
            completed_via_reason: None,
            completed_via_at: None,
        };
        db.insert_loop_spec(&spec).unwrap();
        spec
    }

    // ── loop_add_spec / loop_update_spec ─────────────────────────

    #[tokio::test]
    async fn loop_add_spec_happy_path_and_position_conflict() {
        let (dir, db, handler) = endpoint_test_handler();
        let lp = insert_test_loop(&db, dir.path());

        let added = handler
            .loop_add_spec(Parameters(LoopAddSpecParams {
                loop_id: lp.id.clone(),
                name: "First Spec".to_string(),
                description: Some(valid_spec_description()),
                position: 1,
                parallelizable: false,
            }))
            .await
            .unwrap();
        assert!(!is_err(&added), "{}", text(&added));

        let conflict = handler
            .loop_add_spec(Parameters(LoopAddSpecParams {
                loop_id: lp.id.clone(),
                name: "Second Spec".to_string(),
                description: Some(valid_spec_description()),
                position: 1,
                parallelizable: false,
            }))
            .await
            .unwrap();
        assert!(is_err(&conflict));
        assert!(text(&conflict).contains("already has a spec at position"));

        let missing_desc = handler
            .loop_add_spec(Parameters(LoopAddSpecParams {
                loop_id: lp.id.clone(),
                name: "Third Spec".to_string(),
                description: None,
                position: 2,
                parallelizable: false,
            }))
            .await
            .unwrap();
        assert!(is_err(&missing_desc));

        let bad_template = handler
            .loop_add_spec(Parameters(LoopAddSpecParams {
                loop_id: lp.id,
                name: "Fourth Spec".to_string(),
                description: Some("just a sentence".to_string()),
                position: 3,
                parallelizable: false,
            }))
            .await
            .unwrap();
        assert!(is_err(&bad_template));
        assert!(text(&bad_template).contains("missing required sections"));
    }

    #[tokio::test]
    async fn loop_update_spec_renames_and_rejects_no_op() {
        let (dir, db, handler) = endpoint_test_handler();
        let lp = insert_test_loop(&db, dir.path());
        let spec = insert_test_spec(&db, &lp.id, 1);

        let updated = handler
            .loop_update_spec(Parameters(LoopUpdateSpecParams {
                spec_id: spec.id.clone(),
                name: Some("New Name".to_string()),
                description: None,
                position: None,
                parallelizable: None,
            }))
            .await
            .unwrap();
        assert!(!is_err(&updated), "{}", text(&updated));
        assert_eq!(
            db.get_loop_spec(&spec.id).unwrap().unwrap().name,
            "New Name"
        );

        let no_op = handler
            .loop_update_spec(Parameters(LoopUpdateSpecParams {
                spec_id: spec.id.clone(),
                name: None,
                description: None,
                position: None,
                parallelizable: None,
            }))
            .await
            .unwrap();
        assert!(is_err(&no_op));

        let missing = handler
            .loop_update_spec(Parameters(LoopUpdateSpecParams {
                spec_id: "ghost".to_string(),
                name: Some("x".to_string()),
                description: None,
                position: None,
                parallelizable: None,
            }))
            .await
            .unwrap();
        assert!(is_err(&missing));
    }

    // ── spec_create / spec_list / spec_update / spec_set_status / spec_delete

    #[tokio::test]
    async fn spec_create_and_list_and_update() {
        let (_dir, db, handler) = endpoint_test_handler();

        let created = handler
            .spec_create(Parameters(SpecCreateParams {
                name: "Backlog item".to_string(),
                description: valid_spec_description(),
                workdir: Some("/tmp".to_string()),
            }))
            .await
            .unwrap();
        assert!(!is_err(&created), "{}", text(&created));

        let bad = handler
            .spec_create(Parameters(SpecCreateParams {
                name: "Backlog item 2".to_string(),
                description: "too short".to_string(),
                workdir: None,
            }))
            .await
            .unwrap();
        assert!(is_err(&bad));

        let listed = handler
            .spec_list(Parameters(SpecListParams {
                workdir: None,
                status: None,
                unassigned_only: Some(true),
                include_descriptions: None,
            }))
            .await
            .unwrap();
        assert!(text(&listed).contains("Backlog item"));

        let bad_status = handler
            .spec_list(Parameters(SpecListParams {
                workdir: None,
                status: Some("sideways".to_string()),
                unassigned_only: None,
                include_descriptions: None,
            }))
            .await
            .unwrap();
        assert!(is_err(&bad_status));

        let all_specs = db.list_specs(None, None, false).unwrap();
        let spec_id = all_specs
            .iter()
            .find(|s| s.name == "Backlog item")
            .unwrap()
            .id
            .clone();

        let updated = handler
            .spec_update(Parameters(SpecUpdateParams {
                spec_id: spec_id.clone(),
                name: Some("Renamed backlog item".to_string()),
                description: None,
                workdir: None,
            }))
            .await
            .unwrap();
        assert!(!is_err(&updated), "{}", text(&updated));
        assert_eq!(
            db.get_loop_spec(&spec_id).unwrap().unwrap().name,
            "Renamed backlog item"
        );
    }

    #[tokio::test]
    async fn spec_set_status_transitions_and_rejects_bound_spec() {
        let (dir, db, handler) = endpoint_test_handler();

        let standalone = handler
            .spec_create(Parameters(SpecCreateParams {
                name: "Standalone".to_string(),
                description: valid_spec_description(),
                workdir: None,
            }))
            .await
            .unwrap();
        assert!(!is_err(&standalone));
        let spec_id = db
            .list_specs(None, None, false)
            .unwrap()
            .into_iter()
            .find(|s| s.name == "Standalone")
            .unwrap()
            .id;

        let completed = handler
            .spec_set_status(Parameters(SpecSetStatusParams {
                spec_id: spec_id.clone(),
                status: "completed".to_string(),
                reason: "manually closed".to_string(),
            }))
            .await
            .unwrap();
        assert!(!is_err(&completed), "{}", text(&completed));
        assert_eq!(
            db.get_loop_spec(&spec_id).unwrap().unwrap().status,
            LoopSpecStatus::Completed
        );

        let empty_reason = handler
            .spec_set_status(Parameters(SpecSetStatusParams {
                spec_id: spec_id.clone(),
                status: "pending".to_string(),
                reason: "  ".to_string(),
            }))
            .await
            .unwrap();
        assert!(is_err(&empty_reason));

        // A loop-bound spec must be rejected as "not standalone".
        let lp = insert_test_loop(&db, dir.path());
        let bound_spec = insert_test_spec(&db, &lp.id, 1);
        let not_standalone = handler
            .spec_set_status(Parameters(SpecSetStatusParams {
                spec_id: bound_spec.id.clone(),
                status: "completed".to_string(),
                reason: "trying anyway".to_string(),
            }))
            .await
            .unwrap();
        assert!(is_err(&not_standalone));
        assert!(text(&not_standalone).contains("bound to loop"));

        let unknown_status = handler
            .spec_set_status(Parameters(SpecSetStatusParams {
                spec_id: spec_id.clone(),
                status: "sideways".to_string(),
                reason: "x".to_string(),
            }))
            .await
            .unwrap();
        assert!(is_err(&unknown_status));
    }

    #[tokio::test]
    async fn spec_delete_removes_standalone_but_refuses_bound_spec() {
        let (dir, db, handler) = endpoint_test_handler();
        handler
            .spec_create(Parameters(SpecCreateParams {
                name: "Deletable".to_string(),
                description: valid_spec_description(),
                workdir: None,
            }))
            .await
            .unwrap();
        let spec_id = db
            .list_specs(None, None, false)
            .unwrap()
            .into_iter()
            .find(|s| s.name == "Deletable")
            .unwrap()
            .id;

        let deleted = handler
            .spec_delete(Parameters(SpecDeleteParams {
                spec_id: spec_id.clone(),
            }))
            .await
            .unwrap();
        assert!(!is_err(&deleted), "{}", text(&deleted));
        assert!(db.get_loop_spec(&spec_id).unwrap().is_none());

        let missing = handler
            .spec_delete(Parameters(SpecDeleteParams {
                spec_id: "ghost".to_string(),
            }))
            .await
            .unwrap();
        assert!(is_err(&missing));

        let lp = insert_test_loop(&db, dir.path());
        let bound_spec = insert_test_spec(&db, &lp.id, 1);
        let refused = handler
            .spec_delete(Parameters(SpecDeleteParams {
                spec_id: bound_spec.id,
            }))
            .await
            .unwrap();
        assert!(is_err(&refused));
    }

    // ── blueprint_list / blueprint_create / blueprint_delete ─────

    #[tokio::test]
    async fn blueprint_create_list_delete_round_trip() {
        let (_dir, db, handler) = endpoint_test_handler();

        let created = handler
            .blueprint_create(Parameters(BlueprintCreateParams {
                name: "my-agent-bp".to_string(),
                kind: "agent".to_string(),
                config: agent_node_config("claude"),
            }))
            .await
            .unwrap();
        assert!(!is_err(&created), "{}", text(&created));

        let duplicate = handler
            .blueprint_create(Parameters(BlueprintCreateParams {
                name: "my-agent-bp".to_string(),
                kind: "agent".to_string(),
                config: agent_node_config("claude"),
            }))
            .await
            .unwrap();
        assert!(is_err(&duplicate));
        assert!(text(&duplicate).contains("already exists"));

        let bad_kind = handler
            .blueprint_create(Parameters(BlueprintCreateParams {
                name: "other-bp".to_string(),
                kind: "not-a-kind".to_string(),
                config: agent_node_config("claude"),
            }))
            .await
            .unwrap();
        assert!(is_err(&bad_kind));

        let bad_config = handler
            .blueprint_create(Parameters(BlueprintCreateParams {
                name: "no-platform-bp".to_string(),
                kind: "agent".to_string(),
                config: serde_json::Map::new(),
            }))
            .await
            .unwrap();
        assert!(is_err(&bad_config));

        let listed = handler.blueprint_list().await.unwrap();
        assert!(text(&listed).contains("my-agent-bp"));
        assert!(db
            .list_blueprints()
            .unwrap()
            .iter()
            .any(|bp| bp.name == "my-agent-bp"));

        let deleted = handler
            .blueprint_delete(Parameters(BlueprintDeleteParams {
                name: "my-agent-bp".to_string(),
            }))
            .await
            .unwrap();
        assert!(!is_err(&deleted), "{}", text(&deleted));
        assert!(db.get_blueprint_by_name("my-agent-bp").unwrap().is_none());

        let missing = handler
            .blueprint_delete(Parameters(BlueprintDeleteParams {
                name: "ghost-bp".to_string(),
            }))
            .await
            .unwrap();
        assert!(is_err(&missing));
    }

    // ── loop_add_node / loop_update_node ──────────────────────────

    #[tokio::test]
    async fn loop_add_node_happy_path_and_invalid_config() {
        let (dir, db, handler) = endpoint_test_handler();
        let lp = insert_test_loop(&db, dir.path());
        let spec = insert_test_spec(&db, &lp.id, 1);

        let added = handler
            .loop_add_node(Parameters(LoopAddNodeParams {
                spec_id: Some(spec.id.clone()),
                loop_id: None,
                name: "Kickoff".to_string(),
                kind: Some("agent".to_string()),
                config: Some(agent_node_config("claude")),
                blueprint: None,
                config_overrides: None,
            }))
            .await
            .unwrap();
        assert!(!is_err(&added), "{}", text(&added));
        assert_eq!(db.list_loop_nodes(&spec.id).unwrap().len(), 1);

        let missing_both_targets = handler
            .loop_add_node(Parameters(LoopAddNodeParams {
                spec_id: None,
                loop_id: None,
                name: "Orphan".to_string(),
                kind: Some("agent".to_string()),
                config: Some(agent_node_config("claude")),
                blueprint: None,
                config_overrides: None,
            }))
            .await
            .unwrap();
        assert!(is_err(&missing_both_targets));

        let bad_config = handler
            .loop_add_node(Parameters(LoopAddNodeParams {
                spec_id: Some(spec.id.clone()),
                loop_id: None,
                name: "No Platform".to_string(),
                kind: Some("agent".to_string()),
                config: Some(serde_json::Map::new()),
                blueprint: None,
                config_overrides: None,
            }))
            .await
            .unwrap();
        assert!(is_err(&bad_config));

        let join_rejected = handler
            .loop_add_node(Parameters(LoopAddNodeParams {
                spec_id: Some(spec.id),
                loop_id: None,
                name: "Quorum".to_string(),
                kind: Some("join".to_string()),
                config: Some(serde_json::Map::new()),
                blueprint: None,
                config_overrides: None,
            }))
            .await
            .unwrap();
        assert!(is_err(&join_rejected));
        assert!(text(&join_rejected).contains("engine-managed"));
    }

    /// The exact incident shape, through the real `loop_add_node` tool
    /// call: a `prompt` key (instead of `prompt_template`) must be rejected
    /// at write time, naming the correct key, and must never reach the DB.
    #[tokio::test]
    async fn loop_add_node_rejects_prompt_key_naming_prompt_template() {
        let (dir, db, handler) = endpoint_test_handler();
        let lp = insert_test_loop(&db, dir.path());
        let spec = insert_test_spec(&db, &lp.id, 1);

        let mut config = agent_node_config("claude");
        config.insert(
            "prompt".to_string(),
            serde_json::Value::String("implement the spec".to_string()),
        );

        let rejected = handler
            .loop_add_node(Parameters(LoopAddNodeParams {
                spec_id: Some(spec.id.clone()),
                loop_id: None,
                name: "Implementer".to_string(),
                kind: Some("agent".to_string()),
                config: Some(config),
                blueprint: None,
                config_overrides: None,
            }))
            .await
            .unwrap();
        assert!(is_err(&rejected), "{}", text(&rejected));
        assert!(text(&rejected).contains("prompt_template"));
        assert!(db.list_loop_nodes(&spec.id).unwrap().is_empty());
    }

    #[tokio::test]
    async fn loop_update_node_renames_and_validates() {
        let (dir, db, handler) = endpoint_test_handler();
        let lp = insert_test_loop(&db, dir.path());
        let spec = insert_test_spec(&db, &lp.id, 1);
        let node_id = handler
            .loop_add_node(Parameters(LoopAddNodeParams {
                spec_id: Some(spec.id.clone()),
                loop_id: None,
                name: "Node A".to_string(),
                kind: Some("agent".to_string()),
                config: Some(agent_node_config("claude")),
                blueprint: None,
                config_overrides: None,
            }))
            .await
            .unwrap();
        let node_id = extract_id(&node_id, "node_id");

        let renamed = handler
            .loop_update_node(Parameters(LoopUpdateNodeParams {
                node_id: node_id.clone(),
                name: Some("Node A Renamed".to_string()),
                kind: None,
                config: None,
                position: None,
            }))
            .await
            .unwrap();
        assert!(!is_err(&renamed), "{}", text(&renamed));
        assert_eq!(
            db.get_loop_node(&node_id).unwrap().unwrap().name,
            "Node A Renamed"
        );

        let no_op = handler
            .loop_update_node(Parameters(LoopUpdateNodeParams {
                node_id: node_id.clone(),
                name: None,
                kind: None,
                config: None,
                position: None,
            }))
            .await
            .unwrap();
        assert!(is_err(&no_op));

        let missing = handler
            .loop_update_node(Parameters(LoopUpdateNodeParams {
                node_id: "ghost".to_string(),
                name: Some("x".to_string()),
                kind: None,
                config: None,
                position: None,
            }))
            .await
            .unwrap();
        assert!(is_err(&missing));
    }

    // ── loop_add_edge / loop_update_edge ──────────────────────────

    async fn add_agent_node(handler: &TaskTriggerHandler, spec_id: &str, name: &str) -> String {
        let result = handler
            .loop_add_node(Parameters(LoopAddNodeParams {
                spec_id: Some(spec_id.to_string()),
                loop_id: None,
                name: name.to_string(),
                kind: Some("agent".to_string()),
                config: Some(agent_node_config("claude")),
                blueprint: None,
                config_overrides: None,
            }))
            .await
            .unwrap();
        extract_id(&result, "node_id")
    }

    #[tokio::test]
    async fn loop_add_edge_wires_nodes_and_rejects_foreign_node() {
        let (dir, db, handler) = endpoint_test_handler();
        let lp = insert_test_loop(&db, dir.path());
        let spec = insert_test_spec(&db, &lp.id, 1);
        let a = add_agent_node(&handler, &spec.id, "A").await;
        let b = add_agent_node(&handler, &spec.id, "B").await;

        let edge = handler
            .loop_add_edge(Parameters(LoopAddEdgeParams {
                spec_id: Some(spec.id.clone()),
                loop_id: None,
                from_node: a.clone(),
                to_node: b.clone(),
                condition: "always".to_string(),
                route: None,
            }))
            .await
            .unwrap();
        assert!(!is_err(&edge), "{}", text(&edge));
        assert_eq!(db.list_loop_edges(&spec.id).unwrap().len(), 1);

        let foreign = handler
            .loop_add_edge(Parameters(LoopAddEdgeParams {
                spec_id: Some(spec.id.clone()),
                loop_id: None,
                from_node: a.clone(),
                to_node: "not-a-real-node".to_string(),
                condition: "always".to_string(),
                route: None,
            }))
            .await
            .unwrap();
        assert!(is_err(&foreign));

        let bad_condition = handler
            .loop_add_edge(Parameters(LoopAddEdgeParams {
                spec_id: Some(spec.id),
                loop_id: None,
                from_node: a,
                to_node: b,
                condition: "sideways".to_string(),
                route: None,
            }))
            .await
            .unwrap();
        assert!(is_err(&bad_condition));

        let edges = db
            .list_loop_edges(&db.list_loop_specs(&lp.id).unwrap()[0].id)
            .unwrap();
        let edge_id = edges[0].id.clone();
        let updated = handler
            .loop_update_edge(Parameters(LoopUpdateEdgeParams {
                edge_id: edge_id.clone(),
                condition: "fail".to_string(),
                route: None,
                to_node: None,
            }))
            .await
            .unwrap();
        assert!(!is_err(&updated), "{}", text(&updated));

        let same_condition = handler
            .loop_update_edge(Parameters(LoopUpdateEdgeParams {
                edge_id: edge_id.clone(),
                condition: "fail".to_string(),
                route: None,
                to_node: None,
            }))
            .await
            .unwrap();
        assert!(!is_err(&same_condition));
        assert!(text(&same_condition).contains("already uses condition"));

        let missing_edge = handler
            .loop_update_edge(Parameters(LoopUpdateEdgeParams {
                edge_id: "ghost-edge".to_string(),
                condition: "pass".to_string(),
                route: None,
                to_node: None,
            }))
            .await
            .unwrap();
        assert!(is_err(&missing_edge));
    }

    // ── loop_update_edge(to_node) / loop_delete_edge / loop_delete_node ──

    #[tokio::test]
    async fn loop_update_edge_retargets_destination_without_changing_condition() {
        let (dir, db, handler) = endpoint_test_handler();
        let lp = insert_test_loop(&db, dir.path());
        let spec = insert_test_spec(&db, &lp.id, 1);
        let a = add_agent_node(&handler, &spec.id, "A").await;
        let b = add_agent_node(&handler, &spec.id, "B").await;
        let c = add_agent_node(&handler, &spec.id, "C").await;

        let added = handler
            .loop_add_edge(Parameters(LoopAddEdgeParams {
                spec_id: Some(spec.id.clone()),
                loop_id: None,
                from_node: a.clone(),
                to_node: b.clone(),
                condition: "pass".to_string(),
                route: None,
            }))
            .await
            .unwrap();
        assert!(!is_err(&added), "{}", text(&added));
        let edge_id = db.list_loop_edges(&spec.id).unwrap()[0].id.clone();

        let retargeted = handler
            .loop_update_edge(Parameters(LoopUpdateEdgeParams {
                edge_id: edge_id.clone(),
                condition: "pass".to_string(),
                route: None,
                to_node: Some(c.clone()),
            }))
            .await
            .unwrap();
        assert!(!is_err(&retargeted), "{}", text(&retargeted));

        let edge = db.get_loop_edge(&edge_id).unwrap().unwrap();
        assert_eq!(edge.to_node, c, "target changed to the new node");
        assert_eq!(
            edge.condition,
            crate::domain::loops::LoopEdgeCondition::Pass,
            "condition untouched by a target-only update"
        );
        assert_eq!(
            edge.from_node, a,
            "source untouched by a target-only update"
        );
    }

    #[tokio::test]
    async fn loop_update_edge_rejects_retarget_to_a_node_in_another_loop() {
        let (dir, db, handler) = endpoint_test_handler();
        let lp = insert_test_loop(&db, dir.path());
        let spec = insert_test_spec(&db, &lp.id, 1);
        let a = add_agent_node(&handler, &spec.id, "A").await;
        let b = add_agent_node(&handler, &spec.id, "B").await;

        let other_lp = insert_test_loop(&db, dir.path());
        let other_spec = insert_test_spec(&db, &other_lp.id, 1);
        let foreign = add_agent_node(&handler, &other_spec.id, "Foreign").await;

        handler
            .loop_add_edge(Parameters(LoopAddEdgeParams {
                spec_id: Some(spec.id.clone()),
                loop_id: None,
                from_node: a.clone(),
                to_node: b.clone(),
                condition: "pass".to_string(),
                route: None,
            }))
            .await
            .unwrap();
        let edge_id = db.list_loop_edges(&spec.id).unwrap()[0].id.clone();

        let result = handler
            .loop_update_edge(Parameters(LoopUpdateEdgeParams {
                edge_id: edge_id.clone(),
                condition: "pass".to_string(),
                route: None,
                to_node: Some(foreign),
            }))
            .await
            .unwrap();
        assert!(is_err(&result));

        let edge = db.get_loop_edge(&edge_id).unwrap().unwrap();
        assert_eq!(edge.to_node, b, "cross-loop retarget never persisted");
    }

    #[tokio::test]
    async fn loop_delete_edge_removes_a_single_edge() {
        let (dir, db, handler) = endpoint_test_handler();
        let lp = insert_test_loop(&db, dir.path());
        let spec = insert_test_spec(&db, &lp.id, 1);
        let a = add_agent_node(&handler, &spec.id, "A").await;
        let b = add_agent_node(&handler, &spec.id, "B").await;
        handler
            .loop_add_edge(Parameters(LoopAddEdgeParams {
                spec_id: Some(spec.id.clone()),
                loop_id: None,
                from_node: a,
                to_node: b,
                condition: "pass".to_string(),
                route: None,
            }))
            .await
            .unwrap();
        let edge_id = db.list_loop_edges(&spec.id).unwrap()[0].id.clone();

        let deleted = handler
            .loop_delete_edge(Parameters(LoopDeleteEdgeParams {
                edge_id: edge_id.clone(),
            }))
            .await
            .unwrap();
        assert!(!is_err(&deleted), "{}", text(&deleted));
        assert!(db.get_loop_edge(&edge_id).unwrap().is_none());

        let missing = handler
            .loop_delete_edge(Parameters(LoopDeleteEdgeParams {
                edge_id: "ghost-edge".to_string(),
            }))
            .await
            .unwrap();
        assert!(is_err(&missing));
    }

    #[tokio::test]
    async fn loop_delete_node_cascades_to_its_edges() {
        let (dir, db, handler) = endpoint_test_handler();
        let lp = insert_test_loop(&db, dir.path());
        let spec = insert_test_spec(&db, &lp.id, 1);
        let a = add_agent_node(&handler, &spec.id, "A").await;
        let b = add_agent_node(&handler, &spec.id, "B").await;
        let c = add_agent_node(&handler, &spec.id, "C").await;
        for (from, to) in [(a.clone(), b.clone()), (b.clone(), c.clone())] {
            handler
                .loop_add_edge(Parameters(LoopAddEdgeParams {
                    spec_id: Some(spec.id.clone()),
                    loop_id: None,
                    from_node: from,
                    to_node: to,
                    condition: "pass".to_string(),
                    route: None,
                }))
                .await
                .unwrap();
        }
        assert_eq!(db.list_loop_edges(&spec.id).unwrap().len(), 2);

        // `b` (not the entry point — `a` is) sits between two edges; deleting
        // it must drop both, not just the ones naming it as `from_node`.
        let deleted = handler
            .loop_delete_node(Parameters(LoopDeleteNodeParams { node_id: b.clone() }))
            .await
            .unwrap();
        assert!(!is_err(&deleted), "{}", text(&deleted));

        assert!(db.get_loop_node(&b).unwrap().is_none());
        assert!(db.list_loop_edges(&spec.id).unwrap().is_empty());
    }

    #[tokio::test]
    async fn loop_delete_node_rejects_the_graphs_entry_point() {
        let (dir, db, handler) = endpoint_test_handler();
        let lp = insert_test_loop(&db, dir.path());
        let spec = insert_test_spec(&db, &lp.id, 1);
        let a = add_agent_node(&handler, &spec.id, "A").await;
        let b = add_agent_node(&handler, &spec.id, "B").await;
        handler
            .loop_add_edge(Parameters(LoopAddEdgeParams {
                spec_id: Some(spec.id.clone()),
                loop_id: None,
                from_node: a.clone(),
                to_node: b,
                condition: "pass".to_string(),
                route: None,
            }))
            .await
            .unwrap();

        let result = handler
            .loop_delete_node(Parameters(LoopDeleteNodeParams { node_id: a.clone() }))
            .await
            .unwrap();
        assert!(is_err(&result));
        assert!(db.get_loop_node(&a).unwrap().is_some());
    }

    #[tokio::test]
    async fn topology_mutations_are_rejected_while_the_loop_is_running() {
        let (dir, db, handler) = endpoint_test_handler();
        let lp = insert_test_loop(&db, dir.path());
        let spec = insert_test_spec(&db, &lp.id, 1);
        let a = add_agent_node(&handler, &spec.id, "A").await;
        let b = add_agent_node(&handler, &spec.id, "B").await;
        let c = add_agent_node(&handler, &spec.id, "C").await;
        handler
            .loop_add_edge(Parameters(LoopAddEdgeParams {
                spec_id: Some(spec.id.clone()),
                loop_id: None,
                from_node: a,
                to_node: b.clone(),
                condition: "pass".to_string(),
                route: None,
            }))
            .await
            .unwrap();
        let edge_id = db.list_loop_edges(&spec.id).unwrap()[0].id.clone();

        db.update_loop_status(&lp.id, LoopStatus::Running, None, None)
            .unwrap();

        let retarget = handler
            .loop_update_edge(Parameters(LoopUpdateEdgeParams {
                edge_id: edge_id.clone(),
                condition: "pass".to_string(),
                route: None,
                to_node: Some(c),
            }))
            .await
            .unwrap();
        assert!(is_err(&retarget));
        assert!(text(&retarget).contains("running"), "{}", text(&retarget));

        let delete_edge = handler
            .loop_delete_edge(Parameters(LoopDeleteEdgeParams {
                edge_id: edge_id.clone(),
            }))
            .await
            .unwrap();
        assert!(is_err(&delete_edge));
        assert!(
            text(&delete_edge).contains("running"),
            "{}",
            text(&delete_edge)
        );

        let delete_node = handler
            .loop_delete_node(Parameters(LoopDeleteNodeParams { node_id: b.clone() }))
            .await
            .unwrap();
        assert!(is_err(&delete_node));
        assert!(
            text(&delete_node).contains("running"),
            "{}",
            text(&delete_node)
        );

        // Nothing actually mutated while the loop was running.
        assert_eq!(db.get_loop_edge(&edge_id).unwrap().unwrap().to_node, b);
        assert!(db.get_loop_node(&b).unwrap().is_some());
    }

    // ── router nodes (routes + route edges) ──────────────────────────

    fn router_node_config(
        routes: &serde_json::Value,
        fallback: &str,
    ) -> serde_json::Map<String, serde_json::Value> {
        serde_json::json!({ "routes": routes, "fallback": fallback })
            .as_object()
            .unwrap()
            .clone()
    }

    fn two_routes() -> serde_json::Value {
        serde_json::json!([
            { "label": "retry", "description": "Retry the current step." },
            { "label": "escalate", "description": "Hand off to a human." },
        ])
    }

    #[tokio::test]
    async fn loop_add_node_router_created_persisted_and_read_back_with_routes() {
        let (dir, db, handler) = endpoint_test_handler();
        let lp = insert_test_loop(&db, dir.path());
        let spec = insert_test_spec(&db, &lp.id, 1);

        let router_result = handler
            .loop_add_node(Parameters(LoopAddNodeParams {
                spec_id: Some(spec.id.clone()),
                loop_id: None,
                name: "Router".to_string(),
                kind: Some("router".to_string()),
                config: Some(router_node_config(&two_routes(), "retry")),
                blueprint: None,
                config_overrides: None,
            }))
            .await
            .unwrap();
        assert!(!is_err(&router_result), "{}", text(&router_result));
        let router_id = extract_id(&router_result, "node_id");

        let retry_target = add_agent_node(&handler, &spec.id, "Retry target").await;
        let escalate_target = add_agent_node(&handler, &spec.id, "Escalate target").await;

        for (route, target) in [("retry", &retry_target), ("escalate", &escalate_target)] {
            let edge = handler
                .loop_add_edge(Parameters(LoopAddEdgeParams {
                    spec_id: Some(spec.id.clone()),
                    loop_id: None,
                    from_node: router_id.clone(),
                    to_node: target.clone(),
                    condition: "route".to_string(),
                    route: Some(route.to_string()),
                }))
                .await
                .unwrap();
            assert!(!is_err(&edge), "{}", text(&edge));
        }

        let got = handler
            .loop_get(Parameters(LoopGetParams {
                loop_id: lp.id.clone(),
            }))
            .await
            .unwrap();
        assert!(!is_err(&got), "{}", text(&got));
        let json: serde_json::Value = serde_json::from_str(&raw_text(&got)).unwrap();
        let nodes = json["specs"][0]["nodes"].as_array().unwrap();
        let router_json = nodes
            .iter()
            .find(|n| n["id"] == router_id)
            .expect("router node present in loop_get");
        assert_eq!(router_json["kind"], "router");
        let routes = router_json["routes"].as_array().expect("routes array");
        assert_eq!(routes.len(), 2);
        let retry_route = routes
            .iter()
            .find(|r| r["label"] == "retry")
            .expect("retry route present");
        assert_eq!(retry_route["fallback"], true);
        assert_eq!(retry_route["to_node"], retry_target);
        let escalate_route = routes
            .iter()
            .find(|r| r["label"] == "escalate")
            .expect("escalate route present");
        assert_eq!(escalate_route["fallback"], false);
        assert_eq!(escalate_route["to_node"], escalate_target);
    }

    #[tokio::test]
    async fn loop_add_node_router_rejects_fewer_than_two_routes_and_missing_fallback() {
        let (dir, db, handler) = endpoint_test_handler();
        let lp = insert_test_loop(&db, dir.path());
        let spec = insert_test_spec(&db, &lp.id, 1);

        let too_few = handler
            .loop_add_node(Parameters(LoopAddNodeParams {
                spec_id: Some(spec.id.clone()),
                loop_id: None,
                name: "Router".to_string(),
                kind: Some("router".to_string()),
                config: Some(router_node_config(
                    &serde_json::json!([{ "label": "retry", "description": "Retry." }]),
                    "retry",
                )),
                blueprint: None,
                config_overrides: None,
            }))
            .await
            .unwrap();
        assert!(is_err(&too_few));
        assert!(text(&too_few).contains("at least 2 routes"));

        let mut no_fallback_config = router_node_config(&two_routes(), "retry");
        no_fallback_config.remove("fallback");
        let no_fallback = handler
            .loop_add_node(Parameters(LoopAddNodeParams {
                spec_id: Some(spec.id),
                loop_id: None,
                name: "Router".to_string(),
                kind: Some("router".to_string()),
                config: Some(no_fallback_config),
                blueprint: None,
                config_overrides: None,
            }))
            .await
            .unwrap();
        assert!(is_err(&no_fallback));
        assert!(text(&no_fallback).contains("fallback"));
    }

    #[tokio::test]
    async fn loop_add_edge_route_condition_rejects_undeclared_route_and_non_router_source() {
        let (dir, db, handler) = endpoint_test_handler();
        let lp = insert_test_loop(&db, dir.path());
        let spec = insert_test_spec(&db, &lp.id, 1);

        let router_result = handler
            .loop_add_node(Parameters(LoopAddNodeParams {
                spec_id: Some(spec.id.clone()),
                loop_id: None,
                name: "Router".to_string(),
                kind: Some("router".to_string()),
                config: Some(router_node_config(&two_routes(), "retry")),
                blueprint: None,
                config_overrides: None,
            }))
            .await
            .unwrap();
        let router_id = extract_id(&router_result, "node_id");
        let target = add_agent_node(&handler, &spec.id, "Target").await;

        let undeclared = handler
            .loop_add_edge(Parameters(LoopAddEdgeParams {
                spec_id: Some(spec.id.clone()),
                loop_id: None,
                from_node: router_id.clone(),
                to_node: target.clone(),
                condition: "route".to_string(),
                route: Some("nonexistent".to_string()),
            }))
            .await
            .unwrap();
        assert!(is_err(&undeclared));
        assert!(text(&undeclared).contains("undeclared route"));

        let non_router = add_agent_node(&handler, &spec.id, "Non-router source").await;
        let wrong_source = handler
            .loop_add_edge(Parameters(LoopAddEdgeParams {
                spec_id: Some(spec.id),
                loop_id: None,
                from_node: non_router,
                to_node: target,
                condition: "route".to_string(),
                route: Some("retry".to_string()),
            }))
            .await
            .unwrap();
        assert!(is_err(&wrong_source));
        assert!(text(&wrong_source).contains("router node"));
    }

    #[tokio::test]
    async fn loop_update_node_router_enforces_route_coverage_and_edge_consistency() {
        let (dir, db, handler) = endpoint_test_handler();
        let lp = insert_test_loop(&db, dir.path());
        let spec = insert_test_spec(&db, &lp.id, 1);

        let router_result = handler
            .loop_add_node(Parameters(LoopAddNodeParams {
                spec_id: Some(spec.id.clone()),
                loop_id: None,
                name: "Router".to_string(),
                kind: Some("router".to_string()),
                config: Some(router_node_config(&two_routes(), "retry")),
                blueprint: None,
                config_overrides: None,
            }))
            .await
            .unwrap();
        let router_id = extract_id(&router_result, "node_id");
        let retry_target = add_agent_node(&handler, &spec.id, "Retry target").await;

        // Wire only the "retry" route — "escalate" is declared but not yet
        // served by any edge.
        let edge = handler
            .loop_add_edge(Parameters(LoopAddEdgeParams {
                spec_id: Some(spec.id.clone()),
                loop_id: None,
                from_node: router_id.clone(),
                to_node: retry_target,
                condition: "route".to_string(),
                route: Some("retry".to_string()),
            }))
            .await
            .unwrap();
        assert!(!is_err(&edge), "{}", text(&edge));

        // Re-asserting the same routes now that wiring has started must
        // fail: "escalate" has no outgoing edge.
        let no_coverage = handler
            .loop_update_node(Parameters(LoopUpdateNodeParams {
                node_id: router_id.clone(),
                name: None,
                kind: None,
                config: Some(router_node_config(&two_routes(), "retry")),
                position: None,
            }))
            .await
            .unwrap();
        assert!(is_err(&no_coverage));
        assert!(text(&no_coverage).contains("'escalate' has no outgoing edge"));

        // Changing the declared routes so they no longer include "retry"
        // leaves the existing retry-labeled edge dangling on an undeclared
        // route.
        let stale_edge = handler
            .loop_update_node(Parameters(LoopUpdateNodeParams {
                node_id: router_id.clone(),
                name: None,
                kind: None,
                config: Some(router_node_config(
                    &serde_json::json!([
                        { "label": "escalate", "description": "Hand off to a human." },
                        { "label": "abort", "description": "Give up." },
                    ]),
                    "escalate",
                )),
                position: None,
            }))
            .await
            .unwrap();
        assert!(is_err(&stale_edge));
        assert!(text(&stale_edge).contains("undeclared route 'retry'"));
    }

    // ── loop_add_ensemble ──────────────────────────────────────────

    #[tokio::test]
    async fn loop_add_ensemble_happy_path_and_validation_errors() {
        let (dir, db, handler) = endpoint_test_handler();
        let lp = insert_test_loop(&db, dir.path());
        let spec = insert_test_spec(&db, &lp.id, 1);
        let entry = add_agent_node(&handler, &spec.id, "Entry").await;
        let arbiter = add_agent_node(&handler, &spec.id, "Arbiter").await;

        let members = vec![
            crate::daemon::params::EnsembleMemberParams {
                platform: "claude".to_string(),
                model: None,
                prompt_override: None,
            },
            crate::daemon::params::EnsembleMemberParams {
                platform: "opencode".to_string(),
                model: None,
                prompt_override: None,
            },
        ];

        let created = handler
            .loop_add_ensemble(Parameters(LoopAddEnsembleParams {
                spec_id: Some(spec.id.clone()),
                loop_id: None,
                name: "Review Ensemble".to_string(),
                prompt_template: Some("Review {{spec_name}}".to_string()),
                members: Some(members.clone()),
                blueprint: None,
                from_node: entry.clone(),
                condition: "always".to_string(),
                min_pass: Some(2),
                straggler_timeout_minutes: None,
                timeout_minutes: None,
                on_pass_to: arbiter.clone(),
                on_fail_to: None,
            }))
            .await
            .unwrap();
        assert!(!is_err(&created), "{}", text(&created));
        assert_eq!(db.list_ensembles_for_spec(&spec.id).unwrap().len(), 1);

        let missing_prompt = handler
            .loop_add_ensemble(Parameters(LoopAddEnsembleParams {
                spec_id: Some(spec.id.clone()),
                loop_id: None,
                name: "No Prompt".to_string(),
                prompt_template: None,
                members: Some(members.clone()),
                blueprint: None,
                from_node: entry.clone(),
                condition: "always".to_string(),
                min_pass: None,
                straggler_timeout_minutes: None,
                timeout_minutes: None,
                on_pass_to: arbiter.clone(),
                on_fail_to: None,
            }))
            .await
            .unwrap();
        assert!(is_err(&missing_prompt));
        assert!(text(&missing_prompt).contains("prompt_template"));

        let bad_min_pass = handler
            .loop_add_ensemble(Parameters(LoopAddEnsembleParams {
                spec_id: Some(spec.id.clone()),
                loop_id: None,
                name: "Bad Min Pass".to_string(),
                prompt_template: Some("Review".to_string()),
                members: Some(members.clone()),
                blueprint: None,
                from_node: entry.clone(),
                condition: "always".to_string(),
                min_pass: Some(99),
                straggler_timeout_minutes: None,
                timeout_minutes: None,
                on_pass_to: arbiter.clone(),
                on_fail_to: None,
            }))
            .await
            .unwrap();
        assert!(is_err(&bad_min_pass));
        assert!(text(&bad_min_pass).contains("min_pass must be between"));

        let unknown_entry = handler
            .loop_add_ensemble(Parameters(LoopAddEnsembleParams {
                spec_id: Some(spec.id.clone()),
                loop_id: None,
                name: "Bad Entry".to_string(),
                prompt_template: Some("Review".to_string()),
                members: Some(members.clone()),
                blueprint: None,
                from_node: "not-a-node".to_string(),
                condition: "always".to_string(),
                min_pass: None,
                straggler_timeout_minutes: None,
                timeout_minutes: None,
                on_pass_to: arbiter.clone(),
                on_fail_to: None,
            }))
            .await
            .unwrap();
        assert!(is_err(&unknown_entry));
        assert!(text(&unknown_entry).contains("not found"));

        let too_few_members = handler
            .loop_add_ensemble(Parameters(LoopAddEnsembleParams {
                spec_id: Some(spec.id),
                loop_id: None,
                name: "Too Few".to_string(),
                prompt_template: Some("Review".to_string()),
                members: Some(vec![members[0].clone()]),
                blueprint: None,
                from_node: entry,
                condition: "always".to_string(),
                min_pass: None,
                straggler_timeout_minutes: None,
                timeout_minutes: None,
                on_pass_to: arbiter,
                on_fail_to: None,
            }))
            .await
            .unwrap();
        assert!(is_err(&too_few_members));
    }

    // ── loop_get / loop_list ──────────────────────────────────────

    #[tokio::test]
    async fn loop_get_and_loop_list() {
        let (dir, db, handler) = endpoint_test_handler();
        let lp = insert_test_loop(&db, dir.path());

        let got = handler
            .loop_get(Parameters(LoopGetParams {
                loop_id: lp.id.clone(),
            }))
            .await
            .unwrap();
        assert!(!is_err(&got), "{}", text(&got));
        assert!(raw_text(&got).contains(&lp.id));

        let missing = handler
            .loop_get(Parameters(LoopGetParams {
                loop_id: "ghost".to_string(),
            }))
            .await
            .unwrap();
        assert!(is_err(&missing));

        let listed = handler
            .loop_list(Parameters(LoopListParams {
                workdir: None,
                include_archived: None,
            }))
            .await
            .unwrap();
        assert!(raw_text(&listed).contains(&lp.id));

        let filtered_out = handler
            .loop_list(Parameters(LoopListParams {
                workdir: Some("/nowhere".to_string()),
                include_archived: None,
            }))
            .await
            .unwrap();
        assert!(!raw_text(&filtered_out).contains(&lp.id));
    }

    /// `loop_get`'s node JSON must make an agent node's prompt source
    /// visible: `"explicit"` for a `prompt_template`, `"preset"` for a
    /// `prompt_preset`, and — the case this spec exists for — the node
    /// running on the bare fallback nobody chose must be distinguishable
    /// too, as `"default_fallback"`, without requiring a run first.
    #[tokio::test]
    async fn loop_get_reports_prompt_source_for_agent_nodes() {
        let (dir, db, handler) = endpoint_test_handler();
        let lp = insert_test_loop(&db, dir.path());
        let spec = insert_test_spec(&db, &lp.id, 1);

        let mut explicit_config = agent_node_config("claude");
        explicit_config.insert(
            "prompt_template".to_string(),
            serde_json::Value::String("do the thing".to_string()),
        );
        let explicit_id = extract_id(
            &handler
                .loop_add_node(Parameters(LoopAddNodeParams {
                    spec_id: Some(spec.id.clone()),
                    loop_id: None,
                    name: "Explicit".to_string(),
                    kind: Some("agent".to_string()),
                    config: Some(explicit_config),
                    blueprint: None,
                    config_overrides: None,
                }))
                .await
                .unwrap(),
            "node_id",
        );

        let mut preset_config = agent_node_config("claude");
        preset_config.insert(
            "prompt_preset".to_string(),
            serde_json::Value::String("implementer".to_string()),
        );
        let preset_id = extract_id(
            &handler
                .loop_add_node(Parameters(LoopAddNodeParams {
                    spec_id: Some(spec.id.clone()),
                    loop_id: None,
                    name: "Preset".to_string(),
                    kind: Some("agent".to_string()),
                    config: Some(preset_config),
                    blueprint: None,
                    config_overrides: None,
                }))
                .await
                .unwrap(),
            "node_id",
        );

        let default_id = extract_id(
            &handler
                .loop_add_node(Parameters(LoopAddNodeParams {
                    spec_id: Some(spec.id.clone()),
                    loop_id: None,
                    name: "Default".to_string(),
                    kind: Some("agent".to_string()),
                    config: Some(agent_node_config("claude")),
                    blueprint: None,
                    config_overrides: None,
                }))
                .await
                .unwrap(),
            "node_id",
        );

        let got = handler
            .loop_get(Parameters(LoopGetParams {
                loop_id: lp.id.clone(),
            }))
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_str(&raw_text(&got)).unwrap();
        let nodes = json["specs"][0]["nodes"].as_array().unwrap();
        let prompt_source_of = |node_id: &str| -> String {
            nodes
                .iter()
                .find(|n| n["id"] == node_id)
                .unwrap_or_else(|| panic!("node {node_id} not found in {nodes:?}"))["prompt_source"]
                .as_str()
                .unwrap()
                .to_string()
        };
        assert_eq!(prompt_source_of(&explicit_id), "explicit");
        assert_eq!(prompt_source_of(&preset_id), "preset");
        assert_eq!(prompt_source_of(&default_id), "default_fallback");
    }

    /// `loop_audit_node_configs` must find a node that predates write-time
    /// validation and still carries a key its kind never reads — verified
    /// against the exact incident shape (a `prompt` key on an agent node)
    /// and resolving the owning loop through the node's spec, since a
    /// spec-scoped node's own row has no `loop_id`.
    #[tokio::test]
    async fn loop_audit_node_configs_finds_node_with_ignored_prompt_key() {
        let (dir, db, handler) = endpoint_test_handler();
        let lp = insert_test_loop(&db, dir.path());
        let spec = insert_test_spec(&db, &lp.id, 1);

        // Bypass `loop_add_node`'s validation to simulate a node that was
        // written before this spec's check existed — exactly how loop
        // `824de730-7fec-4031-800a-7933d2cf94c1`'s node
        // `d0458fe0-24b8-4a29-8a69-7bb0e742e046` ended up carrying `prompt`.
        let legacy_node = LoopNode {
            id: uuid::Uuid::new_v4().to_string(),
            spec_id: Some(spec.id.clone()),
            loop_id: None,
            name: "Legacy Implementer".to_string(),
            kind: LoopNodeKind::Agent,
            config: serde_json::json!({"platform": "claude", "prompt": "implement the spec"}),
            position: 1,
            created_at: chrono::Utc::now(),
        };
        db.insert_loop_node(&legacy_node).unwrap();

        // A clean node must not show up in the audit.
        let clean_node = insert_named_node(&db, &spec.id, "Clean", 2);

        let audited = handler.loop_audit_node_configs().await.unwrap();
        assert!(!is_err(&audited), "{}", text(&audited));
        let json: serde_json::Value = serde_json::from_str(&raw_text(&audited)).unwrap();
        let flagged = json["flagged_nodes"].as_array().unwrap();
        assert_eq!(flagged.len(), 1);
        let flagged_node = &flagged[0];
        assert_eq!(flagged_node["node_id"], legacy_node.id);
        assert_eq!(flagged_node["loop_id"], lp.id);
        assert_eq!(flagged_node["unknown_keys"], serde_json::json!(["prompt"]));
        assert!(flagged.iter().all(|n| n["node_id"] != clean_node.id));
    }

    // ── loop_node_runs_list / loop_node_run_get ────────────────────

    fn insert_named_node(db: &Database, spec_id: &str, name: &str, position: i64) -> LoopNode {
        let node = LoopNode {
            id: uuid::Uuid::new_v4().to_string(),
            spec_id: Some(spec_id.to_string()),
            loop_id: None,
            name: name.to_string(),
            kind: LoopNodeKind::Agent,
            config: serde_json::json!({"platform": "claude"}),
            position,
            created_at: chrono::Utc::now(),
        };
        db.insert_loop_node(&node).unwrap();
        node
    }

    #[allow(clippy::too_many_arguments)]
    fn insert_finalized_node_run(
        db: &Database,
        loop_id: &str,
        spec_id: &str,
        node_id: &str,
        status: LoopRunStatus,
        output: Option<serde_json::Value>,
        started_at: chrono::DateTime<chrono::Utc>,
    ) -> LoopNodeRun {
        let run = LoopNodeRun {
            id: uuid::Uuid::new_v4().to_string(),
            loop_id: loop_id.to_string(),
            spec_id: spec_id.to_string(),
            node_id: node_id.to_string(),
            status,
            input: None,
            output,
            started_at,
            completed_at: Some(started_at),
            iteration: 1,
            pid: None,
            boot_id: None,
            session_id: Some("ses_test_123".to_string()),
        };
        db.insert_loop_run(&run).unwrap();
        run
    }

    #[tokio::test]
    async fn loop_node_runs_list_defaults_to_most_recent_first_and_resolves_node_name() {
        let (dir, db, handler) = endpoint_test_handler();
        let lp = insert_test_loop(&db, dir.path());
        let spec = insert_test_spec(&db, &lp.id, 1);
        let node_a = insert_named_node(&db, &spec.id, "build", 1);
        let node_b = insert_named_node(&db, &spec.id, "review", 2);

        let now = chrono::Utc::now();
        let older = insert_finalized_node_run(
            &db,
            &lp.id,
            &spec.id,
            &node_a.id,
            LoopRunStatus::Pass,
            Some(serde_json::json!({"reported_output": "ok"})),
            now - chrono::Duration::minutes(10),
        );
        let newer = insert_finalized_node_run(
            &db,
            &lp.id,
            &spec.id,
            &node_b.id,
            LoopRunStatus::Fail,
            Some(serde_json::json!({"stderr": "Error: Unsupported model mimo-auto"})),
            now,
        );

        let listed = handler
            .loop_node_runs_list(Parameters(LoopNodeRunsListParams {
                loop_id: lp.id.clone(),
                spec_id: None,
                node_id: None,
                limit: None,
                offset: None,
                compact: None,
            }))
            .await
            .unwrap();
        assert!(!is_err(&listed), "{}", text(&listed));
        let body = raw_text(&listed);
        let parsed: serde_json::Value = serde_json::from_str(&body).unwrap();
        let runs = parsed["runs"].as_array().unwrap();
        assert_eq!(
            runs[0]["id"], newer.id,
            "the most recently started run must lead the default listing"
        );
        assert_eq!(runs[1]["id"], older.id);
        assert_eq!(runs[0]["node_name"], "review");
        assert_eq!(runs[0]["status"], "fail");
        assert_eq!(runs[0]["session_id"], "ses_test_123");
        assert!(
            runs[0].get("output").is_none(),
            "list responses must omit output — fetch it via loop_node_run_get"
        );

        let unknown_loop = handler
            .loop_node_runs_list(Parameters(LoopNodeRunsListParams {
                loop_id: "ghost".to_string(),
                spec_id: None,
                node_id: None,
                limit: None,
                offset: None,
                compact: None,
            }))
            .await
            .unwrap();
        assert!(is_err(&unknown_loop));
    }

    #[tokio::test]
    async fn loop_node_runs_list_filters_by_spec_and_node_and_respects_limit() {
        let (dir, db, handler) = endpoint_test_handler();
        let lp = insert_test_loop(&db, dir.path());
        let spec1 = insert_test_spec(&db, &lp.id, 1);
        let spec2 = insert_test_spec(&db, &lp.id, 2);
        let node1 = insert_named_node(&db, &spec1.id, "node1", 1);
        let node2 = insert_named_node(&db, &spec2.id, "node2", 1);
        let now = chrono::Utc::now();
        insert_finalized_node_run(
            &db,
            &lp.id,
            &spec1.id,
            &node1.id,
            LoopRunStatus::Pass,
            None,
            now - chrono::Duration::minutes(2),
        );
        let spec2_run = insert_finalized_node_run(
            &db,
            &lp.id,
            &spec2.id,
            &node2.id,
            LoopRunStatus::Pass,
            None,
            now - chrono::Duration::minutes(1),
        );
        insert_finalized_node_run(
            &db,
            &lp.id,
            &spec1.id,
            &node1.id,
            LoopRunStatus::Fail,
            None,
            now,
        );

        let by_spec = handler
            .loop_node_runs_list(Parameters(LoopNodeRunsListParams {
                loop_id: lp.id.clone(),
                spec_id: Some(spec2.id.clone()),
                node_id: None,
                limit: None,
                offset: None,
                compact: None,
            }))
            .await
            .unwrap();
        let by_spec_runs = serde_json::from_str::<serde_json::Value>(&raw_text(&by_spec)).unwrap()
            ["runs"]
            .as_array()
            .unwrap()
            .clone();
        assert_eq!(by_spec_runs.len(), 1);
        assert_eq!(by_spec_runs[0]["id"], spec2_run.id);

        let limited = handler
            .loop_node_runs_list(Parameters(LoopNodeRunsListParams {
                loop_id: lp.id.clone(),
                spec_id: None,
                node_id: None,
                limit: Some(1),
                offset: None,
                compact: None,
            }))
            .await
            .unwrap();
        let limited_runs = serde_json::from_str::<serde_json::Value>(&raw_text(&limited)).unwrap()
            ["runs"]
            .as_array()
            .unwrap()
            .clone();
        assert_eq!(limited_runs.len(), 1, "limit must bound the page size");
        assert_eq!(
            limited_runs[0]["status"], "fail",
            "the single returned run must be the most recent one"
        );
    }

    #[tokio::test]
    async fn loop_node_run_get_returns_full_output_and_redacts_secrets() {
        let (dir, db, handler) = endpoint_test_handler();
        let lp = insert_test_loop(&db, dir.path());
        let spec = insert_test_spec(&db, &lp.id, 1);
        let node = insert_named_node(&db, &spec.id, "resilience", 1);
        let run = insert_finalized_node_run(
            &db,
            &lp.id,
            &spec.id,
            &node.id,
            LoopRunStatus::Fail,
            Some(serde_json::json!({
                "cli": "mimocode",
                "stderr": "Error: Unsupported model mimo-auto",
                "stdout": "leaked token=abcdefghij1234567890 in output",
                "infra_attempt": 1,
                "infra_crash": true,
            })),
            chrono::Utc::now(),
        );

        let got = handler
            .loop_node_run_get(Parameters(LoopNodeRunGetParams {
                run_id: run.id.clone(),
            }))
            .await
            .unwrap();
        assert!(!is_err(&got), "{}", text(&got));
        let body = raw_text(&got);
        assert!(body.contains("Unsupported model mimo-auto"));
        assert!(body.contains("\"node_name\": \"resilience\""));
        assert!(
            body.contains("\"infra_attempt\": 1") && body.contains("\"infra_crash\": true"),
            "B19 infra markers must survive verbatim: {body}"
        );
        assert!(
            !body.contains("abcdefghij1234567890"),
            "a token-shaped value must be redacted, not passed through: {body}"
        );
        assert!(body.contains("[REDACTED]"));

        let missing = handler
            .loop_node_run_get(Parameters(LoopNodeRunGetParams {
                run_id: "ghost-run".to_string(),
            }))
            .await
            .unwrap();
        assert!(is_err(&missing));
    }

    // ── loop_pause / loop_continue ────────────────────────────────

    #[tokio::test]
    async fn loop_pause_rejects_non_running_loop() {
        let (dir, db, handler) = endpoint_test_handler();
        let lp = insert_test_loop(&db, dir.path());
        let result = handler
            .loop_pause(Parameters(LoopPauseParams {
                loop_id: lp.id.clone(),
            }))
            .await
            .unwrap();
        assert!(is_err(&result));
        assert!(text(&result).contains("not running"));
    }

    #[tokio::test]
    async fn loop_continue_requires_a_paused_loop() {
        let (dir, db, handler) = endpoint_test_handler();
        let lp = insert_test_loop(&db, dir.path());

        let not_paused = handler
            .loop_continue(Parameters(LoopContinueParams {
                loop_id: lp.id.clone(),
                action: "retry_current_node".to_string(),
            }))
            .await
            .unwrap();
        assert!(is_err(&not_paused));
        assert!(text(&not_paused).contains("not paused"));

        let missing = handler
            .loop_continue(Parameters(LoopContinueParams {
                loop_id: "ghost".to_string(),
                action: "retry_current_node".to_string(),
            }))
            .await
            .unwrap();
        assert!(is_err(&missing));

        db.update_loop_status(&lp.id, LoopStatus::Paused, None, None)
            .unwrap();
        let bad_action = handler
            .loop_continue(Parameters(LoopContinueParams {
                loop_id: lp.id.clone(),
                action: "sideways".to_string(),
            }))
            .await
            .unwrap();
        assert!(is_err(&bad_action));
        assert!(text(&bad_action).contains("retry_current_node or skip_next_spec"));
    }

    // ── loop_complete_node / loop_report_blocker ──────────────────

    /// Creates a spec + agent node under `loop_id` and a `Running` node-run
    /// against it — the FK-satisfying fixture `loop_complete_node` /
    /// `loop_report_blocker` need (`loop_runs.spec_id`/`node_id` are both
    /// `NOT NULL REFERENCES`).
    fn insert_running_node_run(db: &Database, loop_id: &str) -> LoopNodeRun {
        let spec = insert_test_spec(db, loop_id, 1);
        let node = LoopNode {
            id: uuid::Uuid::new_v4().to_string(),
            spec_id: Some(spec.id.clone()),
            loop_id: None,
            name: "Node".to_string(),
            kind: LoopNodeKind::Agent,
            config: serde_json::json!({"platform": "claude"}),
            position: 1,
            created_at: chrono::Utc::now(),
        };
        db.insert_loop_node(&node).unwrap();
        let run = LoopNodeRun {
            id: uuid::Uuid::new_v4().to_string(),
            loop_id: loop_id.to_string(),
            spec_id: spec.id,
            node_id: node.id,
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
        db.insert_loop_run(&run).unwrap();
        run
    }

    #[tokio::test]
    async fn loop_complete_node_records_result_and_rejects_stale_report() {
        let (dir, db, handler) = endpoint_test_handler();
        let lp = insert_test_loop(&db, dir.path());
        let run = insert_running_node_run(&db, &lp.id);
        let node_id = run.node_id.clone();

        let bad_status = handler
            .loop_complete_node(Parameters(LoopCompleteNodeParams {
                run_id: run.id.clone(),
                node_id: node_id.clone(),
                status: "sideways".to_string(),
                output: "out".to_string(),
                summary: "sum".to_string(),
            }))
            .await
            .unwrap();
        assert!(is_err(&bad_status));

        let ok = handler
            .loop_complete_node(Parameters(LoopCompleteNodeParams {
                run_id: run.id.clone(),
                node_id: node_id.clone(),
                status: "pass".to_string(),
                output: "out".to_string(),
                summary: "sum".to_string(),
            }))
            .await
            .unwrap();
        assert!(!is_err(&ok), "{}", text(&ok));

        // Reporting again against the now-finalized run must be rejected as
        // stale, not silently accepted.
        let stale = handler
            .loop_complete_node(Parameters(LoopCompleteNodeParams {
                run_id: run.id.clone(),
                node_id: node_id.clone(),
                status: "pass".to_string(),
                output: "out2".to_string(),
                summary: "sum2".to_string(),
            }))
            .await
            .unwrap();
        assert!(is_err(&stale));
        assert!(text(&stale).contains("no longer active"));

        let wrong_node = handler
            .loop_complete_node(Parameters(LoopCompleteNodeParams {
                run_id: run.id,
                node_id: "not-the-right-node".to_string(),
                status: "pass".to_string(),
                output: "out".to_string(),
                summary: "sum".to_string(),
            }))
            .await
            .unwrap();
        assert!(is_err(&wrong_node));

        let unknown_run = handler
            .loop_complete_node(Parameters(LoopCompleteNodeParams {
                run_id: "ghost-run".to_string(),
                node_id,
                status: "pass".to_string(),
                output: "out".to_string(),
                summary: "sum".to_string(),
            }))
            .await
            .unwrap();
        assert!(is_err(&unknown_run));
    }

    #[tokio::test]
    async fn loop_report_blocker_pauses_the_loop() {
        let (dir, db, handler) = endpoint_test_handler();
        let lp = insert_test_loop(&db, dir.path());
        let run = insert_running_node_run(&db, &lp.id);
        let node_id = run.node_id.clone();

        let result = handler
            .loop_report_blocker(Parameters(LoopReportBlockerParams {
                run_id: run.id,
                node_id,
                description: "waiting on human input".to_string(),
            }))
            .await
            .unwrap();
        assert!(!is_err(&result), "{}", text(&result));
        assert_eq!(
            db.get_loop(&lp.id).unwrap().unwrap().status,
            LoopStatus::Paused
        );
    }

    /// C19 FR4 (reusing the same blocker mechanism `loop_report_blocker`
    /// uses): a loop paused with an active blocker must not be silently
    /// relaunched via `loop_run` — that's the whole "not started again
    /// until a human clears it" the budget exists for. A plain
    /// `loop_pause` (`Paused`, no blocker) is unaffected — `loop_run`
    /// resuming that is the normal, sanctioned path and must keep working.
    #[tokio::test]
    async fn loop_run_refuses_a_paused_loop_with_an_active_blocker() {
        let (dir, db, handler) = endpoint_test_handler();
        let lp = insert_test_loop(&db, dir.path());
        let run = insert_running_node_run(&db, &lp.id);
        let node_id = run.node_id.clone();

        let blocked = handler
            .loop_report_blocker(Parameters(LoopReportBlockerParams {
                run_id: run.id,
                node_id,
                description: "waiting on human input".to_string(),
            }))
            .await
            .unwrap();
        assert!(!is_err(&blocked), "{}", text(&blocked));

        let result = handler
            .loop_run(Parameters(LoopRunParams {
                loop_id: lp.id.clone(),
                queue_id: None,
                workdir: None,
            }))
            .await
            .unwrap();
        assert!(is_err(&result), "a blocked loop must refuse loop_run");
        assert!(text(&result).contains("blocked"), "{}", text(&result));
        assert_eq!(
            db.get_loop(&lp.id).unwrap().unwrap().status,
            LoopStatus::Paused,
            "the refusal must not itself change the loop's status"
        );
    }

    /// The defect this closes (2026-08-06, loop 824de730): a spec that dies
    /// because a FAILING node has no outgoing edge left no blocker anywhere
    /// a human could see from `loop_list` — the engine now derives one onto
    /// the terminating run (`LoopEngine::record_terminal_blocker`), and
    /// `loop_list` picks it up through the unchanged `loop_run_blocker`
    /// read path, with no MCP-side change required.
    #[tokio::test]
    async fn loop_list_reports_blocked_for_a_spec_that_dead_ends_on_a_failing_node() {
        let (dir, db, handler) = endpoint_test_handler();
        let lp = insert_test_loop(&db, dir.path());
        let spec = insert_test_spec(&db, &lp.id, 1);
        db.insert_loop_node(&LoopNode {
            id: "dead-end".to_string(),
            spec_id: Some(spec.id.clone()),
            loop_id: None,
            name: "dead-end".to_string(),
            kind: LoopNodeKind::Check,
            config: serde_json::json!({
                "command": "exit 1",
                "success_condition": "exit_code_0",
            }),
            position: 1,
            created_at: chrono::Utc::now(),
        })
        .unwrap();
        // No outgoing edge from "dead-end" for either status — the spec
        // terminates right there.

        handler
            .loop_engine
            .run_loop(lp.id.clone(), None, None)
            .await
            .unwrap();

        assert_eq!(
            db.get_loop_spec(&spec.id).unwrap().unwrap().status,
            LoopSpecStatus::Failed
        );

        let listed = handler
            .loop_list(Parameters(LoopListParams {
                workdir: None,
                include_archived: None,
            }))
            .await
            .unwrap();
        assert!(!is_err(&listed), "{}", text(&listed));
        let value: serde_json::Value = serde_json::from_str(&raw_text(&listed)).unwrap();
        let entry = value
            .as_array()
            .unwrap()
            .iter()
            .find(|v| v["id"] == lp.id)
            .expect("loop must appear in loop_list");
        assert_eq!(entry["blocked"], serde_json::json!(true));
        let blocker = entry["blocker"].as_str().expect("blocker must be a string");
        assert!(blocker.contains("dead-end"), "blocker: {blocker}");
    }

    // ── loop_copy_node / loop_copy_ensemble ───────────────────────

    #[tokio::test]
    async fn loop_copy_node_unwired_and_wired() {
        let (dir, db, handler) = endpoint_test_handler();
        let lp = insert_test_loop(&db, dir.path());
        let spec = insert_test_spec(&db, &lp.id, 1);
        let source = add_agent_node(&handler, &spec.id, "Source").await;
        let entry = add_agent_node(&handler, &spec.id, "Entry").await;
        let exit = add_agent_node(&handler, &spec.id, "Exit").await;

        let unwired = handler
            .loop_copy_node(Parameters(LoopCopyNodeParams {
                source_node_id: source.clone(),
                spec_id: None,
                loop_id: None,
                name: Some("Source Copy".to_string()),
                config_overrides: None,
                entry_from_node: None,
                entry_condition: None,
                on_pass_to: None,
                on_fail_to: None,
            }))
            .await
            .unwrap();
        assert!(!is_err(&unwired), "{}", text(&unwired));
        let value: serde_json::Value = serde_json::from_str(&raw_text(&unwired)).unwrap();
        assert_eq!(value["wired"], false);
        assert_eq!(db.list_loop_nodes(&spec.id).unwrap().len(), 4);

        let wired = handler
            .loop_copy_node(Parameters(LoopCopyNodeParams {
                source_node_id: source,
                spec_id: None,
                loop_id: None,
                name: Some("Source Copy 2".to_string()),
                config_overrides: None,
                entry_from_node: Some(entry),
                entry_condition: Some("always".to_string()),
                on_pass_to: Some(exit),
                on_fail_to: None,
            }))
            .await
            .unwrap();
        assert!(!is_err(&wired), "{}", text(&wired));
        let value: serde_json::Value = serde_json::from_str(&raw_text(&wired)).unwrap();
        assert_eq!(value["wired"], true);

        let missing_source = handler
            .loop_copy_node(Parameters(LoopCopyNodeParams {
                source_node_id: "ghost-node".to_string(),
                spec_id: None,
                loop_id: None,
                name: None,
                config_overrides: None,
                entry_from_node: None,
                entry_condition: None,
                on_pass_to: None,
                on_fail_to: None,
            }))
            .await
            .unwrap();
        assert!(is_err(&missing_source));
    }

    #[tokio::test]
    async fn loop_copy_ensemble_duplicates_members_and_quorum() {
        let (dir, db, handler) = endpoint_test_handler();
        let lp = insert_test_loop(&db, dir.path());
        let spec = insert_test_spec(&db, &lp.id, 1);
        let entry = add_agent_node(&handler, &spec.id, "Entry").await;
        let arbiter = add_agent_node(&handler, &spec.id, "Arbiter").await;

        let created = handler
            .loop_add_ensemble(Parameters(LoopAddEnsembleParams {
                spec_id: Some(spec.id.clone()),
                loop_id: None,
                name: "Source Ensemble".to_string(),
                prompt_template: Some("Review {{spec_name}}".to_string()),
                members: Some(vec![
                    crate::daemon::params::EnsembleMemberParams {
                        platform: "claude".to_string(),
                        model: None,
                        prompt_override: None,
                    },
                    crate::daemon::params::EnsembleMemberParams {
                        platform: "opencode".to_string(),
                        model: None,
                        prompt_override: None,
                    },
                ]),
                blueprint: None,
                from_node: entry,
                condition: "always".to_string(),
                min_pass: None,
                straggler_timeout_minutes: None,
                timeout_minutes: None,
                on_pass_to: arbiter,
                on_fail_to: None,
            }))
            .await
            .unwrap();
        let source_ensemble_id = extract_id(&created, "ensemble_id");

        let copied = handler
            .loop_copy_ensemble(Parameters(LoopCopyEnsembleParams {
                source_ensemble_id: source_ensemble_id.clone(),
                spec_id: None,
                loop_id: None,
                name: Some("Copied Ensemble".to_string()),
                prompt_template: None,
                members: None,
                min_pass: None,
                timeout_minutes: None,
                straggler_timeout_minutes: None,
                from_node: None,
                condition: None,
                on_pass_to: None,
                on_fail_to: None,
            }))
            .await
            .unwrap();
        assert!(!is_err(&copied), "{}", text(&copied));
        let copied_ensemble_id = extract_id(&copied, "ensemble_id");
        assert_ne!(copied_ensemble_id, source_ensemble_id);
        let copied_details = db
            .get_ensemble_details(&copied_ensemble_id)
            .unwrap()
            .unwrap();
        assert_eq!(copied_details.ensemble.name, "Copied Ensemble");
        assert_eq!(copied_details.members.len(), 2);

        let missing_source = handler
            .loop_copy_ensemble(Parameters(LoopCopyEnsembleParams {
                source_ensemble_id: "ghost-ensemble".to_string(),
                spec_id: None,
                loop_id: None,
                name: None,
                prompt_template: None,
                members: None,
                min_pass: None,
                timeout_minutes: None,
                straggler_timeout_minutes: None,
                from_node: None,
                condition: None,
                on_pass_to: None,
                on_fail_to: None,
            }))
            .await
            .unwrap();
        assert!(is_err(&missing_source));
    }

    /// A member's `prompt_override` replaces the shared `prompt_template` for
    /// that member only — everything else (platform/model semantics, the
    /// other members, `loop_get`'s prompt_source, and `loop_copy_ensemble`
    /// carrying it forward) keeps working exactly as before this field
    /// existed.
    #[tokio::test]
    async fn loop_add_ensemble_member_prompt_override_replaces_shared_prompt_for_that_member_only()
    {
        let (dir, db, handler) = endpoint_test_handler();
        let lp = insert_test_loop(&db, dir.path());
        let spec = insert_test_spec(&db, &lp.id, 1);
        let entry = add_agent_node(&handler, &spec.id, "Entry").await;
        let arbiter = add_agent_node(&handler, &spec.id, "Arbiter").await;

        let created = handler
            .loop_add_ensemble(Parameters(LoopAddEnsembleParams {
                spec_id: Some(spec.id.clone()),
                loop_id: None,
                name: "Panel".to_string(),
                prompt_template: Some("shared review prompt".to_string()),
                members: Some(vec![
                    crate::daemon::params::EnsembleMemberParams {
                        platform: "claude".to_string(),
                        model: None,
                        prompt_override: Some("review for security issues".to_string()),
                    },
                    crate::daemon::params::EnsembleMemberParams {
                        platform: "codex".to_string(),
                        model: None,
                        prompt_override: None,
                    },
                ]),
                blueprint: None,
                from_node: entry,
                condition: "always".to_string(),
                min_pass: Some(2),
                straggler_timeout_minutes: None,
                timeout_minutes: None,
                on_pass_to: arbiter,
                on_fail_to: None,
            }))
            .await
            .unwrap();
        assert!(!is_err(&created), "{}", text(&created));
        let ensemble_id = extract_id(&created, "ensemble_id");
        let details = db.get_ensemble_details(&ensemble_id).unwrap().unwrap();

        // Platform/model are untouched by the override — it's an independent
        // knob, not a replacement for them.
        assert_eq!(details.members[0].platform, "claude");
        assert_eq!(
            details.members[0].prompt_override.as_deref(),
            Some("review for security issues")
        );
        assert_eq!(details.members[1].prompt_override, None);

        let overridden_node = db
            .get_loop_node(&details.members[0].node_id)
            .unwrap()
            .unwrap();
        assert_eq!(
            overridden_node
                .config
                .get("prompt_template")
                .and_then(|v| v.as_str()),
            Some("review for security issues")
        );
        let shared_node = db
            .get_loop_node(&details.members[1].node_id)
            .unwrap()
            .unwrap();
        assert_eq!(
            shared_node
                .config
                .get("prompt_template")
                .and_then(|v| v.as_str()),
            Some("shared review prompt")
        );

        // loop_get's ensemble view distinguishes shared vs. override per
        // member without the caller having to diff node config themselves.
        let ensembles_json = crate::daemon::handler::ensemble_details_json(&details);
        let members_json = ensembles_json["members"].as_array().unwrap();
        assert_eq!(members_json[0]["prompt_source"], "override");
        assert_eq!(members_json[1]["prompt_source"], "shared");

        // Copying without replacing members carries each member's own
        // override (or lack of one) forward into the copy.
        let copied = handler
            .loop_copy_ensemble(Parameters(LoopCopyEnsembleParams {
                source_ensemble_id: ensemble_id.clone(),
                spec_id: None,
                loop_id: None,
                name: Some("Panel copy".to_string()),
                prompt_template: None,
                members: None,
                min_pass: None,
                timeout_minutes: None,
                straggler_timeout_minutes: None,
                from_node: None,
                condition: None,
                on_pass_to: None,
                on_fail_to: None,
            }))
            .await
            .unwrap();
        assert!(!is_err(&copied), "{}", text(&copied));
        let copied_ensemble_id = extract_id(&copied, "ensemble_id");
        let copied_details = db
            .get_ensemble_details(&copied_ensemble_id)
            .unwrap()
            .unwrap();
        assert_eq!(
            copied_details.members[0].prompt_override.as_deref(),
            Some("review for security issues")
        );
        assert_eq!(copied_details.members[1].prompt_override, None);
    }

    // ── loop_update_ensemble ────────────────────────────────────────

    #[tokio::test]
    async fn loop_update_ensemble_prompt_members_join_and_wiring() {
        let (dir, db, handler) = endpoint_test_handler();
        let lp = insert_test_loop(&db, dir.path());
        let spec = insert_test_spec(&db, &lp.id, 1);
        let entry = add_agent_node(&handler, &spec.id, "Entry").await;
        let arbiter = add_agent_node(&handler, &spec.id, "Arbiter").await;
        let alt_exit = add_agent_node(&handler, &spec.id, "AltExit").await;

        let created = handler
            .loop_add_ensemble(Parameters(LoopAddEnsembleParams {
                spec_id: Some(spec.id.clone()),
                loop_id: None,
                name: "Ensemble".to_string(),
                prompt_template: Some("Original prompt".to_string()),
                members: Some(vec![
                    crate::daemon::params::EnsembleMemberParams {
                        platform: "claude".to_string(),
                        model: None,
                        prompt_override: None,
                    },
                    crate::daemon::params::EnsembleMemberParams {
                        platform: "opencode".to_string(),
                        model: None,
                        prompt_override: None,
                    },
                ]),
                blueprint: None,
                from_node: entry,
                condition: "always".to_string(),
                min_pass: Some(2),
                straggler_timeout_minutes: None,
                timeout_minutes: None,
                on_pass_to: arbiter,
                on_fail_to: None,
            }))
            .await
            .unwrap();
        let ensemble_id = extract_id(&created, "ensemble_id");

        // No fields at all -> rejected.
        let no_op = handler
            .loop_update_ensemble(Parameters(LoopUpdateEnsembleParams {
                ensemble_id: ensemble_id.clone(),
                prompt_template: None,
                members: None,
                min_pass: None,
                straggler_timeout_minutes: None,
                timeout_minutes: None,
                on_pass_to: None,
                on_fail_to: None,
            }))
            .await
            .unwrap();
        assert!(is_err(&no_op));
        assert!(text(&no_op).contains("at least one field"));

        // Prompt-only update, propagated to existing members without a resize.
        let prompt_updated = handler
            .loop_update_ensemble(Parameters(LoopUpdateEnsembleParams {
                ensemble_id: ensemble_id.clone(),
                prompt_template: Some("New prompt".to_string()),
                members: None,
                min_pass: None,
                straggler_timeout_minutes: None,
                timeout_minutes: None,
                on_pass_to: None,
                on_fail_to: None,
            }))
            .await
            .unwrap();
        assert!(!is_err(&prompt_updated), "{}", text(&prompt_updated));
        let details = db.get_ensemble_details(&ensemble_id).unwrap().unwrap();
        assert_eq!(details.ensemble.prompt_template, "New prompt");

        // Grow membership 2 -> 3.
        let grown = handler
            .loop_update_ensemble(Parameters(LoopUpdateEnsembleParams {
                ensemble_id: ensemble_id.clone(),
                prompt_template: None,
                members: Some(vec![
                    crate::daemon::params::EnsembleMemberParams {
                        platform: "claude".to_string(),
                        model: None,
                        prompt_override: None,
                    },
                    crate::daemon::params::EnsembleMemberParams {
                        platform: "opencode".to_string(),
                        model: None,
                        prompt_override: None,
                    },
                    crate::daemon::params::EnsembleMemberParams {
                        platform: "gemini".to_string(),
                        model: None,
                        prompt_override: Some("Review only for test coverage gaps.".to_string()),
                    },
                ]),
                min_pass: None,
                straggler_timeout_minutes: None,
                timeout_minutes: None,
                on_pass_to: None,
                on_fail_to: None,
            }))
            .await
            .unwrap();
        assert!(!is_err(&grown), "{}", text(&grown));
        let details = db.get_ensemble_details(&ensemble_id).unwrap().unwrap();
        assert_eq!(details.members.len(), 3);
        // The new member's own prompt_override is persisted on the ensemble
        // row and propagated into its node config as the effective prompt,
        // while the other two still carry no override.
        assert_eq!(details.members[0].prompt_override, None);
        assert_eq!(details.members[1].prompt_override, None);
        assert_eq!(
            details.members[2].prompt_override.as_deref(),
            Some("Review only for test coverage gaps.")
        );
        let gemini_node = db
            .get_loop_node(&details.members[2].node_id)
            .unwrap()
            .unwrap();
        assert_eq!(
            gemini_node
                .config
                .get("prompt_template")
                .and_then(|v| v.as_str()),
            Some("Review only for test coverage gaps.")
        );

        // Shrink membership 3 -> 2.
        let shrunk = handler
            .loop_update_ensemble(Parameters(LoopUpdateEnsembleParams {
                ensemble_id: ensemble_id.clone(),
                prompt_template: None,
                members: Some(vec![
                    crate::daemon::params::EnsembleMemberParams {
                        platform: "claude".to_string(),
                        model: None,
                        prompt_override: None,
                    },
                    crate::daemon::params::EnsembleMemberParams {
                        platform: "opencode".to_string(),
                        model: None,
                        prompt_override: None,
                    },
                ]),
                min_pass: None,
                straggler_timeout_minutes: None,
                timeout_minutes: None,
                on_pass_to: None,
                on_fail_to: None,
            }))
            .await
            .unwrap();
        assert!(!is_err(&shrunk), "{}", text(&shrunk));
        let details = db.get_ensemble_details(&ensemble_id).unwrap().unwrap();
        assert_eq!(details.members.len(), 2);

        // min_pass out of bounds.
        let bad_min_pass = handler
            .loop_update_ensemble(Parameters(LoopUpdateEnsembleParams {
                ensemble_id: ensemble_id.clone(),
                prompt_template: None,
                members: None,
                min_pass: Some(99),
                straggler_timeout_minutes: None,
                timeout_minutes: None,
                on_pass_to: None,
                on_fail_to: None,
            }))
            .await
            .unwrap();
        assert!(is_err(&bad_min_pass));

        // Negative straggler timeout.
        let bad_straggler = handler
            .loop_update_ensemble(Parameters(LoopUpdateEnsembleParams {
                ensemble_id: ensemble_id.clone(),
                prompt_template: None,
                members: None,
                min_pass: None,
                straggler_timeout_minutes: Some(Some(-1)),
                timeout_minutes: None,
                on_pass_to: None,
                on_fail_to: None,
            }))
            .await
            .unwrap();
        assert!(is_err(&bad_straggler));

        // Re-wire on_pass_to to a new valid target.
        let rewired = handler
            .loop_update_ensemble(Parameters(LoopUpdateEnsembleParams {
                ensemble_id: ensemble_id.clone(),
                prompt_template: None,
                members: None,
                min_pass: Some(2),
                straggler_timeout_minutes: None,
                timeout_minutes: None,
                on_pass_to: Some(alt_exit.clone()),
                on_fail_to: None,
            }))
            .await
            .unwrap();
        assert!(!is_err(&rewired), "{}", text(&rewired));
        let details = db.get_ensemble_details(&ensemble_id).unwrap().unwrap();
        assert_eq!(details.ensemble.on_pass_to, alt_exit);

        // Unknown on_pass_to target.
        let bad_target = handler
            .loop_update_ensemble(Parameters(LoopUpdateEnsembleParams {
                ensemble_id: ensemble_id.clone(),
                prompt_template: None,
                members: None,
                min_pass: None,
                straggler_timeout_minutes: None,
                timeout_minutes: None,
                on_pass_to: Some("not-a-node".to_string()),
                on_fail_to: None,
            }))
            .await
            .unwrap();
        assert!(is_err(&bad_target));

        let missing_ensemble = handler
            .loop_update_ensemble(Parameters(LoopUpdateEnsembleParams {
                ensemble_id: "ghost-ensemble".to_string(),
                prompt_template: Some("x".to_string()),
                members: None,
                min_pass: None,
                straggler_timeout_minutes: None,
                timeout_minutes: None,
                on_pass_to: None,
                on_fail_to: None,
            }))
            .await
            .unwrap();
        assert!(is_err(&missing_ensemble));
    }

    // ── sync_* / intelligence_* / get_tools / project_* ────────────

    /// RAII guard: sets the real `CANOPY_AGENT_ID` env var, which
    /// `resolve_sync_agent_id` falls back to when no request `Parts` header
    /// is present (as in these direct-call tests). Safe under `cargo
    /// nextest` (one process per test).
    struct AgentIdVar {
        prev: Option<std::ffi::OsString>,
    }

    impl AgentIdVar {
        fn set(id: &str) -> Self {
            let prev = std::env::var_os(crate::shared::sync_identity::CANOPY_AGENT_ID_ENV);
            unsafe {
                std::env::set_var(crate::shared::sync_identity::CANOPY_AGENT_ID_ENV, id);
            }
            AgentIdVar { prev }
        }
    }

    impl Drop for AgentIdVar {
        fn drop(&mut self) {
            let key = crate::shared::sync_identity::CANOPY_AGENT_ID_ENV;
            match &self.prev {
                Some(v) => unsafe { std::env::set_var(key, v) },
                None => unsafe { std::env::remove_var(key) },
            }
        }
    }

    // ── intelligence_upsert / intelligence_search / intelligence_graph_walk

    #[tokio::test]
    async fn intelligence_upsert_search_and_graph_walk() {
        let (_dir, _db, handler) = endpoint_test_handler();
        let _agent_guard = AgentIdVar::set("agent-intel-1");

        let created = handler
            .intelligence_upsert(
                Parameters(IntelligenceUpsertParams {
                    node_data: IntelligenceNodeParams {
                        id: Some("fact-1".to_string()),
                        kind: "fact".to_string(),
                        title: "Rust is memory safe".to_string(),
                        body: "Ownership rules prevent data races.".to_string(),
                        metadata: None,
                        project_hash: None,
                        session_id: None,
                        relations: None,
                    },
                }),
                OptionalExtension(None),
            )
            .await
            .unwrap();
        assert!(!is_err(&created), "{}", text(&created));

        let linked = handler
            .intelligence_upsert(
                Parameters(IntelligenceUpsertParams {
                    node_data: IntelligenceNodeParams {
                        id: Some("fact-2".to_string()),
                        kind: "fact".to_string(),
                        title: "Ownership rules".to_string(),
                        body: "One owner at a time.".to_string(),
                        metadata: None,
                        project_hash: None,
                        session_id: None,
                        relations: Some(vec![IntelligenceRelationParams {
                            to_node_id: "fact-1".to_string(),
                            relation: "supports".to_string(),
                            weight: Some(1.0),
                        }]),
                    },
                }),
                OptionalExtension(None),
            )
            .await
            .unwrap();
        assert!(!is_err(&linked), "{}", text(&linked));

        let searched = handler
            .intelligence_search(
                Parameters(IntelligenceSearchParams {
                    query: "memory safe".to_string(),
                    kind: Some("fact".to_string()),
                    limit: Some(5),
                }),
                OptionalExtension(None),
            )
            .await
            .unwrap();
        assert!(raw_text(&searched).contains("fact-1"));

        let walked = handler
            .intelligence_graph_walk(Parameters(IntelligenceGraphWalkParams {
                node_id: "fact-2".to_string(),
                depth: Some(2),
            }))
            .await
            .unwrap();
        assert!(!is_err(&walked), "{}", text(&walked));
        assert!(raw_text(&walked).contains("fact-1"));

        let missing_root = handler
            .intelligence_graph_walk(Parameters(IntelligenceGraphWalkParams {
                node_id: "ghost-node".to_string(),
                depth: None,
            }))
            .await
            .unwrap();
        assert!(is_err(&missing_root));
    }

    // ── intelligence_delete_node / intelligence_delete_relation ──────────

    async fn upsert_intel_fact(
        handler: &TaskTriggerHandler,
        id: &str,
        title: &str,
        relations: Option<Vec<IntelligenceRelationParams>>,
    ) {
        let result = handler
            .intelligence_upsert(
                Parameters(IntelligenceUpsertParams {
                    node_data: IntelligenceNodeParams {
                        id: Some(id.to_string()),
                        kind: "fact".to_string(),
                        title: title.to_string(),
                        body: "Test body".to_string(),
                        metadata: None,
                        project_hash: None,
                        session_id: None,
                        relations,
                    },
                }),
                OptionalExtension(None),
            )
            .await
            .unwrap();
        assert!(!is_err(&result), "{}", text(&result));
    }

    #[tokio::test]
    async fn intelligence_delete_node_removes_node_and_reports_relations() {
        let (_dir, _db, handler) = endpoint_test_handler();
        let _agent_guard = AgentIdVar::set("agent-intel-delete-1");

        upsert_intel_fact(&handler, "delete-a", "Node A", None).await;
        upsert_intel_fact(
            &handler,
            "delete-b",
            "Node B",
            Some(vec![IntelligenceRelationParams {
                to_node_id: "delete-a".to_string(),
                relation: "supports".to_string(),
                weight: None,
            }]),
        )
        .await;
        upsert_intel_fact(
            &handler,
            "delete-c",
            "Node C",
            Some(vec![IntelligenceRelationParams {
                to_node_id: "delete-b".to_string(),
                relation: "supports".to_string(),
                weight: None,
            }]),
        )
        .await;

        // "delete-b" sits in the middle of a -> b, b -> c: two edges touch it.
        let deleted = handler
            .intelligence_delete_node(
                Parameters(IntelligenceDeleteNodeParams {
                    node_id: "delete-b".to_string(),
                    project_hash: None,
                }),
                OptionalExtension(None),
            )
            .await
            .unwrap();
        assert!(!is_err(&deleted), "{}", text(&deleted));
        let body = raw_text(&deleted);
        assert!(body.contains("delete-b"));
        assert!(body.contains("\"relations_removed\": 2"));

        let searched = handler
            .intelligence_search(
                Parameters(IntelligenceSearchParams {
                    query: "Node B".to_string(),
                    kind: None,
                    limit: Some(5),
                }),
                OptionalExtension(None),
            )
            .await
            .unwrap();
        assert!(!raw_text(&searched).contains("delete-b"));

        // Neither surviving neighbor's graph walk should error or reference
        // the removed middle node.
        let from_a = handler
            .intelligence_graph_walk(Parameters(IntelligenceGraphWalkParams {
                node_id: "delete-a".to_string(),
                depth: Some(3),
            }))
            .await
            .unwrap();
        assert!(!is_err(&from_a), "{}", text(&from_a));
        assert!(!raw_text(&from_a).contains("delete-b"));

        let from_c = handler
            .intelligence_graph_walk(Parameters(IntelligenceGraphWalkParams {
                node_id: "delete-c".to_string(),
                depth: Some(3),
            }))
            .await
            .unwrap();
        assert!(!is_err(&from_c), "{}", text(&from_c));
        assert!(!raw_text(&from_c).contains("delete-b"));
    }

    #[tokio::test]
    async fn intelligence_delete_node_missing_id_returns_error_not_silent_success() {
        let (_dir, _db, handler) = endpoint_test_handler();
        let _agent_guard = AgentIdVar::set("agent-intel-delete-2");

        let deleted = handler
            .intelligence_delete_node(
                Parameters(IntelligenceDeleteNodeParams {
                    node_id: "ghost-node".to_string(),
                    project_hash: None,
                }),
                OptionalExtension(None),
            )
            .await
            .unwrap();
        assert!(is_err(&deleted));
    }

    #[tokio::test]
    async fn intelligence_delete_relation_removes_edge_leaves_nodes_intact() {
        let (_dir, _db, handler) = endpoint_test_handler();
        let _agent_guard = AgentIdVar::set("agent-intel-delete-3");

        upsert_intel_fact(&handler, "rel-a", "Node A", None).await;
        upsert_intel_fact(
            &handler,
            "rel-b",
            "Node B",
            Some(vec![IntelligenceRelationParams {
                to_node_id: "rel-a".to_string(),
                relation: "supports".to_string(),
                weight: None,
            }]),
        )
        .await;

        let walked = handler
            .intelligence_graph_walk(Parameters(IntelligenceGraphWalkParams {
                node_id: "rel-b".to_string(),
                depth: Some(1),
            }))
            .await
            .unwrap();
        let edge_id = serde_json::from_str::<serde_json::Value>(&raw_text(&walked))
            .unwrap()
            .get("edges")
            .and_then(|edges| edges.as_array())
            .and_then(|edges| edges.first())
            .and_then(|edge| edge.get("id"))
            .and_then(|id| id.as_i64())
            .expect("edge id present in graph walk response");

        let deleted = handler
            .intelligence_delete_relation(
                Parameters(IntelligenceDeleteRelationParams {
                    edge_id,
                    project_hash: None,
                }),
                OptionalExtension(None),
            )
            .await
            .unwrap();
        assert!(!is_err(&deleted), "{}", text(&deleted));

        let walked_after = handler
            .intelligence_graph_walk(Parameters(IntelligenceGraphWalkParams {
                node_id: "rel-b".to_string(),
                depth: Some(1),
            }))
            .await
            .unwrap();
        assert!(!raw_text(&walked_after).contains("rel-a"));

        // Both endpoint nodes survive the relation delete.
        let searched = handler
            .intelligence_search(
                Parameters(IntelligenceSearchParams {
                    query: "Node".to_string(),
                    kind: Some("fact".to_string()),
                    limit: Some(10),
                }),
                OptionalExtension(None),
            )
            .await
            .unwrap();
        let search_body = raw_text(&searched);
        assert!(search_body.contains("rel-a"));
        assert!(search_body.contains("rel-b"));

        let missing = handler
            .intelligence_delete_relation(
                Parameters(IntelligenceDeleteRelationParams {
                    edge_id,
                    project_hash: None,
                }),
                OptionalExtension(None),
            )
            .await
            .unwrap();
        assert!(is_err(&missing));
    }

    #[tokio::test]
    async fn intelligence_get_context_light_and_full() {
        let (_dir, _db, handler) = endpoint_test_handler();
        let _agent_guard = AgentIdVar::set("agent-ctx-1");

        let bad_scope = handler
            .intelligence_get_context(
                Parameters(IntelligenceGetContextParams {
                    scope: "sideways".to_string(),
                    project_hash: None,
                }),
                OptionalExtension(None),
            )
            .await
            .unwrap();
        assert!(is_err(&bad_scope));

        let light = handler
            .intelligence_get_context(
                Parameters(IntelligenceGetContextParams {
                    scope: "light".to_string(),
                    project_hash: None,
                }),
                OptionalExtension(None),
            )
            .await
            .unwrap();
        assert!(!is_err(&light), "{}", text(&light));
        assert!(raw_text(&light).contains("\"scope\": \"light\""));

        let full = handler
            .intelligence_get_context(
                Parameters(IntelligenceGetContextParams {
                    scope: "full".to_string(),
                    project_hash: None,
                }),
                OptionalExtension(None),
            )
            .await
            .unwrap();
        assert!(!is_err(&full), "{}", text(&full));
        assert!(raw_text(&full).contains("\"scope\": \"full\""));
    }

    // ── intelligence_list_projects / intelligence_link_projects ────

    #[tokio::test]
    async fn intelligence_list_and_link_projects() {
        let (_dir, db, handler) = endpoint_test_handler();
        let dir_a = tempdir().unwrap();
        let dir_b = tempdir().unwrap();
        db.register_project_path(dir_a.path()).unwrap();
        db.register_project_path(dir_b.path()).unwrap();
        let hash_a = crate::domain::project::workdir_hash(&dir_a.path().to_string_lossy());
        let hash_b = crate::domain::project::workdir_hash(&dir_b.path().to_string_lossy());

        let listed = handler
            .intelligence_list_projects(Parameters(IntelligenceListProjectsParams {
                query: None,
                limit: None,
            }))
            .await
            .unwrap();
        assert!(!is_err(&listed));

        let empty_fields = handler
            .intelligence_link_projects(Parameters(IntelligenceLinkProjectsParams {
                from_project_hash: "  ".to_string(),
                to_project_hash: hash_b.clone(),
                relation: None,
                weight: None,
            }))
            .await
            .unwrap();
        assert!(is_err(&empty_fields));

        let linked = handler
            .intelligence_link_projects(Parameters(IntelligenceLinkProjectsParams {
                from_project_hash: hash_a,
                to_project_hash: hash_b,
                relation: Some("depends_on".to_string()),
                weight: Some(0.5),
            }))
            .await
            .unwrap();
        assert!(!is_err(&linked), "{}", text(&linked));
        assert!(raw_text(&linked).contains("depends_on"));
    }

    // ── get_tools ────────────────────────────────────────────────

    #[tokio::test]
    async fn get_tools_returns_scoped_protocol() {
        let (_dir, _db, handler) = endpoint_test_handler();
        let _agent_guard = AgentIdVar::set("agent-tools-1");

        let bad_scope = handler
            .get_tools(
                Parameters(GetToolsParams {
                    scope: "sideways".to_string(),
                    path: None,
                }),
                OptionalExtension(None),
            )
            .await
            .unwrap();
        assert!(is_err(&bad_scope));

        let file_write = handler
            .get_tools(
                Parameters(GetToolsParams {
                    scope: "file_write".to_string(),
                    path: Some("/tmp/a.rs".to_string()),
                }),
                OptionalExtension(None),
            )
            .await
            .unwrap();
        assert!(!is_err(&file_write));
        assert!(raw_text(&file_write).contains("/tmp/a.rs"));

        let session_start = handler
            .get_tools(
                Parameters(GetToolsParams {
                    scope: "session_start".to_string(),
                    path: None,
                }),
                OptionalExtension(None),
            )
            .await
            .unwrap();
        assert!(!is_err(&session_start));
        assert!(raw_text(&session_start).contains("session_start"));
    }

    // ── project_search / project_update ─────────────────────────

    #[tokio::test]
    async fn project_search_and_update() {
        let (_dir, db, handler) = endpoint_test_handler();
        let base = tempdir().unwrap();
        let project_dir = base.path().join("searchable-project");
        std::fs::create_dir_all(&project_dir).unwrap();
        let project = db.register_project_path(&project_dir).unwrap();

        let no_match = handler
            .project_search(Parameters(ProjectSearchParams {
                query: "nothing-matches-this-xyz".to_string(),
            }))
            .await
            .unwrap();
        assert!(text(&no_match).contains("No projects found"));

        let found = handler
            .project_search(Parameters(ProjectSearchParams {
                query: "searchable-project".to_string(),
            }))
            .await
            .unwrap();
        assert!(raw_text(&found).contains(&project.hash));

        let updated = handler
            .project_update(Parameters(ProjectUpdateParams {
                project_hash: project.hash.clone(),
                description: Some("A test project".to_string()),
                tags: Some(vec!["rust".to_string()]),
            }))
            .await
            .unwrap();
        assert!(!is_err(&updated), "{}", text(&updated));

        let missing = handler
            .project_update(Parameters(ProjectUpdateParams {
                project_hash: "not-a-real-hash".to_string(),
                description: Some("x".to_string()),
                tags: None,
            }))
            .await
            .unwrap();
        assert!(is_err(&missing));
    }

    // ── project_remap ─────────────────────────────────────────────

    #[tokio::test]
    async fn project_remap_moves_project_to_new_path() {
        let (_dir, db, handler) = endpoint_test_handler();
        let base = tempdir().unwrap();
        let old_dir = base.path().join("cadforge");
        let new_dir = base.path().join("cadspec");
        std::fs::create_dir_all(&old_dir).unwrap();
        std::fs::create_dir_all(&new_dir).unwrap();
        let project = db.register_project_path(&old_dir).unwrap();
        db.insert_terminal_session("t-1", "t-1", "bash", &project.path)
            .unwrap();

        let result = handler
            .project_remap(Parameters(ProjectRemapParams {
                project_hash: project.hash.clone(),
                new_path: new_dir.to_string_lossy().to_string(),
                dry_run: None,
                force: None,
            }))
            .await
            .unwrap();
        assert!(!is_err(&result), "{}", text(&result));
        let body = raw_text(&result);
        assert!(body.contains("\"kind\": \"move\""));
        assert!(body.contains("\"rows_moved\": 2"));

        assert!(db.get_project(&project.hash).unwrap().is_none());
        let new_canonical = std::fs::canonicalize(&new_dir).unwrap();
        let moved = db
            .get_project_by_path(&new_canonical)
            .unwrap()
            .expect("project moved to new path");
        assert_eq!(
            db.project_dependent_counts(&moved.path)
                .unwrap()
                .terminal_sessions,
            1
        );
    }

    #[tokio::test]
    async fn project_remap_dry_run_changes_nothing() {
        let (_dir, db, handler) = endpoint_test_handler();
        let base = tempdir().unwrap();
        let old_dir = base.path().join("old-loc");
        let new_dir = base.path().join("new-loc");
        std::fs::create_dir_all(&old_dir).unwrap();
        std::fs::create_dir_all(&new_dir).unwrap();
        let project = db.register_project_path(&old_dir).unwrap();

        let result = handler
            .project_remap(Parameters(ProjectRemapParams {
                project_hash: project.hash.clone(),
                new_path: new_dir.to_string_lossy().to_string(),
                dry_run: Some(true),
                force: None,
            }))
            .await
            .unwrap();
        assert!(!is_err(&result), "{}", text(&result));
        assert!(raw_text(&result).contains("\"dry_run\": true"));

        // Nothing actually moved.
        assert!(db.get_project(&project.hash).unwrap().is_some());
    }

    #[tokio::test]
    async fn project_remap_refuses_missing_path_without_force() {
        let (_dir, db, handler) = endpoint_test_handler();
        let base = tempdir().unwrap();
        let old_dir = base.path().join("still-here");
        std::fs::create_dir_all(&old_dir).unwrap();
        let project = db.register_project_path(&old_dir).unwrap();

        let result = handler
            .project_remap(Parameters(ProjectRemapParams {
                project_hash: project.hash.clone(),
                new_path: "/definitely/does/not/exist/anywhere".to_string(),
                dry_run: None,
                force: None,
            }))
            .await
            .unwrap();
        assert!(is_err(&result));
        assert!(db.get_project(&project.hash).unwrap().is_some());
    }

    // ── skill_list / skill_get ──

    #[tokio::test]
    async fn skill_list_empty_and_skill_get_unknown() {
        let (_dir, _db, handler) = endpoint_test_handler();

        let listed = handler.skill_list().await.unwrap();
        assert!(!is_err(&listed));

        let missing = handler
            .skill_get(Parameters(SkillGetParams {
                name: "unknown-skill".to_string(),
            }))
            .await;
        assert!(missing.is_err());
    }

    // ── intelligence prefix resolution + created field ──────────────

    #[tokio::test]
    async fn intelligence_upsert_returns_created_true_for_new_node() {
        let (_dir, _db, handler) = endpoint_test_handler();
        let _agent_guard = AgentIdVar::set("agent-intel-created-1");

        let result = handler
            .intelligence_upsert(
                Parameters(IntelligenceUpsertParams {
                    node_data: IntelligenceNodeParams {
                        id: Some("brand-new-node-1".to_string()),
                        kind: "fact".to_string(),
                        title: "First time".to_string(),
                        body: "body".to_string(),
                        metadata: None,
                        project_hash: None,
                        session_id: None,
                        relations: None,
                    },
                }),
                OptionalExtension(None),
            )
            .await
            .unwrap();
        assert!(!is_err(&result), "{}", text(&result));
        let body = raw_text(&result);
        assert!(
            body.contains("\"created\": true"),
            "expected created:true in: {body}"
        );
        // An explicit id that matched nothing must produce a loud warning, not
        // just `created: true` (which also fires on the auto-id path).
        assert!(
            body.contains("\"warning\":") && body.contains("brand-new-node-1"),
            "explicit unknown id should warn about creating a new node: {body}"
        );
    }

    #[tokio::test]
    async fn intelligence_upsert_returns_created_false_for_update() {
        let (_dir, _db, handler) = endpoint_test_handler();
        let _agent_guard = AgentIdVar::set("agent-intel-created-2");

        let first = handler
            .intelligence_upsert(
                Parameters(IntelligenceUpsertParams {
                    node_data: IntelligenceNodeParams {
                        id: Some("update-me-1".to_string()),
                        kind: "fact".to_string(),
                        title: "Original".to_string(),
                        body: "v1".to_string(),
                        metadata: None,
                        project_hash: None,
                        session_id: None,
                        relations: None,
                    },
                }),
                OptionalExtension(None),
            )
            .await
            .unwrap();
        assert!(!is_err(&first), "{}", text(&first));
        assert!(
            raw_text(&first).contains("\"created\": true"),
            "first upsert should be created:true: {}",
            raw_text(&first)
        );

        let second = handler
            .intelligence_upsert(
                Parameters(IntelligenceUpsertParams {
                    node_data: IntelligenceNodeParams {
                        id: Some("update-me-1".to_string()),
                        kind: "fact".to_string(),
                        title: "Updated".to_string(),
                        body: "v2".to_string(),
                        metadata: None,
                        project_hash: None,
                        session_id: None,
                        relations: None,
                    },
                }),
                OptionalExtension(None),
            )
            .await
            .unwrap();
        assert!(!is_err(&second), "{}", text(&second));
        assert!(
            raw_text(&second).contains("\"created\": false"),
            "second upsert should be created:false: {}",
            raw_text(&second)
        );
        // Updating an existing node must not carry the "created a new node"
        // warning.
        assert!(
            !raw_text(&second).contains("\"warning\":"),
            "update path should not warn: {}",
            raw_text(&second)
        );
    }

    #[tokio::test]
    async fn intelligence_upsert_with_prefix_updates_existing() {
        let (_dir, _db, handler) = endpoint_test_handler();
        let _agent_guard = AgentIdVar::set("agent-intel-prefix-1");

        let full_id = "3a476c63-6b4a-4860-9c18-1c784b40a4b2";
        let create = handler
            .intelligence_upsert(
                Parameters(IntelligenceUpsertParams {
                    node_data: IntelligenceNodeParams {
                        id: Some(full_id.to_string()),
                        kind: "fact".to_string(),
                        title: "Original".to_string(),
                        body: "v1".to_string(),
                        metadata: None,
                        project_hash: None,
                        session_id: None,
                        relations: None,
                    },
                }),
                OptionalExtension(None),
            )
            .await
            .unwrap();
        assert!(!is_err(&create), "{}", text(&create));

        let via_prefix = handler
            .intelligence_upsert(
                Parameters(IntelligenceUpsertParams {
                    node_data: IntelligenceNodeParams {
                        id: Some("3a476c63".to_string()),
                        kind: "fact".to_string(),
                        title: "Updated via prefix".to_string(),
                        body: "v2".to_string(),
                        metadata: None,
                        project_hash: None,
                        session_id: None,
                        relations: None,
                    },
                }),
                OptionalExtension(None),
            )
            .await
            .unwrap();
        assert!(!is_err(&via_prefix), "{}", text(&via_prefix));
        let body = raw_text(&via_prefix);
        assert!(
            body.contains("\"created\": false"),
            "prefix upsert should update, not create: {body}"
        );
        assert!(
            body.contains(full_id),
            "response should contain the full UUID: {body}"
        );
        assert!(
            body.contains("Updated via prefix"),
            "response should contain the new title: {body}"
        );

        let searched = handler
            .intelligence_search(
                Parameters(IntelligenceSearchParams {
                    query: "Updated via prefix".to_string(),
                    kind: Some("fact".to_string()),
                    limit: Some(10),
                }),
                OptionalExtension(None),
            )
            .await
            .unwrap();
        let search_body = raw_text(&searched);
        assert!(
            search_body.contains(full_id),
            "the node should exist with updated title: {search_body}"
        );
        assert!(
            search_body.contains("\"count\": 1"),
            "exactly one node must match — the prefix upsert must not have created a duplicate: {search_body}"
        );
    }

    #[tokio::test]
    async fn intelligence_upsert_with_ambiguous_prefix_errors() {
        let (_dir, _db, handler) = endpoint_test_handler();
        let _agent_guard = AgentIdVar::set("agent-intel-prefix-ambig");

        let id1 = "abc12300-0000-0000-0000-000000000001";
        let id2 = "abc45600-0000-0000-0000-000000000002";
        for id in [id1, id2] {
            let r = handler
                .intelligence_upsert(
                    Parameters(IntelligenceUpsertParams {
                        node_data: IntelligenceNodeParams {
                            id: Some(id.to_string()),
                            kind: "fact".to_string(),
                            title: format!("Node {id}"),
                            body: "body".to_string(),
                            metadata: None,
                            project_hash: None,
                            session_id: None,
                            relations: None,
                        },
                    }),
                    OptionalExtension(None),
                )
                .await
                .unwrap();
            assert!(!is_err(&r), "{}", text(&r));
        }

        let ambiguous = handler
            .intelligence_upsert(
                Parameters(IntelligenceUpsertParams {
                    node_data: IntelligenceNodeParams {
                        id: Some("abc".to_string()),
                        kind: "fact".to_string(),
                        title: "Should fail".to_string(),
                        body: "body".to_string(),
                        metadata: None,
                        project_hash: None,
                        session_id: None,
                        relations: None,
                    },
                }),
                OptionalExtension(None),
            )
            .await
            .unwrap();
        assert!(
            is_err(&ambiguous),
            "ambiguous prefix should error: {}",
            text(&ambiguous)
        );
        let err_body = raw_text(&ambiguous);
        assert!(
            err_body.contains("Ambiguous"),
            "error should mention Ambiguous: {err_body}"
        );
        assert!(
            err_body.contains(id1),
            "error should list candidate {id1}: {err_body}"
        );
        assert!(
            err_body.contains(id2),
            "error should list candidate {id2}: {err_body}"
        );
    }

    #[tokio::test]
    async fn intelligence_graph_walk_with_prefix() {
        let (_dir, _db, handler) = endpoint_test_handler();
        let _agent_guard = AgentIdVar::set("agent-intel-walk-prefix");

        let full_id = "deadbeef-0000-0000-0000-000000000001";
        let created = handler
            .intelligence_upsert(
                Parameters(IntelligenceUpsertParams {
                    node_data: IntelligenceNodeParams {
                        id: Some(full_id.to_string()),
                        kind: "fact".to_string(),
                        title: "Walk me".to_string(),
                        body: "body".to_string(),
                        metadata: None,
                        project_hash: None,
                        session_id: None,
                        relations: None,
                    },
                }),
                OptionalExtension(None),
            )
            .await
            .unwrap();
        assert!(!is_err(&created), "{}", text(&created));

        let walked = handler
            .intelligence_graph_walk(Parameters(IntelligenceGraphWalkParams {
                node_id: "deadbeef".to_string(),
                depth: Some(1),
            }))
            .await
            .unwrap();
        assert!(
            !is_err(&walked),
            "prefix walk should succeed: {}",
            text(&walked)
        );
        let body = raw_text(&walked);
        assert!(
            body.contains(full_id),
            "walk response should contain the full UUID: {body}"
        );
    }

    #[tokio::test]
    async fn intelligence_delete_node_with_prefix() {
        let (_dir, _db, handler) = endpoint_test_handler();
        let _agent_guard = AgentIdVar::set("agent-intel-del-prefix");

        let full_id = "feedface-0000-0000-0000-000000000001";
        let created = handler
            .intelligence_upsert(
                Parameters(IntelligenceUpsertParams {
                    node_data: IntelligenceNodeParams {
                        id: Some(full_id.to_string()),
                        kind: "fact".to_string(),
                        title: "Delete me by prefix".to_string(),
                        body: "body".to_string(),
                        metadata: None,
                        project_hash: None,
                        session_id: None,
                        relations: None,
                    },
                }),
                OptionalExtension(None),
            )
            .await
            .unwrap();
        assert!(!is_err(&created), "{}", text(&created));

        let deleted = handler
            .intelligence_delete_node(
                Parameters(IntelligenceDeleteNodeParams {
                    node_id: "feedface".to_string(),
                    project_hash: None,
                }),
                OptionalExtension(None),
            )
            .await
            .unwrap();
        assert!(
            !is_err(&deleted),
            "prefix delete should succeed: {}",
            text(&deleted)
        );
        let body = raw_text(&deleted);
        assert!(
            body.contains(full_id),
            "delete response should contain the full UUID: {body}"
        );

        let walked = handler
            .intelligence_graph_walk(Parameters(IntelligenceGraphWalkParams {
                node_id: full_id.to_string(),
                depth: Some(1),
            }))
            .await
            .unwrap();
        assert!(
            is_err(&walked),
            "node should be gone after prefix delete: {}",
            text(&walked)
        );
    }

    #[tokio::test]
    async fn spec_list_default_compact_omits_description() {
        let (_dir, _db, handler) = endpoint_test_handler();
        let created = handler
            .spec_create(Parameters(SpecCreateParams {
                name: "Compact test".to_string(),
                description: valid_spec_description(),
                workdir: None,
            }))
            .await
            .unwrap();
        assert!(!is_err(&created), "{}", text(&created));

        let listed = handler
            .spec_list(Parameters(SpecListParams {
                workdir: None,
                status: None,
                unassigned_only: None,
                include_descriptions: None,
            }))
            .await
            .unwrap();
        let body = raw_text(&listed);
        assert!(
            !body.contains("Objective"),
            "compact mode must omit description content"
        );
        assert!(body.contains("Compact test"));
    }

    #[tokio::test]
    async fn spec_list_with_include_descriptions_returns_description() {
        let (_dir, _db, handler) = endpoint_test_handler();
        let desc = valid_spec_description();
        let created = handler
            .spec_create(Parameters(SpecCreateParams {
                name: "Verbose test".to_string(),
                description: desc.clone(),
                workdir: None,
            }))
            .await
            .unwrap();
        assert!(!is_err(&created), "{}", text(&created));

        let listed = handler
            .spec_list(Parameters(SpecListParams {
                workdir: None,
                status: None,
                unassigned_only: None,
                include_descriptions: Some(true),
            }))
            .await
            .unwrap();
        let body = raw_text(&listed);
        assert!(
            body.contains("Objective"),
            "include_descriptions must include description content"
        );
    }

    #[tokio::test]
    async fn loop_node_runs_list_compact_mode() {
        let (dir, db, handler) = endpoint_test_handler();
        let lp = insert_test_loop(&db, dir.path());
        let spec = insert_test_spec(&db, &lp.id, 1);
        let node = insert_named_node(&db, &spec.id, "builder", 1);
        let now = chrono::Utc::now();
        insert_finalized_node_run(
            &db,
            &lp.id,
            &spec.id,
            &node.id,
            LoopRunStatus::Pass,
            None,
            now,
        );

        let listed = handler
            .loop_node_runs_list(Parameters(LoopNodeRunsListParams {
                loop_id: lp.id.clone(),
                spec_id: None,
                node_id: None,
                limit: None,
                offset: None,
                compact: Some(true),
            }))
            .await
            .unwrap();
        assert!(!is_err(&listed), "{}", text(&listed));
        let body = raw_text(&listed);
        let parsed: serde_json::Value = serde_json::from_str(&body).unwrap();
        let runs = parsed["runs"].as_array().unwrap();
        assert_eq!(runs.len(), 1);
        assert!(runs[0].get("id").is_none(), "compact mode must omit id");
        assert!(
            runs[0].get("spec_id").is_none(),
            "compact mode must omit spec_id"
        );
        assert_eq!(runs[0]["spec_name"], "Test Spec");
        assert_eq!(runs[0]["node_name"], "builder");
    }

    #[tokio::test]
    async fn loop_node_runs_list_reports_truncation() {
        let (dir, db, handler) = endpoint_test_handler();
        let lp = insert_test_loop(&db, dir.path());
        let spec = insert_test_spec(&db, &lp.id, 1);
        let node = insert_named_node(&db, &spec.id, "builder", 1);
        let now = chrono::Utc::now();
        for i in 0..3 {
            insert_finalized_node_run(
                &db,
                &lp.id,
                &spec.id,
                &node.id,
                LoopRunStatus::Pass,
                None,
                now - chrono::Duration::minutes(i),
            );
        }

        let capped = handler
            .loop_node_runs_list(Parameters(LoopNodeRunsListParams {
                loop_id: lp.id.clone(),
                spec_id: None,
                node_id: None,
                limit: Some(1),
                offset: None,
                compact: None,
            }))
            .await
            .unwrap();
        assert!(!is_err(&capped), "{}", text(&capped));
        let parsed: serde_json::Value = serde_json::from_str(&raw_text(&capped)).unwrap();
        assert_eq!(parsed["runs"].as_array().unwrap().len(), 1);
        assert_eq!(parsed["total"], 3);
        assert_eq!(parsed["truncated"], true);
        assert_eq!(parsed["omitted"], 2);

        let full = handler
            .loop_node_runs_list(Parameters(LoopNodeRunsListParams {
                loop_id: lp.id.clone(),
                spec_id: None,
                node_id: None,
                limit: Some(50),
                offset: None,
                compact: None,
            }))
            .await
            .unwrap();
        let parsed_full: serde_json::Value = serde_json::from_str(&raw_text(&full)).unwrap();
        assert_eq!(parsed_full["total"], 3);
        assert!(
            parsed_full.get("truncated").is_none(),
            "a complete result must not claim truncation"
        );
        assert!(parsed_full.get("omitted").is_none());
    }

    #[tokio::test]
    async fn spec_update_with_prefix_resolves() {
        let (_dir, db, handler) = endpoint_test_handler();
        let created = handler
            .spec_create(Parameters(SpecCreateParams {
                name: "Prefix test".to_string(),
                description: valid_spec_description(),
                workdir: None,
            }))
            .await
            .unwrap();
        assert!(!is_err(&created), "{}", text(&created));

        let all_specs = db.list_specs(None, None, false).unwrap();
        let full_id = all_specs[0].id.clone();
        let prefix = &full_id[..8];

        let updated = handler
            .spec_update(Parameters(SpecUpdateParams {
                spec_id: prefix.to_string(),
                name: Some("Renamed via prefix".to_string()),
                description: None,
                workdir: None,
            }))
            .await
            .unwrap();
        assert!(!is_err(&updated), "{}", text(&updated));
        assert_eq!(
            db.get_loop_spec(&full_id).unwrap().unwrap().name,
            "Renamed via prefix"
        );
    }

    #[tokio::test]
    async fn resolve_spec_id_by_prefix_tests() {
        let (_dir, db, _handler) = endpoint_test_handler();
        let spec1 = LoopSpec {
            id: "aaaaaaaa-1111-1111-1111-111111111111".to_string(),
            loop_id: None,
            name: "Spec A".to_string(),
            description: Some("First".to_string()),
            position: 1,
            parallelizable: false,
            status: LoopSpecStatus::Pending,
            started_at: None,
            completed_at: None,
            spec_start_head: None,
            spec_committed_head: None,
            workdir: None,
            completed_via: None,
            completed_via_reason: None,
            completed_via_at: None,
        };
        db.insert_loop_spec(&spec1).unwrap();
        let spec2 = LoopSpec {
            id: "aaaaaaaa-2222-2222-2222-222222222222".to_string(),
            loop_id: None,
            name: "Spec B".to_string(),
            description: Some("Second".to_string()),
            position: 2,
            parallelizable: false,
            status: LoopSpecStatus::Pending,
            started_at: None,
            completed_at: None,
            spec_start_head: None,
            spec_committed_head: None,
            workdir: None,
            completed_via: None,
            completed_via_reason: None,
            completed_via_at: None,
        };
        db.insert_loop_spec(&spec2).unwrap();

        let exact = db.resolve_spec_id_by_prefix(&spec1.id).unwrap();
        assert_eq!(exact, Some(spec1.id.clone()));

        let prefix_match = db.resolve_spec_id_by_prefix("aaaaaaaa-1111").unwrap();
        assert_eq!(prefix_match, Some(spec1.id));

        let ambiguous = db.resolve_spec_id_by_prefix("aaaaaaaa");
        assert!(ambiguous.is_err(), "ambiguous prefix should error");
        assert!(ambiguous.unwrap_err().to_string().contains("Ambiguous"));

        let not_found = db.resolve_spec_id_by_prefix("zzzzzzzz").unwrap();
        assert_eq!(not_found, None);
    }

    #[tokio::test]
    async fn resolve_queue_id_by_prefix_tests() {
        let (_dir, db, _handler) = endpoint_test_handler();
        let q1 = Queue {
            id: "q-aaaaaaaa-1111".to_string(),
            name: "Q1".to_string(),
            created_at: chrono::Utc::now(),
        };
        let q2 = Queue {
            id: "q-aaaaaaaa-2222".to_string(),
            name: "Q2".to_string(),
            created_at: chrono::Utc::now(),
        };
        db.insert_queue(&q1).unwrap();
        db.insert_queue(&q2).unwrap();

        let exact = db.resolve_queue_id_by_prefix("q-aaaaaaaa-1111").unwrap();
        assert_eq!(exact, Some("q-aaaaaaaa-1111".to_string()));

        let prefix_match = db.resolve_queue_id_by_prefix("q-aaaaaaaa-111").unwrap();
        assert_eq!(prefix_match, Some("q-aaaaaaaa-1111".to_string()));

        let ambiguous = db.resolve_queue_id_by_prefix("q-aaaaaaaa");
        assert!(ambiguous.is_err());

        let not_found = db.resolve_queue_id_by_prefix("q-zzzz").unwrap();
        assert_eq!(not_found, None);
    }

    #[tokio::test]
    async fn resolve_loop_id_by_prefix_tests() {
        let (dir, db, _handler) = endpoint_test_handler();
        let lp1 = insert_test_loop(&db, dir.path());
        let lp2 = Loop {
            id: "loop-bbbbbbbb-2222".to_string(),
            name: "Loop B".to_string(),
            description: None,
            workdir: dir.path().to_string_lossy().to_string(),
            status: LoopStatus::Draft,
            trigger: None,
            created_at: chrono::Utc::now(),
            started_at: None,
            completed_at: None,
            autorun_at: None,
            active_run_queue_id: None,
            on_completed: None,
            auto_continue_at: None,
            auto_continue_action: None,
            archived: false,
            paused_by_reconciliation: false,
        };
        db.insert_loop(&lp2).unwrap();

        let exact = db.resolve_loop_id_by_prefix(&lp1.id).unwrap();
        assert_eq!(exact, Some(lp1.id));

        let not_found = db.resolve_loop_id_by_prefix("loop-zzzz").unwrap();
        assert_eq!(not_found, None);
    }
}
