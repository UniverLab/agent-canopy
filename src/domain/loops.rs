use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::domain::models::{Trigger, WatchEvent};

const REQUIRED_SPEC_SECTIONS: &[(&str, &[&str])] = &[
    (
        "functional requirements",
        &[
            "functional requirements",
            "requerimientos funcionales",
            "requisitos funcionales",
        ],
    ),
    (
        "non-functional requirements",
        &[
            "non-functional requirements",
            "non functional requirements",
            "requerimientos no funcionales",
            "requisitos no funcionales",
        ],
    ),
    (
        "objective / expected outcome",
        &[
            "objective",
            "expected outcome",
            "objetivo",
            "resultado esperado",
        ],
    ),
    (
        "constraints",
        &[
            "constraints",
            "what to respect",
            "restrictions",
            "restricciones",
            "que respetar",
            "qué respetar",
        ],
    ),
    (
        "guidelines",
        &[
            "guidelines",
            "lineamientos",
            "guidance",
            "lineas guia",
            "líneas guía",
        ],
    ),
    (
        "in scope",
        &["in scope", "scope in", "que si", "qué sí", "incluye"],
    ),
    (
        "out of scope",
        &["out of scope", "scope out", "que no", "qué no", "excluye"],
    ),
];

pub fn validate_spec_description_template(description: &str) -> Result<(), String> {
    let normalized = description.trim().to_lowercase();
    if normalized.is_empty() {
        return Err(format!(
            "Loop spec description must include sections for: {}.",
            required_spec_section_names()
        ));
    }

    let missing: Vec<&str> = REQUIRED_SPEC_SECTIONS
        .iter()
        .filter_map(|(canonical, aliases)| {
            let has_section = aliases.iter().any(|alias| {
                let marker = alias.to_lowercase();
                normalized.contains(&format!("{marker}:"))
                    || normalized.contains(&format!("{marker}\n"))
                    || normalized.contains(&format!("## {marker}"))
                    || normalized.contains(&format!("### {marker}"))
                    || normalized.contains(&format!("- {marker}:"))
                    || normalized.contains(&format!("* {marker}:"))
            });
            (!has_section).then_some(*canonical)
        })
        .collect();

    if missing.is_empty() {
        return Ok(());
    }

    Err(format!(
        "Loop spec description is missing required sections: {}. Expected sections: {}.",
        missing.join(", "),
        required_spec_section_names()
    ))
}

fn required_spec_section_names() -> String {
    REQUIRED_SPEC_SECTIONS
        .iter()
        .map(|(name, _)| *name)
        .collect::<Vec<_>>()
        .join(", ")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LoopStatus {
    Draft,
    Running,
    Paused,
    Completed,
    Failed,
}

impl LoopStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Draft => "draft",
            Self::Running => "running",
            Self::Paused => "paused",
            Self::Completed => "completed",
            Self::Failed => "failed",
        }
    }

    pub fn from_str(value: &str) -> Self {
        match value {
            "running" => Self::Running,
            "paused" => Self::Paused,
            "completed" => Self::Completed,
            "failed" => Self::Failed,
            _ => Self::Draft,
        }
    }
}

/// Result of [`crate::db::Database::reset_loop`] — the single state-transition
/// path used by both the `loop_reset` MCP tool and the scheduler's
/// auto-reset-and-resume of a `failed` loop on autorun.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LoopResetOutcome {
    NotFound,
    /// A `running` loop must be paused first — resetting underneath a live
    /// run would corrupt its in-flight state.
    Running,
    /// One of the explicitly requested `specs` doesn't belong to this loop.
    InvalidSpec(String),
    Reset {
        spec_count: usize,
    },
}

