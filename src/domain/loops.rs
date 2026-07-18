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
        validate_spec_description_template, LoopEdgeCondition, LoopNodeKind, LoopRunStatus,
        LoopSpecStatus, LoopStatus,
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
}
