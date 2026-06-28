use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;

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
    pub created_at: DateTime<Utc>,
    pub started_at: Option<DateTime<Utc>>,
    pub completed_at: Option<DateTime<Utc>>,
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
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LoopNode {
    pub id: String,
    pub spec_id: String,
    pub name: String,
    pub kind: LoopNodeKind,
    pub config: Value,
    pub position: i64,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LoopEdge {
    pub id: String,
    pub spec_id: String,
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
    pub specs: Vec<LoopSpecDetails>,
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
}