#[derive(Debug, Clone)]
pub enum SpecAdminStatusOutcome {
    Success,
    NotFound,
    /// Spec is bound to a loop (not standalone); spec_set_status only
    /// administers standalone specs.
    NotStandalone(String),
    /// Spec has an active run and cannot be administratively transitioned.
    ActiveRun {
        loop_id: String,
        run_id: String,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LoopSpecStatus {
    Pending,
    Running,
    Completed,
    Failed,
    Skipped,
}

impl LoopSpecStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Running => "running",
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Skipped => "skipped",
        }
    }

    pub fn from_str(value: &str) -> Self {
        match value {
            "running" => Self::Running,
            "completed" => Self::Completed,
            "failed" => Self::Failed,
            "skipped" => Self::Skipped,
            _ => Self::Pending,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LoopNodeKind {
    Agent,
    Check,
    Gate,
    /// Engine-managed quorum node for an ensemble (F1) — never created via
    /// `loop_add_node` directly, only as part of `loop_add_ensemble`'s
    /// one-call expansion. Waits for every member branch to terminate,
    /// consolidates their outputs, and routes onward. See
    /// [`crate::loop_engine::LoopEngine`]'s ensemble fan-out handling.
    Join,
}

impl LoopNodeKind {
    /// Serde/DB-safe kind string. Unchanged for all variants (keeps
    /// existing DB rows and `from_str` parsing intact).
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Agent => "agent",
            Self::Check => "check",
            Self::Gate => "gate",
            Self::Join => "join",
        }
    }

    /// User-facing label for the node kind. Returns `"quorum"` for
    /// `Join` so user-facing surfaces (TUI, CLI, MCP descriptions)
    /// display the intent rather than the internal enum name.
    pub fn display_str(self) -> &'static str {
        match self {
            Self::Join => "quorum",
            other => other.as_str(),
        }
    }

    pub fn from_str(value: &str) -> Option<Self> {
        match value {
            "agent" => Some(Self::Agent),
            "check" => Some(Self::Check),
            "gate" => Some(Self::Gate),
            "join" => Some(Self::Join),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LoopEdgeCondition {
    Pass,
    Fail,
    Always,
}

impl LoopEdgeCondition {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pass => "pass",
            Self::Fail => "fail",
            Self::Always => "always",
        }
    }

    pub fn from_str(value: &str) -> Option<Self> {
        match value {
            "pass" => Some(Self::Pass),
            "fail" => Some(Self::Fail),
            "always" => Some(Self::Always),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LoopRunStatus {
    Running,
    Pass,
    Fail,
}

impl LoopRunStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Pass => "pass",
            Self::Fail => "fail",
        }
    }

    pub fn from_str(value: &str) -> Self {
        match value {
            "pass" => Self::Pass,
            "fail" => Self::Fail,
            _ => Self::Running,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Loop {
    pub id: String,
    pub name: String,
    pub description: Option<String>,
    pub workdir: String,
    pub status: LoopStatus,
    /// Optional automatic trigger. Reuses the agent [`Trigger`] model so a loop
    /// can fire on a cron schedule or a file-system watch, exactly like an
    /// agent. `None` means the loop is manual-only (`loop_run`).
    #[serde(default)]
    pub trigger: Option<Trigger>,
    pub created_at: DateTime<Utc>,
    pub started_at: Option<DateTime<Utc>>,
    pub completed_at: Option<DateTime<Utc>>,
    /// One-shot resume schedule: when set and reached, the scheduler starts
    /// this loop once (e.g. `run_loop`) and clears the field. Unlike
    /// `trigger`'s cron, this never repeats — it exists so a loop that fails
    /// on a quota can reschedule its own resumption at the exact reset time
    /// instead of relying on a blindly polling cron.
    #[serde(default)]
    pub autorun_at: Option<DateTime<Utc>>,
    /// One-shot deferred resume for a *paused* loop: when set and reached
    /// while the loop is still `Paused`, the scheduler fires the equivalent
    /// of `loop_continue` (see `auto_continue_action`) — preserving the
    /// paused cursor/context — rather than `autorun_at`'s reset-and-relaunch.
    /// Lets a user pause a loop to stop burning quota now and have it pick
    /// back up automatically at a later time, without a human calling
    /// `loop_continue`. Deliberately a separate field from `autorun_at`
    /// rather than a shared one: the two fire through entirely different
    /// paths (`resume_background` alone vs. auto-reset-then-`resume_background`)
    /// and must never be conflated. See `is_auto_continue_due`.
    #[serde(default)]
    pub auto_continue_at: Option<DateTime<Utc>>,
    /// The `loop_continue` action (`"retry_current_node"` or
    /// `"skip_next_spec"`) to apply when `auto_continue_at` fires. `None`
    /// (or any value other than `"skip_next_spec"`) defaults to
    /// `retry_current_node` — see [`crate::scheduler::cron_scheduler`]'s
    /// auto-continue fire branch.
    #[serde(default)]
    pub auto_continue_action: Option<String>,
    /// The pool a run against this loop is currently — or most recently —
    /// drew from, persisted the moment that run starts (`None` for a
    /// bound-spec run). Interrupted runs (a quota failure, a daemon restart)
    /// leave this set so every resume path — scheduled autorun, `loop_reset`
    /// — knows which pool to pick up rather than falling back to the loop's
    /// (often empty) bound specs. It survives genuine completion too (B31),
    /// giving a finished pool-driven loop the only link back to the queue it
    /// ran so `loop list` / `loop info` can render its real `n/n` progress
    /// instead of `0/0`. A stale value never pollutes a later run: every
    /// launch path overwrites this field before the first spec executes, so
    /// a fresh `loop_run` against a different pool (or a bound-spec run,
    /// which writes `None`) replaces it.
    #[serde(default)]
    pub active_run_pool_id: Option<String>,
    /// Optional post-completion hook (N2): an agent-node-style config the
    /// engine fires exactly once, right after a run transitions to
    /// `Completed` — never on `failed`/`paused`, never retroactively, and
    /// never more than once per completing run (a completed→reset→completed
    /// cycle fires again, once per completion). `None` preserves pre-N2
    /// behavior exactly. See [`crate::loop_engine::LoopEngine`]'s
    /// `run_loop_dispatch` for where it fires and
    /// `render_completion_hook_prompt` for its placeholders.
    #[serde(default)]
    pub on_completed: Option<LoopCompletionHook>,
}

/// Config for a loop's `on_completed` hook — deliberately shaped like an
/// agent node's config (`platform`/`model`/`timeout_minutes`) so it reuses
/// the same CLI-resolution and spawn path, but keeps its own `prompt` field
/// (rather than `prompt_template`) since it has no spec/node graph context to
/// template against — only the placeholders `render_completion_hook_prompt`
/// documents.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LoopCompletionHook {
    pub platform: String,
    pub model: Option<String>,
    pub prompt: String,
    pub timeout_minutes: Option<u64>,
}

impl Loop {
    /// Short label for the loop's trigger: `"cron"`, `"watch"`, or `"manual"`.
    pub fn trigger_type_label(&self) -> &'static str {
        match &self.trigger {
            Some(Trigger::Cron { .. }) => "cron",
            Some(Trigger::Watch { .. }) => "watch",
            None => "manual",
        }
    }

    /// The cron expression when this loop is cron-triggered.
    pub fn schedule_expr(&self) -> Option<&str> {
        match &self.trigger {
            Some(Trigger::Cron { schedule_expr }) => Some(schedule_expr),
            _ => None,
        }
    }

    /// The watched path when this loop is watch-triggered.
    pub fn watch_path(&self) -> Option<&str> {
        match &self.trigger {
            Some(Trigger::Watch { path, .. }) => Some(path),
            _ => None,
        }
    }

    #[allow(dead_code)]
    pub fn watch_events(&self) -> Option<&[WatchEvent]> {
        match &self.trigger {
            Some(Trigger::Watch { events, .. }) => Some(events),
            _ => None,
        }
    }

    pub fn is_cron(&self) -> bool {
        matches!(&self.trigger, Some(Trigger::Cron { .. }))
    }

    pub fn is_watch(&self) -> bool {
        matches!(&self.trigger, Some(Trigger::Watch { .. }))
    }

    /// Whether a triggered loop is currently eligible to start a fresh run.
    ///
    /// A loop that is already `Running` or `Paused` must not be re-launched by
    /// its trigger — that would spawn a duplicate execution over the same
    /// graph. Draft/Completed/Failed loops are fireable (a scheduled loop
    /// re-runs its graph on each cron slot / watch event).
    pub fn is_fireable(&self) -> bool {
        !matches!(self.status, LoopStatus::Running | LoopStatus::Paused)
    }

    /// Whether this loop's one-shot `autorun_at` schedule is due at `now`.
    ///
    /// True only when `autorun_at` is set, `now` has reached it, and the loop
    /// isn't already `Running`/`Paused`. Firing must clear `autorun_at` so it
    /// never fires twice.
    pub fn is_autorun_due(&self, now: DateTime<Utc>) -> bool {
        self.autorun_at.is_some_and(|at| now >= at) && self.is_fireable()
    }

    /// Whether `auto_continue_at` has been reached at `now`, independent of
    /// status. Used by the scheduler to decide when a schedule is stale (the
    /// loop left `Paused` some other way before firing) and must be cleared
    /// even though it won't actually resume the loop — see
    /// [`Self::is_auto_continue_due`] for the status-gated check that decides
    /// whether to fire.
    pub fn is_auto_continue_time_reached(&self, now: DateTime<Utc>) -> bool {
        self.auto_continue_at.is_some_and(|at| now >= at)
    }

    /// Whether this loop's one-shot `auto_continue_at` schedule should
    /// actually fire a deferred `loop_continue` at `now`.
    ///
    /// Deliberately Paused-only — unlike [`Self::is_autorun_due`], which is
    /// due on any *fireable* (non-`Running`/`Paused`) status. Deferring a
    /// resume only makes sense while the loop is sitting `Paused`; if it left
    /// that state some other way (manual `loop_continue`, failure) before the
    /// scheduled time, the schedule is stale — the scheduler clears it
    /// without firing rather than waiting here for `Paused` to recur.
    pub fn is_auto_continue_due(&self, now: DateTime<Utc>) -> bool {
        self.auto_continue_at.is_some_and(|at| now >= at) && self.status == LoopStatus::Paused
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LoopSpec {
    pub id: String,
    /// The loop this spec has been assigned to. `None` means the spec is a
    /// standalone backlog item — authored ahead of time, not yet queued into
    /// any loop's run.
    pub loop_id: Option<String>,
    pub name: String,
    pub description: Option<String>,
    pub position: i64,
    pub parallelizable: bool,
    pub status: LoopSpecStatus,
    pub started_at: Option<DateTime<Utc>>,
    pub completed_at: Option<DateTime<Utc>>,
    /// The loop's workdir `git rev-parse HEAD`, captured once when this spec
    /// starts running (not per node). Lets `check` nodes verify "did this
    /// spec commit anything?" via `{{spec_start_head}}` without relying on
    /// state outside the spec row (e.g. a file marker) that would survive a
    /// daemon restart and produce false positives. `None` when the workdir
    /// isn't a git repo or the spec hasn't started yet.
    #[serde(default)]
    pub spec_start_head: Option<String>,
    /// Optional workdir tag for backlog filtering only (`spec_list`). It does
    /// not drive execution — the run that eventually assigns this spec to a
    /// loop decides the actual workdir.
    #[serde(default)]
    pub workdir: Option<String>,
    /// How the spec was last transitioned to its current status (`admin` for
    /// administrative transitions). `None` means the status change was
    /// engine-driven or the spec was never administratively touched.
    #[serde(default)]
    pub completed_via: Option<String>,
    /// Reason for the most recent administrative status transition.
    #[serde(default)]
    pub completed_via_reason: Option<String>,
    /// Timestamp of the most recent administrative status transition.
    #[serde(default)]
    pub completed_via_at: Option<DateTime<Utc>>,
}

/// A node in either a spec's graph or a loop's top-level graph.
///
/// Exactly one of `spec_id`/`loop_id` is set — enforced by the DB layer (see
/// [`crate::db::Database::insert_loop_node`]) rather than by this type, since
/// callers build a `LoopNode` before it has been validated against the DB.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LoopNode {
    pub id: String,
    pub spec_id: Option<String>,
    pub loop_id: Option<String>,
    pub name: String,
    pub kind: LoopNodeKind,
    pub config: Value,
    pub position: i64,
    pub created_at: DateTime<Utc>,
}

/// An edge in either a spec's graph or a loop's top-level graph.
///
/// Exactly one of `spec_id`/`loop_id` is set — same invariant as
/// [`LoopNode`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LoopEdge {
    pub id: String,
    pub spec_id: Option<String>,
    pub loop_id: Option<String>,
    pub from_node: String,
    pub to_node: String,
    pub condition: LoopEdgeCondition,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LoopNodeRun {
    pub id: String,
    pub loop_id: String,
    pub spec_id: String,
    pub node_id: String,
    pub status: LoopRunStatus,
    pub input: Option<Value>,
    pub output: Option<Value>,
    pub started_at: DateTime<Utc>,
    pub completed_at: Option<DateTime<Utc>>,
    pub iteration: i64,
    /// PID of the OS process group currently executing this node run (the
    /// spawned child is always its own process-group leader — see
    /// `process_group(0)` at the spawn sites), so the engine can `killpg` it
    /// on any abnormal end. `None` once the run is finalized, or if no
    /// process was ever spawned for it (e.g. a gate node).
    pub pid: Option<i64>,
    /// `system::boot_id()` at the moment `pid` was recorded. A PID alone
    /// can't tell a live survivor from an unrelated process that reused the
    /// same PID after a reboot recycled the PID space — only meaningful
    /// together with a matching current boot id. See B12.
    pub boot_id: Option<String>,
    /// The harness session id that served this node run, captured per
    /// platform metadata (RS1): generated and set at spawn for platforms
    /// that accept a caller-chosen id (`session_id_set_flag`), or read back
    /// from the platform's session listing after the run
    /// (`session_list_cmd`). `None` for platforms that expose no session
    /// identity, and for every run recorded before this field existed. The
    /// foundation for resume mode (RS2): without it there is nothing to
    /// resume.
    pub session_id: Option<String>,
}

/// One firing of a loop's `on_completed` hook (N2). Deliberately its own
/// table/type rather than a `LoopNodeRun` — a hook run belongs to no spec and
/// no graph node (`loop_runs.spec_id`/`node_id` are `NOT NULL` FKs into
/// exactly those), and its outcome must never feed back into the run's
/// routing or final status the way a node run's does.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LoopCompletionHookRun {
    pub id: String,
    pub loop_id: String,
    pub status: LoopRunStatus,
    pub output: Option<Value>,
    pub summary: Option<String>,
    pub started_at: DateTime<Utc>,
    pub completed_at: Option<DateTime<Utc>>,
    /// Same B12 kill-on-abnormal-end treatment as [`LoopNodeRun::pid`].
    pub pid: Option<i64>,
    pub boot_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LoopSpecDetails {
    pub spec: LoopSpec,
    pub nodes: Vec<LoopNode>,
    pub edges: Vec<LoopEdge>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LoopDetails {
    pub lp: Loop,
    /// The loop-level graph: nodes/edges that target the loop directly
    /// (`loop_id`) rather than any one spec. Defined once per loop instead of
    /// being repeated across every spec.
    pub graph_nodes: Vec<LoopNode>,
    pub graph_edges: Vec<LoopEdge>,
    pub specs: Vec<LoopSpecDetails>,
    /// Every past firing of `on_completed` (oldest first) — populated only
    /// when the loop has completed at least once with a hook configured.
    pub completion_hook_runs: Vec<LoopCompletionHookRun>,
}

/// An ensemble (F1): a group of homogeneous agent-node members that receive
/// the same shared prompt in parallel, plus the join gate that waits for
/// every member, consolidates their outputs, and routes onward. Persisted as
/// its own row so `loop_get`/`loop_update_ensemble` can address the whole
/// unit — the members and join themselves are ordinary [`LoopNode`] rows
/// (see [`EnsembleMember`]), wired with ordinary [`LoopEdge`] rows, so the
/// engine's existing graph-walking code needs only the ensemble-aware
/// fan-out/fan-in added in `loop_engine`.
///
/// Exactly one of `spec_id`/`loop_id` is set — same invariant as
/// [`LoopNode`]/[`LoopEdge`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Ensemble {
    pub id: String,
    pub spec_id: Option<String>,
    pub loop_id: Option<String>,
    pub name: String,
    /// The one shared prompt every member renders against — supports the
    /// same placeholders as an agent node's `prompt_template`.
    pub prompt_template: String,
    /// The engine-executed [`LoopNodeKind::Join`] node that waits for every
    /// member and consolidates their outputs.
    pub join_node_id: String,
    /// The node this ensemble is wired from — every member gets an incoming
    /// edge from this node with `entry_condition`.
    pub entry_from_node: String,
    pub entry_condition: LoopEdgeCondition,
    /// Members required to pass for the join to report `pass`. Defaults to
    /// every member (set at creation to `members.len()`).
    pub min_pass: i64,
    /// Minutes a member may run before the join kills it (B12) and counts it
    /// as failed. `None` means "use `timeout_minutes`" (the members' own
    /// agent timeout) — see [`Self::effective_straggler_timeout_minutes`].
    pub straggler_timeout_minutes: Option<i64>,
    /// Shared agent timeout (minutes) applied to every member's node config.
    pub timeout_minutes: i64,
    /// Join-node outgoing routing: where a `pass`/`fail` join result routes
    /// to next. `on_pass_to` is required at creation; `on_fail_to` is
    /// optional (a dead end on fail, same as any other node with no
    /// matching outgoing edge).
    pub on_pass_to: String,
    pub on_fail_to: Option<String>,
    pub created_at: DateTime<Utc>,
}

impl Ensemble {
    /// The straggler kill timeout to actually use: the explicit override, or
    /// (by default) the members' own agent timeout.
    pub fn effective_straggler_timeout_minutes(&self) -> i64 {
        self.straggler_timeout_minutes
            .unwrap_or(self.timeout_minutes)
    }
}

/// One homogeneous member of an [`Ensemble`] — differs from its siblings
/// only in `platform`/`model`; `node_id` points at the underlying
/// [`LoopNodeKind::Agent`] row that actually executes.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EnsembleMember {
    pub ensemble_id: String,
    pub node_id: String,
    /// Position within the ensemble (0-based) — the deterministic order used
    /// for consolidation and for keying resize diffs in
    /// `loop_update_ensemble`.
    pub position: i64,
    pub platform: String,
    pub model: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EnsembleDetails {
    pub ensemble: Ensemble,
    /// Members in `position` order — the order consolidation and
    /// `loop_update_ensemble` resize diffs rely on.
    pub members: Vec<EnsembleMember>,
}

#[cfg(test)]
mod tests {
    use super::{
        validate_spec_description_template, LoopEdgeCondition, LoopNodeKind, LoopResetOutcome,
        LoopRunStatus, LoopSpecStatus, LoopStatus, SpecAdminStatusOutcome,
    };

    #[test]
    fn spec_template_validation_accepts_markdown_sections() {
        let description = r#"
## Functional Requirements
- Login with email and password

## Non-Functional Requirements
- Keep current latency profile

## Objective
Implement auth flow

## Constraints
Respect existing daemon architecture

## Guidelines
Reuse current MCP patterns

## In Scope
Backend loop tools

## Out of Scope
New TUI panel
"#;

        assert!(validate_spec_description_template(description).is_ok());
    }

    #[test]
    fn spec_template_validation_accepts_spanish_aliases() {
        let description = r#"
Requerimientos funcionales:
- Crear el flujo

Requerimientos no funcionales:
- Mantener compatibilidad

Objetivo:
- Encadenar agentes

Qué respetar:
- No romper tools actuales

Lineamientos:
- Reusar el daemon

Qué sí:
- Backend y MCP

Qué no:
- TUI nueva
"#;

        assert!(validate_spec_description_template(description).is_ok());
    }

    #[test]
    fn spec_template_validation_rejects_missing_sections() {
        let description = r#"
Functional Requirements:
- Something

Task:
- Something else
"#;

        let error = validate_spec_description_template(description).unwrap_err();

        assert!(error.contains("non-functional requirements"));
        assert!(error.contains("constraints"));
        assert!(error.contains("guidelines"));
        assert!(error.contains("in scope"));
        assert!(error.contains("out of scope"));
    }

    #[test]
    fn loop_status_as_str_roundtrip() {
        assert_eq!(LoopStatus::Draft.as_str(), "draft");
        assert_eq!(LoopStatus::Running.as_str(), "running");
        assert_eq!(LoopStatus::Paused.as_str(), "paused");
        assert_eq!(LoopStatus::Completed.as_str(), "completed");
        assert_eq!(LoopStatus::Failed.as_str(), "failed");
    }

    #[test]
    fn loop_status_from_str() {
        assert_eq!(LoopStatus::from_str("running"), LoopStatus::Running);
        assert_eq!(LoopStatus::from_str("paused"), LoopStatus::Paused);
        assert_eq!(LoopStatus::from_str("completed"), LoopStatus::Completed);
        assert_eq!(LoopStatus::from_str("failed"), LoopStatus::Failed);
        assert_eq!(LoopStatus::from_str("invalid"), LoopStatus::Draft);
    }

    #[test]
    fn loop_spec_status_as_str() {
        assert_eq!(LoopSpecStatus::Pending.as_str(), "pending");
        assert_eq!(LoopSpecStatus::Running.as_str(), "running");
        assert_eq!(LoopSpecStatus::Completed.as_str(), "completed");
        assert_eq!(LoopSpecStatus::Failed.as_str(), "failed");
        assert_eq!(LoopSpecStatus::Skipped.as_str(), "skipped");
    }

    #[test]
    fn loop_spec_status_from_str() {
        assert_eq!(LoopSpecStatus::from_str("running"), LoopSpecStatus::Running);
        assert_eq!(
            LoopSpecStatus::from_str("completed"),
            LoopSpecStatus::Completed
        );
        assert_eq!(LoopSpecStatus::from_str("failed"), LoopSpecStatus::Failed);
        assert_eq!(LoopSpecStatus::from_str("skipped"), LoopSpecStatus::Skipped);
        assert_eq!(LoopSpecStatus::from_str("invalid"), LoopSpecStatus::Pending);
    }

    #[test]
    fn loop_node_kind_as_str() {
        assert_eq!(LoopNodeKind::Agent.as_str(), "agent");
        assert_eq!(LoopNodeKind::Check.as_str(), "check");
        assert_eq!(LoopNodeKind::Gate.as_str(), "gate");
        assert_eq!(LoopNodeKind::Join.as_str(), "join");
    }

    #[test]
    fn loop_node_kind_from_str() {
        assert_eq!(LoopNodeKind::from_str("agent"), Some(LoopNodeKind::Agent));
        assert_eq!(LoopNodeKind::from_str("check"), Some(LoopNodeKind::Check));
        assert_eq!(LoopNodeKind::from_str("gate"), Some(LoopNodeKind::Gate));
        assert_eq!(LoopNodeKind::from_str("join"), Some(LoopNodeKind::Join));
        assert!(LoopNodeKind::from_str("invalid").is_none());
    }

    #[test]
    fn loop_node_kind_display_str() {
        assert_eq!(LoopNodeKind::Agent.display_str(), "agent");
        assert_eq!(LoopNodeKind::Check.display_str(), "check");
        assert_eq!(LoopNodeKind::Gate.display_str(), "gate");
        assert_eq!(LoopNodeKind::Join.display_str(), "quorum");
    }

    #[test]
    fn ensemble_straggler_timeout_defaults_to_member_timeout() {
        let ensemble = super::Ensemble {
            id: "ens1".to_string(),
            spec_id: Some("spec1".to_string()),
            loop_id: None,
            name: "Proposers".to_string(),
            prompt_template: "{{spec_content}}".to_string(),
            join_node_id: "join1".to_string(),
            entry_from_node: "n0".to_string(),
            entry_condition: LoopEdgeCondition::Always,
            min_pass: 2,
            straggler_timeout_minutes: None,
            timeout_minutes: 30,
            on_pass_to: "arbiter".to_string(),
            on_fail_to: None,
            created_at: chrono::Utc::now(),
        };
        assert_eq!(ensemble.effective_straggler_timeout_minutes(), 30);

        let mut overridden = ensemble;
        overridden.straggler_timeout_minutes = Some(5);
        assert_eq!(overridden.effective_straggler_timeout_minutes(), 5);
    }

    #[test]
    fn loop_edge_condition_as_str() {
        assert_eq!(LoopEdgeCondition::Pass.as_str(), "pass");
        assert_eq!(LoopEdgeCondition::Fail.as_str(), "fail");
        assert_eq!(LoopEdgeCondition::Always.as_str(), "always");
    }

    #[test]
    fn loop_edge_condition_from_str() {
        assert_eq!(
            LoopEdgeCondition::from_str("pass"),
            Some(LoopEdgeCondition::Pass)
        );
        assert_eq!(
            LoopEdgeCondition::from_str("fail"),
            Some(LoopEdgeCondition::Fail)
        );
        assert_eq!(
            LoopEdgeCondition::from_str("always"),
            Some(LoopEdgeCondition::Always)
        );
        assert!(LoopEdgeCondition::from_str("invalid").is_none());
    }

    #[test]
    fn loop_run_status_as_str() {
        assert_eq!(LoopRunStatus::Running.as_str(), "running");
        assert_eq!(LoopRunStatus::Pass.as_str(), "pass");
        assert_eq!(LoopRunStatus::Fail.as_str(), "fail");
    }

    #[test]
    fn loop_run_status_from_str() {
        assert_eq!(LoopRunStatus::from_str("pass"), LoopRunStatus::Pass);
        assert_eq!(LoopRunStatus::from_str("fail"), LoopRunStatus::Fail);
        assert_eq!(LoopRunStatus::from_str("invalid"), LoopRunStatus::Running);
    }

    fn loop_with_trigger(status: LoopStatus, trigger: Option<super::Trigger>) -> super::Loop {
        super::Loop {
            id: "wf".to_string(),
            name: "Loop".to_string(),
            description: None,
            workdir: "/tmp".to_string(),
            status,
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
    fn manual_loop_has_no_schedule_and_is_not_cron_or_watch() {
        let lp = loop_with_trigger(LoopStatus::Draft, None);
        assert_eq!(lp.trigger_type_label(), "manual");
        assert_eq!(lp.schedule_expr(), None);
        assert!(!lp.is_cron());
        assert!(!lp.is_watch());
        assert_eq!(lp.watch_path(), None);
    }

    #[test]
    fn cron_loop_exposes_schedule_expr() {
        let lp = loop_with_trigger(
            LoopStatus::Draft,
            Some(super::Trigger::Cron {
                schedule_expr: "30 8 * * *".to_string(),
            }),
        );
        assert_eq!(lp.trigger_type_label(), "cron");
        assert_eq!(lp.schedule_expr(), Some("30 8 * * *"));
        assert!(lp.is_cron());
        assert!(!lp.is_watch());
    }

    #[test]
    fn watch_loop_exposes_path_and_events() {
        let lp = loop_with_trigger(
            LoopStatus::Draft,
            Some(super::Trigger::Watch {
                path: "/tmp/watch".to_string(),
                events: vec![super::WatchEvent::Create],
                debounce_seconds: 2,
                recursive: false,
            }),
        );
        assert_eq!(lp.trigger_type_label(), "watch");
        assert!(lp.is_watch());
        assert_eq!(lp.watch_path(), Some("/tmp/watch"));
        assert_eq!(lp.watch_events(), Some(&[super::WatchEvent::Create][..]));
    }

    #[test]
    fn running_or_paused_loop_is_not_fireable() {
        // A trigger must not relaunch a loop that is already executing.
        assert!(!loop_with_trigger(LoopStatus::Running, None).is_fireable());
        assert!(!loop_with_trigger(LoopStatus::Paused, None).is_fireable());
        assert!(loop_with_trigger(LoopStatus::Draft, None).is_fireable());
        assert!(loop_with_trigger(LoopStatus::Completed, None).is_fireable());
        assert!(loop_with_trigger(LoopStatus::Failed, None).is_fireable());
    }

    #[test]
    fn future_autorun_at_is_not_due() {
        let mut lp = loop_with_trigger(LoopStatus::Failed, None);
        lp.autorun_at = Some(chrono::Utc::now() + chrono::Duration::hours(1));
        assert!(!lp.is_autorun_due(chrono::Utc::now()));
    }

    #[test]
    fn past_autorun_at_is_due_on_a_fireable_loop() {
        let mut lp = loop_with_trigger(LoopStatus::Failed, None);
        lp.autorun_at = Some(chrono::Utc::now() - chrono::Duration::minutes(1));
        assert!(lp.is_autorun_due(chrono::Utc::now()));
    }

    #[test]
    fn no_autorun_at_is_never_due() {
        let lp = loop_with_trigger(LoopStatus::Failed, None);
        assert!(!lp.is_autorun_due(chrono::Utc::now()));
    }

    #[test]
    fn past_autorun_at_is_not_due_while_running_or_paused() {
        for status in [LoopStatus::Running, LoopStatus::Paused] {
            let mut lp = loop_with_trigger(status, None);
            lp.autorun_at = Some(chrono::Utc::now() - chrono::Duration::minutes(1));
            assert!(
                !lp.is_autorun_due(chrono::Utc::now()),
                "{status:?} loop must not fire autorun_at"
            );
        }
    }

    #[test]
    fn past_auto_continue_at_is_due_while_paused() {
        let mut lp = loop_with_trigger(LoopStatus::Paused, None);
        lp.auto_continue_at = Some(chrono::Utc::now() - chrono::Duration::minutes(1));
        assert!(lp.is_auto_continue_due(chrono::Utc::now()));
    }

    #[test]
    fn future_auto_continue_at_is_not_due() {
        let mut lp = loop_with_trigger(LoopStatus::Paused, None);
        lp.auto_continue_at = Some(chrono::Utc::now() + chrono::Duration::hours(1));
        assert!(!lp.is_auto_continue_due(chrono::Utc::now()));
    }

    #[test]
    fn no_auto_continue_at_is_never_due() {
        let lp = loop_with_trigger(LoopStatus::Paused, None);
        assert!(!lp.is_auto_continue_due(chrono::Utc::now()));
    }

    #[test]
    fn past_auto_continue_at_is_not_due_while_not_paused() {
        for status in [
            LoopStatus::Draft,
            LoopStatus::Running,
            LoopStatus::Completed,
            LoopStatus::Failed,
        ] {
            let mut lp = loop_with_trigger(status, None);
            lp.auto_continue_at = Some(chrono::Utc::now() - chrono::Duration::minutes(1));
            assert!(
                !lp.is_auto_continue_due(chrono::Utc::now()),
                "{status:?} loop must not fire auto_continue_at"
            );
            assert!(
                lp.is_auto_continue_time_reached(chrono::Utc::now()),
                "{status:?} loop's auto_continue_at time itself must still register as reached \
                 so the scheduler can clear the stale schedule"
            );
        }
    }

    // ── validate_spec_description_template: edge cases ──────────────────

    #[test]
    fn spec_template_empty_input_fails() {
        let err = validate_spec_description_template("").unwrap_err();
        assert!(err.contains("must include sections"));
    }

    #[test]
    fn spec_template_whitespace_only_input_fails() {
        let err = validate_spec_description_template("   \n  \t  ").unwrap_err();
        assert!(err.contains("must include sections"));
    }

    #[test]
    fn spec_template_missing_all_sections_fails() {
        let err = validate_spec_description_template("Just some random text").unwrap_err();
        assert!(err.contains("missing required sections"));
        assert!(err.contains("functional requirements"));
        assert!(err.contains("non-functional requirements"));
        assert!(err.contains("objective / expected outcome"));
        assert!(err.contains("constraints"));
        assert!(err.contains("guidelines"));
        assert!(err.contains("in scope"));
        assert!(err.contains("out of scope"));
    }

    #[test]
    fn spec_template_only_one_section_fails_with_rest_missing() {
        let desc = "Functional Requirements:\n- Something\n";
        let err = validate_spec_description_template(desc).unwrap_err();
        let missing_part = err.split("missing required sections:").nth(1).unwrap();
        let missing_list = missing_part.split('.').next().unwrap().trim();
        assert!(missing_list.contains("non-functional requirements"));
        assert!(missing_list.contains("constraints"));
        assert!(missing_list.contains("guidelines"));
        assert!(missing_list.contains("in scope"));
        assert!(missing_list.contains("out of scope"));
        assert!(missing_list.contains("objective / expected outcome"));
    }

    #[test]
    fn spec_template_case_insensitive_matching() {
        let desc = r#"
FUNCTIONAL REQUIREMENTS:
- Build the thing

NON-FUNCTIONAL REQUIREMENTS:
- Keep it fast

OBJECTIVE:
Ship it

CONSTRAINTS:
Don't break prod

GUIDELINES:
Keep it simple

IN SCOPE:
Backend

OUT OF SCOPE:
Frontend
"#;
        assert!(validate_spec_description_template(desc).is_ok());
    }

    #[test]
    fn spec_template_mixed_case_matches() {
        let desc = r#"
Functional requirements:
- Build

Non-Functional Requirements:
- Speed

Objective:
Done

Constraints:
Safe

Guidelines:
Clean

In Scope:
API

Out of Scope:
UI
"#;
        assert!(validate_spec_description_template(desc).is_ok());
    }

    #[test]
    fn spec_template_h3_markdown_headings_match() {
        let desc = r#"
### Functional Requirements
- Build

### Non-Functional Requirements
- Fast

### Objective
Ship

### Constraints
Safe

### Guidelines
Simple

### In Scope
API

### Out of Scope
UI
"#;
        assert!(validate_spec_description_template(desc).is_ok());
    }

    #[test]
    fn spec_template_h2_markdown_headings_match() {
        let desc = r#"
## Functional Requirements
- Build

## Non-Functional Requirements
- Fast

## Objective
Ship

## Constraints
Safe

## Guidelines
Simple

## In Scope
API

## Out of Scope
UI
"#;
        assert!(validate_spec_description_template(desc).is_ok());
    }

    #[test]
    fn spec_template_dash_list_markers_match() {
        let desc = r#"
- Functional Requirements:
  - Build

- Non-Functional Requirements:
  - Fast

- Objective:
  - Ship

- Constraints:
  - Safe

- Guidelines:
  - Simple

- In Scope:
  - API

- Out of Scope:
  - UI
"#;
        assert!(validate_spec_description_template(desc).is_ok());
    }

    #[test]
    fn spec_template_star_list_markers_match() {
        let desc = r#"
* Functional Requirements:
  * Build

* Non-Functional Requirements:
  * Fast

* Objective:
  * Ship

* Constraints:
  * Safe

* Guidelines:
  * Simple

* In Scope:
  * API

* Out of Scope:
  * UI
"#;
        assert!(validate_spec_description_template(desc).is_ok());
    }

    // ── validate_spec_description_template: individual alias matching ───

    fn full_desc_with(section_index: usize, alias: &str) -> String {
        let sections = [
            ("Functional Requirements", "Build"),
            ("Non-Functional Requirements", "Fast"),
            ("Objective", "Done"),
            ("Constraints", "Safe"),
            ("Guidelines", "Simple"),
            ("In Scope", "API"),
            ("Out of Scope", "UI"),
        ];
        let mut lines = Vec::new();
        for (i, (canonical, value)) in sections.iter().enumerate() {
            let heading = if i == section_index { alias } else { canonical };
            lines.push(format!("{heading}:"));
            lines.push(format!("- {value}"));
        }
        lines.join("\n")
    }

    #[test]
    fn spec_template_requerimientos_funcionales_matches() {
        let desc = full_desc_with(0, "Requerimientos funcionales");
        assert!(validate_spec_description_template(&desc).is_ok());
    }

    #[test]
    fn spec_template_requisitos_funcionales_matches() {
        let desc = full_desc_with(0, "Requisitos funcionales");
        assert!(validate_spec_description_template(&desc).is_ok());
    }

    #[test]
    fn spec_template_non_functional_requirements_matches() {
        let desc = full_desc_with(1, "Non-Functional Requirements");
        assert!(validate_spec_description_template(&desc).is_ok());
    }

    #[test]
    fn spec_template_non_functional_no_hyphen_matches() {
        let desc = full_desc_with(1, "Non functional requirements");
        assert!(validate_spec_description_template(&desc).is_ok());
    }

    #[test]
    fn spec_template_requerimientos_no_funcionales_matches() {
        let desc = full_desc_with(1, "Requerimientos no funcionales");
        assert!(validate_spec_description_template(&desc).is_ok());
    }

    #[test]
    fn spec_template_requisitos_no_funcionales_matches() {
        let desc = full_desc_with(1, "Requisitos no funcionales");
        assert!(validate_spec_description_template(&desc).is_ok());
    }

    #[test]
    fn spec_template_objective_matches() {
        let desc = full_desc_with(2, "Objective");
        assert!(validate_spec_description_template(&desc).is_ok());
    }

    #[test]
    fn spec_template_expected_outcome_matches() {
        let desc = full_desc_with(2, "Expected Outcome");
        assert!(validate_spec_description_template(&desc).is_ok());
    }

    #[test]
    fn spec_template_objetivo_matches() {
        let desc = full_desc_with(2, "Objetivo");
        assert!(validate_spec_description_template(&desc).is_ok());
    }

    #[test]
    fn spec_template_resultado_esperado_matches() {
        let desc = full_desc_with(2, "Resultado esperado");
        assert!(validate_spec_description_template(&desc).is_ok());
    }

    #[test]
    fn spec_template_constraints_matches() {
        let desc = full_desc_with(3, "Constraints");
        assert!(validate_spec_description_template(&desc).is_ok());
    }

    #[test]
    fn spec_template_what_to_respect_matches() {
        let desc = full_desc_with(3, "What to respect");
        assert!(validate_spec_description_template(&desc).is_ok());
    }

    #[test]
    fn spec_template_restrictions_matches() {
        let desc = full_desc_with(3, "Restrictions");
        assert!(validate_spec_description_template(&desc).is_ok());
    }

    #[test]
    fn spec_template_restricciones_matches() {
        let desc = full_desc_with(3, "Restricciones");
        assert!(validate_spec_description_template(&desc).is_ok());
    }

    #[test]
    fn spec_template_que_respetar_matches() {
        let desc = full_desc_with(3, "Que respetar");
        assert!(validate_spec_description_template(&desc).is_ok());
    }

    #[test]
    fn spec_template_que_respetar_accent_matches() {
        let desc = full_desc_with(3, "Qué respetar");
        assert!(validate_spec_description_template(&desc).is_ok());
    }

    #[test]
    fn spec_template_guidelines_matches() {
        let desc = full_desc_with(4, "Guidelines");
        assert!(validate_spec_description_template(&desc).is_ok());
    }

    #[test]
    fn spec_template_lineamientos_matches() {
        let desc = full_desc_with(4, "Lineamientos");
        assert!(validate_spec_description_template(&desc).is_ok());
    }

    #[test]
    fn spec_template_guidance_matches() {
        let desc = full_desc_with(4, "Guidance");
        assert!(validate_spec_description_template(&desc).is_ok());
    }

    #[test]
    fn spec_template_lineas_guia_matches() {
        let desc = full_desc_with(4, "Lineas guia");
        assert!(validate_spec_description_template(&desc).is_ok());
    }

    #[test]
    fn spec_template_lineas_guia_accent_matches() {
        let desc = full_desc_with(4, "Líneas guía");
        assert!(validate_spec_description_template(&desc).is_ok());
    }

    #[test]
    fn spec_template_in_scope_matches() {
        let desc = full_desc_with(5, "In Scope");
        assert!(validate_spec_description_template(&desc).is_ok());
    }

    #[test]
    fn spec_template_scope_in_matches() {
        let desc = full_desc_with(5, "Scope In");
        assert!(validate_spec_description_template(&desc).is_ok());
    }

    #[test]
    fn spec_template_que_si_matches() {
        let desc = full_desc_with(5, "Que si");
        assert!(validate_spec_description_template(&desc).is_ok());
    }

    #[test]
    fn spec_template_que_si_accent_matches() {
        let desc = full_desc_with(5, "Qué sí");
        assert!(validate_spec_description_template(&desc).is_ok());
    }

    #[test]
    fn spec_template_incluye_matches() {
        let desc = full_desc_with(5, "Incluye");
        assert!(validate_spec_description_template(&desc).is_ok());
    }

    #[test]
    fn spec_template_out_of_scope_matches() {
        let desc = full_desc_with(6, "Out of Scope");
        assert!(validate_spec_description_template(&desc).is_ok());
    }

    #[test]
    fn spec_template_scope_out_matches() {
        let desc = full_desc_with(6, "Scope Out");
        assert!(validate_spec_description_template(&desc).is_ok());
    }

    #[test]
    fn spec_template_que_no_matches() {
        let desc = full_desc_with(6, "Que no");
        assert!(validate_spec_description_template(&desc).is_ok());
    }

    #[test]
    fn spec_template_que_no_accent_matches() {
        let desc = full_desc_with(6, "Qué no");
        assert!(validate_spec_description_template(&desc).is_ok());
    }

    #[test]
    fn spec_template_excluye_matches() {
        let desc = full_desc_with(6, "Excluye");
        assert!(validate_spec_description_template(&desc).is_ok());
    }

    // ── validate_spec_description_template: newline variant matching ────

    #[test]
    fn spec_template_alias_with_newline_matches() {
        let desc = full_desc_with(2, "objective");
        assert!(validate_spec_description_template(&desc).is_ok());
    }

    #[test]
    fn spec_template_alias_without_colon_or_newline_fails() {
        let desc = "objective some extra text after it without colon\n";
        let err = validate_spec_description_template(desc).unwrap_err();
        assert!(err.contains("objective / expected outcome"));
    }

    // ── validate_spec_description_template: error message format ────────

    #[test]
    fn spec_template_error_contains_expected_sections_list() {
        let err = validate_spec_description_template("Hello").unwrap_err();
        assert!(err.contains("Expected sections:"));
        assert!(err.contains("functional requirements"));
        assert!(err.contains("non-functional requirements"));
        assert!(err.contains("objective / expected outcome"));
        assert!(err.contains("constraints"));
        assert!(err.contains("guidelines"));
        assert!(err.contains("in scope"));
        assert!(err.contains("out of scope"));
    }

    #[test]
    fn spec_template_error_lists_only_missing_sections() {
        let desc = r#"
Functional Requirements:
- Build

Non-Functional Requirements:
- Fast

Objective:
- Done

Constraints:
- Safe

Guidelines:
- Simple

In Scope:
- API
"#;
        let err = validate_spec_description_template(desc).unwrap_err();
        // "missing required sections:" should list only "out of scope"
        let missing_part = err.split("missing required sections:").nth(1).unwrap();
        let missing_list = missing_part.split('.').next().unwrap().trim();
        assert_eq!(missing_list, "out of scope");
    }

    // ── required_spec_section_names ─────────────────────────────────────

    #[test]
    fn required_section_names_contains_all_canonical_names() {
        let names = super::required_spec_section_names();
        assert!(names.contains("functional requirements"));
        assert!(names.contains("non-functional requirements"));
        assert!(names.contains("objective / expected outcome"));
        assert!(names.contains("constraints"));
        assert!(names.contains("guidelines"));
        assert!(names.contains("in scope"));
        assert!(names.contains("out of scope"));
    }

    #[test]
    fn required_section_names_has_seven_entries() {
        let names = super::required_spec_section_names();
        let count = names.split(", ").count();
        assert_eq!(count, 7);
    }

    // ── LoopStatus: as_str / from_str roundtrip ─────────────────────────

    #[test]
    fn loop_status_as_str_from_str_roundtrip() {
        let statuses = [
            LoopStatus::Draft,
            LoopStatus::Running,
            LoopStatus::Paused,
            LoopStatus::Completed,
            LoopStatus::Failed,
        ];
        for s in statuses {
            let s_str = s.as_str();
            assert_eq!(
                LoopStatus::from_str(s_str),
                s,
                "roundtrip failed for {s_str}"
            );
        }
    }

    #[test]
    fn loop_status_from_str_unknown_defaults_to_draft() {
        let unknowns = ["", "DRAFT", "Running", "PENDING", "unknown", "123"];
        for input in unknowns {
            assert_eq!(
                LoopStatus::from_str(input),
                LoopStatus::Draft,
                "expected Draft for {input:?}"
            );
        }
    }

    #[test]
    fn loop_status_as_str_returns_lowercase() {
        for s in [
            LoopStatus::Draft,
            LoopStatus::Running,
            LoopStatus::Paused,
            LoopStatus::Completed,
            LoopStatus::Failed,
        ] {
            assert_eq!(s.as_str(), s.as_str().to_lowercase());
        }
    }

    // ── LoopSpecStatus: as_str / from_str roundtrip ─────────────────────

    #[test]
    fn loop_spec_status_as_str_from_str_roundtrip() {
        let statuses = [
            LoopSpecStatus::Pending,
            LoopSpecStatus::Running,
            LoopSpecStatus::Completed,
            LoopSpecStatus::Failed,
            LoopSpecStatus::Skipped,
        ];
        for s in statuses {
            let s_str = s.as_str();
            assert_eq!(
                LoopSpecStatus::from_str(s_str),
                s,
                "roundtrip failed for {s_str}"
            );
        }
    }

    #[test]
    fn loop_spec_status_from_str_unknown_defaults_to_pending() {
        let unknowns = ["", "PENDING", "Running", "DONE", "unknown", "xyz"];
        for input in unknowns {
            assert_eq!(
                LoopSpecStatus::from_str(input),
                LoopSpecStatus::Pending,
                "expected Pending for {input:?}"
            );
        }
    }

    #[test]
    fn loop_spec_status_as_str_returns_lowercase() {
        for s in [
            LoopSpecStatus::Pending,
            LoopSpecStatus::Running,
            LoopSpecStatus::Completed,
            LoopSpecStatus::Failed,
            LoopSpecStatus::Skipped,
        ] {
            assert_eq!(s.as_str(), s.as_str().to_lowercase());
        }
    }

    #[test]
    fn loop_spec_status_pending_is_distinct_from_running() {
        assert_ne!(
            LoopSpecStatus::Pending.as_str(),
            LoopSpecStatus::Running.as_str()
        );
    }

    // ── LoopNodeKind: as_str / from_str / display_str roundtrip ─────────

    #[test]
    fn loop_node_kind_as_str_from_str_roundtrip() {
        let kinds = [
            LoopNodeKind::Agent,
            LoopNodeKind::Check,
            LoopNodeKind::Gate,
            LoopNodeKind::Join,
        ];
        for k in kinds {
            let k_str = k.as_str();
            assert_eq!(
                LoopNodeKind::from_str(k_str),
                Some(k),
                "roundtrip failed for {k_str}"
            );
        }
    }

    #[test]
    fn loop_node_kind_from_str_invalid_returns_none() {
        let invalids = ["", "AGENT", "Agent", "ensemble", "unknown", "workflow"];
        for input in invalids {
            assert_eq!(
                LoopNodeKind::from_str(input),
                None,
                "expected None for {input:?}"
            );
        }
    }

    #[test]
    fn loop_node_kind_display_str_matches_as_str_except_join() {
        for k in [
            LoopNodeKind::Agent,
            LoopNodeKind::Check,
            LoopNodeKind::Gate,
        ] {
            assert_eq!(k.display_str(), k.as_str());
        }
        assert_eq!(LoopNodeKind::Join.display_str(), "quorum");
        assert_ne!(LoopNodeKind::Join.display_str(), LoopNodeKind::Join.as_str());
    }

    #[test]
    fn loop_node_kind_as_str_returns_lowercase() {
        for k in [
            LoopNodeKind::Agent,
            LoopNodeKind::Check,
            LoopNodeKind::Gate,
            LoopNodeKind::Join,
        ] {
            assert_eq!(k.as_str(), k.as_str().to_lowercase());
        }
    }

    // ── LoopEdgeCondition: as_str / from_str roundtrip ──────────────────

    #[test]
    fn loop_edge_condition_as_str_from_str_roundtrip() {
        let conds = [
            LoopEdgeCondition::Pass,
            LoopEdgeCondition::Fail,
            LoopEdgeCondition::Always,
        ];
        for c in conds {
            let c_str = c.as_str();
            assert_eq!(
                LoopEdgeCondition::from_str(c_str),
                Some(c),
                "roundtrip failed for {c_str}"
            );
        }
    }

    #[test]
    fn loop_edge_condition_from_str_invalid_returns_none() {
        let invalids = ["", "PASS", "Pass", "never", "sometimes", "123"];
        for input in invalids {
            assert_eq!(
                LoopEdgeCondition::from_str(input),
                None,
                "expected None for {input:?}"
            );
        }
    }

    #[test]
    fn loop_edge_condition_as_str_returns_lowercase() {
        for c in [
            LoopEdgeCondition::Pass,
            LoopEdgeCondition::Fail,
            LoopEdgeCondition::Always,
        ] {
            assert_eq!(c.as_str(), c.as_str().to_lowercase());
        }
    }

    // ── LoopRunStatus: as_str / from_str roundtrip ──────────────────────

    #[test]
    fn loop_run_status_as_str_from_str_roundtrip() {
        let statuses = [
            LoopRunStatus::Running,
            LoopRunStatus::Pass,
            LoopRunStatus::Fail,
        ];
        for s in statuses {
            let s_str = s.as_str();
            assert_eq!(
                LoopRunStatus::from_str(s_str),
                s,
                "roundtrip failed for {s_str}"
            );
        }
    }

    #[test]
    fn loop_run_status_from_str_unknown_defaults_to_running() {
        let unknowns = ["", "RUNNING", "Running", "done", "unknown", "42"];
        for input in unknowns {
            assert_eq!(
                LoopRunStatus::from_str(input),
                LoopRunStatus::Running,
                "expected Running for {input:?}"
            );
        }
    }

    #[test]
    fn loop_run_status_as_str_returns_lowercase() {
        for s in [
            LoopRunStatus::Running,
            LoopRunStatus::Pass,
            LoopRunStatus::Fail,
        ] {
            assert_eq!(s.as_str(), s.as_str().to_lowercase());
        }
    }

    // ── SpecAdminStatusOutcome: variant construction & equality ─────────

    #[test]
    fn spec_admin_status_outcome_success_variants() {
        let a = SpecAdminStatusOutcome::Success;
        assert!(matches!(a, SpecAdminStatusOutcome::Success));
    }

    #[test]
    fn spec_admin_status_outcome_not_found_variants() {
        let a = SpecAdminStatusOutcome::NotFound;
        assert!(matches!(a, SpecAdminStatusOutcome::NotFound));
    }

    #[test]
    fn spec_admin_status_outcome_not_standalone_carries_id() {
        let outcome = SpecAdminStatusOutcome::NotStandalone("spec-42".to_string());
        match outcome {
            SpecAdminStatusOutcome::NotStandalone(id) => assert_eq!(id, "spec-42"),
            _ => panic!("expected NotStandalone"),
        }
    }

    #[test]
    fn spec_admin_status_outcome_active_run_carries_ids() {
        let outcome = SpecAdminStatusOutcome::ActiveRun {
            loop_id: "loop-1".to_string(),
            run_id: "run-2".to_string(),
        };
        match outcome {
            SpecAdminStatusOutcome::ActiveRun { loop_id, run_id } => {
                assert_eq!(loop_id, "loop-1");
                assert_eq!(run_id, "run-2");
            }
            _ => panic!("expected ActiveRun"),
        }
    }

    #[test]
    fn spec_admin_status_outcome_variants_are_distinct() {
        let success = SpecAdminStatusOutcome::Success;
        let not_found = SpecAdminStatusOutcome::NotFound;
        let not_standalone = SpecAdminStatusOutcome::NotStandalone("x".to_string());
        let active_run = SpecAdminStatusOutcome::ActiveRun {
            loop_id: "a".to_string(),
            run_id: "b".to_string(),
        };
        assert_ne!(format!("{success:?}"), format!("{not_found:?}"));
        assert_ne!(format!("{not_standalone:?}"), format!("{active_run:?}"));
    }

    // ── LoopResetOutcome: variant construction & equality ───────────────

    #[test]
    fn loop_reset_outcome_not_found() {
        let a = LoopResetOutcome::NotFound;
        assert!(matches!(a, LoopResetOutcome::NotFound));
    }

    #[test]
    fn loop_reset_outcome_running() {
        let a = LoopResetOutcome::Running;
        assert!(matches!(a, LoopResetOutcome::Running));
    }

    #[test]
    fn loop_reset_outcome_invalid_spec_carries_id() {
        let outcome = LoopResetOutcome::InvalidSpec("bad-spec".to_string());
        match outcome {
            LoopResetOutcome::InvalidSpec(id) => assert_eq!(id, "bad-spec"),
            _ => panic!("expected InvalidSpec"),
        }
    }

    #[test]
    fn loop_reset_outcome_reset_carries_count() {
        let outcome = LoopResetOutcome::Reset { spec_count: 5 };
        match outcome {
            LoopResetOutcome::Reset { spec_count } => assert_eq!(spec_count, 5),
            _ => panic!("expected Reset"),
        }
    }

    #[test]
    fn loop_reset_outcome_variants_are_distinct() {
        let not_found = LoopResetOutcome::NotFound;
        let running = LoopResetOutcome::Running;
        let invalid = LoopResetOutcome::InvalidSpec("x".to_string());
        let reset = LoopResetOutcome::Reset { spec_count: 0 };
        assert_ne!(format!("{not_found:?}"), format!("{running:?}"));
        assert_ne!(format!("{invalid:?}"), format!("{reset:?}"));
    }

    // ── LoopStatus: equality & Debug ────────────────────────────────────

    #[test]
    fn loop_status_variants_are_distinct() {
        let all = [
            LoopStatus::Draft,
            LoopStatus::Running,
            LoopStatus::Paused,
            LoopStatus::Completed,
            LoopStatus::Failed,
        ];
        for i in 0..all.len() {
            for j in (i + 1)..all.len() {
                let a = all[i];
                let b = all[j];
                assert_ne!(a, b, "{a:?} should differ from {b:?}");
            }
        }
    }

    #[test]
    fn loop_status_debug_format_matches_variant_name() {
        assert_eq!(format!("{:?}", LoopStatus::Draft), "Draft");
        assert_eq!(format!("{:?}", LoopStatus::Running), "Running");
        assert_eq!(format!("{:?}", LoopStatus::Paused), "Paused");
        assert_eq!(format!("{:?}", LoopStatus::Completed), "Completed");
        assert_eq!(format!("{:?}", LoopStatus::Failed), "Failed");
    }

    // ── LoopSpecStatus: equality & Debug ────────────────────────────────

    #[test]
    fn loop_spec_status_variants_are_distinct() {
        let all = [
            LoopSpecStatus::Pending,
            LoopSpecStatus::Running,
            LoopSpecStatus::Completed,
            LoopSpecStatus::Failed,
            LoopSpecStatus::Skipped,
        ];
        for i in 0..all.len() {
            for j in (i + 1)..all.len() {
                let a = all[i];
                let b = all[j];
                assert_ne!(a, b, "{a:?} should differ from {b:?}");
            }
        }
    }

    // ── LoopNodeKind: equality & Debug ──────────────────────────────────

    #[test]
    fn loop_node_kind_variants_are_distinct() {
        let all = [
            LoopNodeKind::Agent,
            LoopNodeKind::Check,
            LoopNodeKind::Gate,
            LoopNodeKind::Join,
        ];
        for i in 0..all.len() {
            for j in (i + 1)..all.len() {
                let a = all[i];
                let b = all[j];
                assert_ne!(a, b, "{a:?} should differ from {b:?}");
            }
        }
    }

    // ── LoopEdgeCondition: equality & Debug ─────────────────────────────

    #[test]
    fn loop_edge_condition_variants_are_distinct() {
        let all = [
            LoopEdgeCondition::Pass,
            LoopEdgeCondition::Fail,
            LoopEdgeCondition::Always,
        ];
        for i in 0..all.len() {
            for j in (i + 1)..all.len() {
                let a = all[i];
                let b = all[j];
                assert_ne!(a, b, "{a:?} should differ from {b:?}");
            }
        }
    }

    // ── LoopRunStatus: equality & Debug ─────────────────────────────────

    #[test]
    fn loop_run_status_variants_are_distinct() {
        let all = [
            LoopRunStatus::Running,
            LoopRunStatus::Pass,
            LoopRunStatus::Fail,
        ];
        for i in 0..all.len() {
            for j in (i + 1)..all.len() {
                let a = all[i];
                let b = all[j];
                assert_ne!(a, b, "{a:?} should differ from {b:?}");
            }
        }
    }

    // ── Clone: all status enums are Clone ───────────────────────────────

    #[test]
    fn loop_status_is_clone() {
        let s = LoopStatus::Running;
        let cloned = s;
        assert_eq!(s, cloned);
    }

    #[test]
    fn loop_spec_status_is_clone() {
        let s = LoopSpecStatus::Failed;
        let cloned = s;
        assert_eq!(s, cloned);
    }

    #[test]
    fn loop_node_kind_is_clone() {
        let k = LoopNodeKind::Join;
        let cloned = k;
        assert_eq!(k, cloned);
    }

    #[test]
    fn loop_edge_condition_is_clone() {
        let c = LoopEdgeCondition::Always;
        let cloned = c;
        assert_eq!(c, cloned);
    }

    #[test]
    fn loop_run_status_is_clone() {
        let s = LoopRunStatus::Pass;
        let cloned = s;
        assert_eq!(s, cloned);
    }

    // ── Loop: is_fireable exhaustive coverage ───────────────────────────

    #[test]
    fn loop_is_fireable_exhaustive() {
        let expected_fireable = [
            (LoopStatus::Draft, true),
            (LoopStatus::Running, false),
            (LoopStatus::Paused, false),
            (LoopStatus::Completed, true),
            (LoopStatus::Failed, true),
        ];
        for (status, expected) in expected_fireable {
            let lp = loop_with_trigger(status, None);
            assert_eq!(
                lp.is_fireable(),
                expected,
                "{status:?}.is_fireable() should be {expected}"
            );
        }
    }

    // ── Loop: trigger_type_label exhaustive ─────────────────────────────

    #[test]
    fn loop_trigger_type_label_exhaustive() {
        let lp_manual = loop_with_trigger(LoopStatus::Draft, None);
        assert_eq!(lp_manual.trigger_type_label(), "manual");

        let lp_cron = loop_with_trigger(
            LoopStatus::Draft,
            Some(super::Trigger::Cron {
                schedule_expr: "* * * * *".to_string(),
            }),
        );
        assert_eq!(lp_cron.trigger_type_label(), "cron");

        let lp_watch = loop_with_trigger(
            LoopStatus::Draft,
            Some(super::Trigger::Watch {
                path: "/tmp".to_string(),
                events: vec![],
                debounce_seconds: 2,
                recursive: false,
            }),
        );
        assert_eq!(lp_watch.trigger_type_label(), "watch");
    }

    // ── Loop: schedule_expr / watch_path exhaustive ─────────────────────

    #[test]
    fn loop_schedule_expr_only_some_for_cron() {
        let cron = loop_with_trigger(
            LoopStatus::Draft,
            Some(super::Trigger::Cron {
                schedule_expr: "0 9 * * 1-5".to_string(),
            }),
        );
        assert_eq!(cron.schedule_expr(), Some("0 9 * * 1-5"));

        let manual = loop_with_trigger(LoopStatus::Draft, None);
        assert_eq!(manual.schedule_expr(), None);

        let watch = loop_with_trigger(
            LoopStatus::Draft,
            Some(super::Trigger::Watch {
                path: "/src".to_string(),
                events: vec![],
                debounce_seconds: 2,
                recursive: false,
            }),
        );
        assert_eq!(watch.schedule_expr(), None);
    }

    #[test]
    fn loop_watch_path_only_some_for_watch() {
        let watch = loop_with_trigger(
            LoopStatus::Draft,
            Some(super::Trigger::Watch {
                path: "/data".to_string(),
                events: vec![super::WatchEvent::Modify],
                debounce_seconds: 2,
                recursive: false,
            }),
        );
        assert_eq!(watch.watch_path(), Some("/data"));

        let cron = loop_with_trigger(
            LoopStatus::Draft,
            Some(super::Trigger::Cron {
                schedule_expr: "* * * * *".to_string(),
            }),
        );
        assert_eq!(cron.watch_path(), None);

        let manual = loop_with_trigger(LoopStatus::Draft, None);
        assert_eq!(manual.watch_path(), None);
    }

    // ── Loop: is_autorun_due edge cases ─────────────────────────────────

    #[test]
    fn loop_autorun_due_exactly_at_threshold() {
        let now = chrono::Utc::now();
        let mut lp = loop_with_trigger(LoopStatus::Completed, None);
        lp.autorun_at = Some(now);
        assert!(lp.is_autorun_due(now));
    }

    #[test]
    fn loop_autorun_not_due_one_second_before() {
        let now = chrono::Utc::now();
        let mut lp = loop_with_trigger(LoopStatus::Completed, None);
        lp.autorun_at = Some(now + chrono::Duration::seconds(1));
        assert!(!lp.is_autorun_due(now));
    }

    // ── Loop: is_auto_continue_due edge cases ───────────────────────────

    #[test]
    fn loop_auto_continue_due_exactly_at_threshold() {
        let now = chrono::Utc::now();
        let mut lp = loop_with_trigger(LoopStatus::Paused, None);
        lp.auto_continue_at = Some(now);
        assert!(lp.is_auto_continue_due(now));
    }

    #[test]
    fn loop_auto_continue_not_due_one_second_before() {
        let now = chrono::Utc::now();
        let mut lp = loop_with_trigger(LoopStatus::Paused, None);
        lp.auto_continue_at = Some(now + chrono::Duration::seconds(1));
        assert!(!lp.is_auto_continue_due(now));
    }

    #[test]
    fn loop_auto_continue_time_reached_independent_of_status() {
        let now = chrono::Utc::now();
        for status in [
            LoopStatus::Draft,
            LoopStatus::Running,
            LoopStatus::Paused,
            LoopStatus::Completed,
            LoopStatus::Failed,
        ] {
            let mut lp = loop_with_trigger(status, None);
            lp.auto_continue_at = Some(now - chrono::Duration::seconds(1));
            assert!(
                lp.is_auto_continue_time_reached(now),
                "{status:?}: time_reached should be true"
            );
        }
    }

    #[test]
    fn loop_auto_continue_time_not_reached_in_future() {
        let now = chrono::Utc::now();
        let mut lp = loop_with_trigger(LoopStatus::Paused, None);
        lp.auto_continue_at = Some(now + chrono::Duration::hours(1));
        assert!(!lp.is_auto_continue_time_reached(now));
    }

    #[test]
    fn loop_auto_continue_time_not_reached_when_none() {
        let lp = loop_with_trigger(LoopStatus::Paused, None);
        assert!(!lp.is_auto_continue_time_reached(chrono::Utc::now()));
    }

    // ── Loop: is_cron / is_watch exhaustive ─────────────────────────────

    #[test]
    fn loop_is_cron_and_is_watch_exhaustive() {
        let manual = loop_with_trigger(LoopStatus::Draft, None);
        assert!(!manual.is_cron());
        assert!(!manual.is_watch());

        let cron = loop_with_trigger(
            LoopStatus::Draft,
            Some(super::Trigger::Cron {
                schedule_expr: "* * * * *".to_string(),
            }),
        );
        assert!(cron.is_cron());
        assert!(!cron.is_watch());

        let watch = loop_with_trigger(
            LoopStatus::Draft,
            Some(super::Trigger::Watch {
                path: "/x".to_string(),
                events: vec![],
                debounce_seconds: 2,
                recursive: false,
            }),
        );
        assert!(!watch.is_cron());
        assert!(watch.is_watch());
    }

    // ── serde: LoopStatus roundtrip ─────────────────────────────────────

    #[test]
    fn loop_status_serde_roundtrip() {
        let statuses = [
            LoopStatus::Draft,
            LoopStatus::Running,
            LoopStatus::Paused,
            LoopStatus::Completed,
            LoopStatus::Failed,
        ];
        for s in statuses {
            let json = serde_json::to_string(&s).unwrap();
            let deserialized: LoopStatus = serde_json::from_str(&json).unwrap();
            assert_eq!(deserialized, s, "serde roundtrip failed for {json}");
        }
    }

    #[test]
    fn loop_status_serde_uses_snake_case() {
        assert_eq!(serde_json::to_string(&LoopStatus::Draft).unwrap(), "\"draft\"");
        assert_eq!(
            serde_json::to_string(&LoopStatus::Running).unwrap(),
            "\"running\""
        );
        assert_eq!(
            serde_json::to_string(&LoopStatus::Completed).unwrap(),
            "\"completed\""
        );
    }

    // ── serde: LoopSpecStatus roundtrip ─────────────────────────────────

    #[test]
    fn loop_spec_status_serde_roundtrip() {
        let statuses = [
            LoopSpecStatus::Pending,
            LoopSpecStatus::Running,
            LoopSpecStatus::Completed,
            LoopSpecStatus::Failed,
            LoopSpecStatus::Skipped,
        ];
        for s in statuses {
            let json = serde_json::to_string(&s).unwrap();
            let deserialized: LoopSpecStatus = serde_json::from_str(&json).unwrap();
            assert_eq!(deserialized, s, "serde roundtrip failed for {json}");
        }
    }

    #[test]
    fn loop_spec_status_serde_uses_snake_case() {
        assert_eq!(
            serde_json::to_string(&LoopSpecStatus::Pending).unwrap(),
            "\"pending\""
        );
        assert_eq!(
            serde_json::to_string(&LoopSpecStatus::Skipped).unwrap(),
            "\"skipped\""
        );
    }

    // ── serde: LoopNodeKind roundtrip ───────────────────────────────────

    #[test]
    fn loop_node_kind_serde_roundtrip() {
        let kinds = [
            LoopNodeKind::Agent,
            LoopNodeKind::Check,
            LoopNodeKind::Gate,
            LoopNodeKind::Join,
        ];
        for k in kinds {
            let json = serde_json::to_string(&k).unwrap();
            let deserialized: LoopNodeKind = serde_json::from_str(&json).unwrap();
            assert_eq!(deserialized, k, "serde roundtrip failed for {json}");
        }
    }

    #[test]
    fn loop_node_kind_serde_uses_snake_case() {
        assert_eq!(
            serde_json::to_string(&LoopNodeKind::Agent).unwrap(),
            "\"agent\""
        );
        assert_eq!(
            serde_json::to_string(&LoopNodeKind::Gate).unwrap(),
            "\"gate\""
        );
    }

    // ── serde: LoopEdgeCondition roundtrip ──────────────────────────────

    #[test]
    fn loop_edge_condition_serde_roundtrip() {
        let conds = [
            LoopEdgeCondition::Pass,
            LoopEdgeCondition::Fail,
            LoopEdgeCondition::Always,
        ];
        for c in conds {
            let json = serde_json::to_string(&c).unwrap();
            let deserialized: LoopEdgeCondition = serde_json::from_str(&json).unwrap();
            assert_eq!(deserialized, c, "serde roundtrip failed for {json}");
        }
    }

    #[test]
    fn loop_edge_condition_serde_uses_snake_case() {
        assert_eq!(
            serde_json::to_string(&LoopEdgeCondition::Pass).unwrap(),
            "\"pass\""
        );
        assert_eq!(
            serde_json::to_string(&LoopEdgeCondition::Always).unwrap(),
            "\"always\""
        );
    }

    // ── serde: LoopRunStatus roundtrip ──────────────────────────────────

    #[test]
    fn loop_run_status_serde_roundtrip() {
        let statuses = [
            LoopRunStatus::Running,
            LoopRunStatus::Pass,
            LoopRunStatus::Fail,
        ];
        for s in statuses {
            let json = serde_json::to_string(&s).unwrap();
            let deserialized: LoopRunStatus = serde_json::from_str(&json).unwrap();
            assert_eq!(deserialized, s, "serde roundtrip failed for {json}");
        }
    }

    #[test]
    fn loop_run_status_serde_uses_snake_case() {
        assert_eq!(
            serde_json::to_string(&LoopRunStatus::Running).unwrap(),
            "\"running\""
        );
        assert_eq!(
            serde_json::to_string(&LoopRunStatus::Pass).unwrap(),
            "\"pass\""
        );
    }

    // ── serde: deserialization from string variants ─────────────────────

    #[test]
    fn loop_status_deserialize_from_json_string() {
        let input = "\"completed\"";
        let s: LoopStatus = serde_json::from_str(input).unwrap();
        assert_eq!(s, LoopStatus::Completed);
    }

    #[test]
    fn loop_spec_status_deserialize_from_json_string() {
        let input = "\"skipped\"";
        let s: LoopSpecStatus = serde_json::from_str(input).unwrap();
        assert_eq!(s, LoopSpecStatus::Skipped);
    }

    #[test]
    fn loop_node_kind_deserialize_from_json_string() {
        let input = "\"join\"";
        let k: LoopNodeKind = serde_json::from_str(input).unwrap();
        assert_eq!(k, LoopNodeKind::Join);
    }

    #[test]
    fn loop_edge_condition_deserialize_from_json_string() {
        let input = "\"always\"";
        let c: LoopEdgeCondition = serde_json::from_str(input).unwrap();
        assert_eq!(c, LoopEdgeCondition::Always);
    }

    #[test]
    fn loop_run_status_deserialize_from_json_string() {
        let input = "\"fail\"";
        let s: LoopRunStatus = serde_json::from_str(input).unwrap();
        assert_eq!(s, LoopRunStatus::Fail);
    }

    // ── SpecAdminStatusOutcome: clone & Debug ───────────────────────────

    #[test]
    fn spec_admin_status_outcome_success_is_clone() {
        let a = SpecAdminStatusOutcome::Success;
        let b = a;
        assert!(matches!(b, SpecAdminStatusOutcome::Success));
        fn _assert_clone<T: Clone>() {}
        _assert_clone::<SpecAdminStatusOutcome>();
    }

    #[test]
    fn spec_admin_status_outcome_not_found_is_clone() {
        let a = SpecAdminStatusOutcome::NotFound;
        let b = a;
        assert!(matches!(b, SpecAdminStatusOutcome::NotFound));
        fn _assert_clone<T: Clone>() {}
        _assert_clone::<SpecAdminStatusOutcome>();
    }

    #[test]
    fn spec_admin_status_outcome_not_standalone_is_clone() {
        let a = SpecAdminStatusOutcome::NotStandalone("s1".to_string());
        let b = a.clone();
        match (&a, &b) {
            (SpecAdminStatusOutcome::NotStandalone(o), SpecAdminStatusOutcome::NotStandalone(c)) => {
                assert_eq!(o, c);
                assert_eq!(c, "s1");
            }
            _ => panic!("expected clone of NotStandalone"),
        }
    }

    #[test]
    fn spec_admin_status_outcome_active_run_is_clone() {
        let a = SpecAdminStatusOutcome::ActiveRun {
            loop_id: "l1".to_string(),
            run_id: "r1".to_string(),
        };
        let b = a.clone();
        match (&a, &b) {
            (
                SpecAdminStatusOutcome::ActiveRun {
                    loop_id: al,
                    run_id: ar,
                },
                SpecAdminStatusOutcome::ActiveRun {
                    loop_id: bl,
                    run_id: br,
                },
            ) => {
                assert_eq!(al, bl);
                assert_eq!(ar, br);
                assert_eq!(bl, "l1");
                assert_eq!(br, "r1");
            }
            _ => panic!("expected clone of ActiveRun"),
        }
    }

    // ── LoopResetOutcome: clone & Debug ─────────────────────────────────

    #[test]
    fn loop_reset_outcome_is_clone() {
        let outcomes = [
            LoopResetOutcome::NotFound,
            LoopResetOutcome::Running,
            LoopResetOutcome::InvalidSpec("x".to_string()),
            LoopResetOutcome::Reset { spec_count: 3 },
        ];
        for o in outcomes {
            let cloned = o.clone();
            assert_eq!(format!("{o:?}"), format!("{cloned:?}"));
        }
    }

    // ── validate_spec_description_template: mixed format markers ────────

    #[test]
    fn spec_template_mixed_h2_and_colon_markers() {
        let desc = r#"
## Functional Requirements:
- Build

Non-Functional Requirements:
- Fast

## Objective:
Done

- Constraints:
  Safe

* Guidelines:
  Simple

- In Scope:
  API

## Out of Scope:
UI
"#;
        assert!(validate_spec_description_template(desc).is_ok());
    }

    #[test]
    fn spec_template_each_section_can_be_detected_independently() {
        let sections = [
            "Functional Requirements",
            "Non-Functional Requirements",
            "Objective",
            "Constraints",
            "Guidelines",
            "In Scope",
            "Out of Scope",
        ];
        for (i, section) in sections.iter().enumerate() {
            let desc = full_desc_with(i, section);
            assert!(
                validate_spec_description_template(&desc).is_ok(),
                "section {section} should have been detected but was reported missing"
            );
        }
    }

    #[test]
    fn spec_template_all_aliases_for_functional_requirements() {
        let aliases = [
            "Functional Requirements",
            "functional requirements",
            "Requerimientos funcionales",
            "Requisitos funcionales",
        ];
        for alias in aliases {
            let desc = format!(
                "{alias}:\n- x\nNon-Functional Requirements:\n- y\nObjective:\n- z\nConstraints:\n- w\nGuidelines:\n- v\nIn Scope:\n- u\nOut of Scope:\n- t\n"
            );
            assert!(
                validate_spec_description_template(&desc).is_ok(),
                "alias {alias:?} should match"
            );
        }
    }

    #[test]
    fn spec_template_all_aliases_for_constraints() {
        let aliases = [
            "Constraints",
            "What to respect",
            "Restrictions",
            "Restricciones",
            "Que respetar",
            "Qué respetar",
        ];
        for alias in aliases {
            let desc = format!(
                "Functional Requirements:\n- x\nNon-Functional Requirements:\n- y\nObjective:\n- z\n{alias}:\n- w\nGuidelines:\n- v\nIn Scope:\n- u\nOut of Scope:\n- t\n"
            );
            assert!(
                validate_spec_description_template(&desc).is_ok(),
                "alias {alias:?} should match"
            );
        }
    }

    #[test]
    fn spec_template_all_aliases_for_guidelines() {
        let aliases = [
            "Guidelines",
            "Lineamientos",
            "Guidance",
            "Lineas guia",
            "Líneas guía",
        ];
        for alias in aliases {
            let desc = format!(
                "Functional Requirements:\n- x\nNon-Functional Requirements:\n- y\nObjective:\n- z\nConstraints:\n- w\n{alias}:\n- v\nIn Scope:\n- u\nOut of Scope:\n- t\n"
            );
            assert!(
                validate_spec_description_template(&desc).is_ok(),
                "alias {alias:?} should match"
            );
        }
    }

    #[test]
    fn spec_template_all_aliases_for_in_scope() {
        let aliases = ["In Scope", "Scope In", "Que si", "Qué sí", "Incluye"];
        for alias in aliases {
            let desc = format!(
                "Functional Requirements:\n- x\nNon-Functional Requirements:\n- y\nObjective:\n- z\nConstraints:\n- w\nGuidelines:\n- v\n{alias}:\n- u\nOut of Scope:\n- t\n"
            );
            assert!(
                validate_spec_description_template(&desc).is_ok(),
                "alias {alias:?} should match"
            );
        }
    }

    #[test]
    fn spec_template_all_aliases_for_out_of_scope() {
        let aliases = [
            "Out of Scope",
            "Scope Out",
            "Que no",
            "Qué no",
            "Excluye",
        ];
        for alias in aliases {
            let desc = format!(
                "Functional Requirements:\n- x\nNon-Functional Requirements:\n- y\nObjective:\n- z\nConstraints:\n- w\nGuidelines:\n- v\nIn Scope:\n- u\n{alias}:\n- t\n"
            );
            assert!(
                validate_spec_description_template(&desc).is_ok(),
                "alias {alias:?} should match"
            );
        }
    }

    #[test]
    fn spec_template_all_aliases_for_non_functional_requirements() {
        let aliases = [
            "Non-Functional Requirements",
            "Non functional requirements",
            "Requerimientos no funcionales",
            "Requisitos no funcionales",
        ];
        for alias in aliases {
            let desc = format!(
                "Functional Requirements:\n- x\n{alias}:\n- y\nObjective:\n- z\nConstraints:\n- w\nGuidelines:\n- v\nIn Scope:\n- u\nOut of Scope:\n- t\n"
            );
            assert!(
                validate_spec_description_template(&desc).is_ok(),
                "alias {alias:?} should match"
            );
        }
    }

    #[test]
    fn spec_template_all_aliases_for_objective() {
        let aliases = [
            "Objective",
            "Expected Outcome",
            "Objetivo",
            "Resultado esperado",
        ];
        for alias in aliases {
            let desc = format!(
                "Functional Requirements:\n- x\nNon-Functional Requirements:\n- y\n{alias}:\n- z\nConstraints:\n- w\nGuidelines:\n- v\nIn Scope:\n- u\nOut of Scope:\n- t\n"
            );
            assert!(
                validate_spec_description_template(&desc).is_ok(),
                "alias {alias:?} should match"
            );
        }
    }
}
