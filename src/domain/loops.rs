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
}

impl LoopNodeKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Agent => "agent",
            Self::Check => "check",
            Self::Gate => "gate",
        }
    }

    pub fn from_str(value: &str) -> Option<Self> {
        match value {
            "agent" => Some(Self::Agent),
            "check" => Some(Self::Check),
            "gate" => Some(Self::Gate),
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
    /// Optional reusable spec template. When set, building a new spec for
    /// this loop can start from the pool's node/edge layout instead of
    /// repeating the same handful of nodes and edges by hand each time.
    #[serde(default)]
    pub spec_pool: Option<SpecPool>,
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

/// A node within a [`SpecPool`] template.
///
/// Unlike [`LoopNode`], it has no `id`/`spec_id` — it isn't tied to any
/// concrete spec yet. `name` is the join key `SpecPoolEdge` uses instead of a
/// generated id.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SpecPoolNode {
    pub name: String,
    pub kind: LoopNodeKind,
    #[serde(default = "default_json_object")]
    pub config: Value,
    pub position: i64,
}

/// An edge within a [`SpecPool`] template, joining two [`SpecPoolNode`]s by
/// name rather than by generated node id.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SpecPoolEdge {
    pub from_node: String,
    pub to_node: String,
    pub condition: LoopEdgeCondition,
}

fn default_json_object() -> Value {
    Value::Object(serde_json::Map::new())
}

/// A reusable template of nodes and edges for building loop specs.
///
/// Loops that repeat the same handful of nodes/edges across specs (e.g. an
/// "agent -> check -> gate" shape) can define that shape once as a
/// `SpecPool` and reference it from [`Loop::spec_pool`], instead of
/// re-declaring every node and edge each time a spec is built.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SpecPool {
    pub id: String,
    pub name: String,
    pub description: Option<String>,
    #[serde(default)]
    pub nodes: Vec<SpecPoolNode>,
    #[serde(default)]
    pub edges: Vec<SpecPoolEdge>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LoopSpec {
    pub id: String,
    pub loop_id: String,
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
}

#[cfg(test)]
mod tests {
    use super::{
        validate_spec_description_template, LoopEdgeCondition, LoopNodeKind, LoopRunStatus,
        LoopSpecStatus, LoopStatus, SpecPool, SpecPoolEdge, SpecPoolNode,
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
    }

    #[test]
    fn loop_node_kind_from_str() {
        assert_eq!(LoopNodeKind::from_str("agent"), Some(LoopNodeKind::Agent));
        assert_eq!(LoopNodeKind::from_str("check"), Some(LoopNodeKind::Check));
        assert_eq!(LoopNodeKind::from_str("gate"), Some(LoopNodeKind::Gate));
        assert!(LoopNodeKind::from_str("invalid").is_none());
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
            spec_pool: None,
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

    fn sample_spec_pool() -> SpecPool {
        SpecPool {
            id: "pool-1".to_string(),
            name: "Agent review gate".to_string(),
            description: Some("agent -> check -> gate".to_string()),
            nodes: vec![
                SpecPoolNode {
                    name: "implement".to_string(),
                    kind: LoopNodeKind::Agent,
                    config: serde_json::json!({"cli": "claude"}),
                    position: 0,
                },
                SpecPoolNode {
                    name: "run-tests".to_string(),
                    kind: LoopNodeKind::Check,
                    config: serde_json::json!({"command": "cargo test"}),
                    position: 1,
                },
                SpecPoolNode {
                    name: "reviewer-gate".to_string(),
                    kind: LoopNodeKind::Gate,
                    config: serde_json::json!({}),
                    position: 2,
                },
            ],
            edges: vec![
                SpecPoolEdge {
                    from_node: "implement".to_string(),
                    to_node: "run-tests".to_string(),
                    condition: LoopEdgeCondition::Always,
                },
                SpecPoolEdge {
                    from_node: "run-tests".to_string(),
                    to_node: "reviewer-gate".to_string(),
                    condition: LoopEdgeCondition::Pass,
                },
            ],
        }
    }

    #[test]
    fn spec_pool_can_be_constructed_with_nodes_and_edges() {
        let pool = sample_spec_pool();
        assert_eq!(pool.nodes.len(), 3);
        assert_eq!(pool.edges.len(), 2);
        assert_eq!(pool.nodes[0].kind, LoopNodeKind::Agent);
        assert_eq!(pool.edges[1].condition, LoopEdgeCondition::Pass);
    }

    #[test]
    fn spec_pool_round_trips_through_json() {
        let pool = sample_spec_pool();
        let json = serde_json::to_string(&pool).expect("serialize spec pool");
        let decoded: SpecPool = serde_json::from_str(&json).expect("deserialize spec pool");
        assert_eq!(decoded, pool);
    }
}
