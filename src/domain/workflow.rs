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
            "Workflow spec description must include sections for: {}.",
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
        "Workflow spec description is missing required sections: {}. Expected sections: {}.",
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
pub enum WorkflowStatus {
    Draft,
    Running,
    Paused,
    Completed,
    Failed,
}

impl WorkflowStatus {
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
pub enum WorkflowSpecStatus {
    Pending,
    Running,
    Completed,
    Failed,
    Skipped,
}

impl WorkflowSpecStatus {
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
pub enum WorkflowNodeKind {
    Agent,
    Check,
    Gate,
}

impl WorkflowNodeKind {
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
pub enum WorkflowEdgeCondition {
    Pass,
    Fail,
    Always,
}

impl WorkflowEdgeCondition {
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
pub enum WorkflowRunStatus {
    Running,
    Pass,
    Fail,
}

impl WorkflowRunStatus {
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
pub struct Workflow {
    pub id: String,
    pub name: String,
    pub description: Option<String>,
    pub workdir: String,
    pub status: WorkflowStatus,
    pub created_at: DateTime<Utc>,
    pub started_at: Option<DateTime<Utc>>,
    pub completed_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkflowSpec {
    pub id: String,
    pub workflow_id: String,
    pub name: String,
    pub description: Option<String>,
    pub position: i64,
    pub parallelizable: bool,
    pub status: WorkflowSpecStatus,
    pub started_at: Option<DateTime<Utc>>,
    pub completed_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkflowNode {
    pub id: String,
    pub spec_id: String,
    pub name: String,
    pub kind: WorkflowNodeKind,
    pub config: Value,
    pub position: i64,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkflowEdge {
    pub id: String,
    pub spec_id: String,
    pub from_node: String,
    pub to_node: String,
    pub condition: WorkflowEdgeCondition,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkflowNodeRun {
    pub id: String,
    pub workflow_id: String,
    pub spec_id: String,
    pub node_id: String,
    pub status: WorkflowRunStatus,
    pub input: Option<Value>,
    pub output: Option<Value>,
    pub started_at: DateTime<Utc>,
    pub completed_at: Option<DateTime<Utc>>,
    pub iteration: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkflowSpecDetails {
    pub spec: WorkflowSpec,
    pub nodes: Vec<WorkflowNode>,
    pub edges: Vec<WorkflowEdge>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkflowDetails {
    pub workflow: Workflow,
    pub specs: Vec<WorkflowSpecDetails>,
}

#[cfg(test)]
mod tests {
    use super::{
        validate_spec_description_template, WorkflowEdgeCondition, WorkflowNodeKind,
        WorkflowRunStatus, WorkflowSpecStatus, WorkflowStatus,
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
Backend workflow tools

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
    fn workflow_status_as_str_roundtrip() {
        assert_eq!(WorkflowStatus::Draft.as_str(), "draft");
        assert_eq!(WorkflowStatus::Running.as_str(), "running");
        assert_eq!(WorkflowStatus::Paused.as_str(), "paused");
        assert_eq!(WorkflowStatus::Completed.as_str(), "completed");
        assert_eq!(WorkflowStatus::Failed.as_str(), "failed");
    }

    #[test]
    fn workflow_status_from_str() {
        assert_eq!(WorkflowStatus::from_str("running"), WorkflowStatus::Running);
        assert_eq!(WorkflowStatus::from_str("paused"), WorkflowStatus::Paused);
        assert_eq!(
            WorkflowStatus::from_str("completed"),
            WorkflowStatus::Completed
        );
        assert_eq!(WorkflowStatus::from_str("failed"), WorkflowStatus::Failed);
        assert_eq!(WorkflowStatus::from_str("invalid"), WorkflowStatus::Draft);
    }

    #[test]
    fn workflow_spec_status_as_str() {
        assert_eq!(WorkflowSpecStatus::Pending.as_str(), "pending");
        assert_eq!(WorkflowSpecStatus::Running.as_str(), "running");
        assert_eq!(WorkflowSpecStatus::Completed.as_str(), "completed");
        assert_eq!(WorkflowSpecStatus::Failed.as_str(), "failed");
        assert_eq!(WorkflowSpecStatus::Skipped.as_str(), "skipped");
    }

    #[test]
    fn workflow_spec_status_from_str() {
        assert_eq!(
            WorkflowSpecStatus::from_str("running"),
            WorkflowSpecStatus::Running
        );
        assert_eq!(
            WorkflowSpecStatus::from_str("completed"),
            WorkflowSpecStatus::Completed
        );
        assert_eq!(
            WorkflowSpecStatus::from_str("failed"),
            WorkflowSpecStatus::Failed
        );
        assert_eq!(
            WorkflowSpecStatus::from_str("skipped"),
            WorkflowSpecStatus::Skipped
        );
        assert_eq!(
            WorkflowSpecStatus::from_str("invalid"),
            WorkflowSpecStatus::Pending
        );
    }

    #[test]
    fn workflow_node_kind_as_str() {
        assert_eq!(WorkflowNodeKind::Agent.as_str(), "agent");
        assert_eq!(WorkflowNodeKind::Check.as_str(), "check");
        assert_eq!(WorkflowNodeKind::Gate.as_str(), "gate");
    }

    #[test]
    fn workflow_node_kind_from_str() {
        assert_eq!(
            WorkflowNodeKind::from_str("agent"),
            Some(WorkflowNodeKind::Agent)
        );
        assert_eq!(
            WorkflowNodeKind::from_str("check"),
            Some(WorkflowNodeKind::Check)
        );
        assert_eq!(
            WorkflowNodeKind::from_str("gate"),
            Some(WorkflowNodeKind::Gate)
        );
        assert!(WorkflowNodeKind::from_str("invalid").is_none());
    }

    #[test]
    fn workflow_edge_condition_as_str() {
        assert_eq!(WorkflowEdgeCondition::Pass.as_str(), "pass");
        assert_eq!(WorkflowEdgeCondition::Fail.as_str(), "fail");
        assert_eq!(WorkflowEdgeCondition::Always.as_str(), "always");
    }

    #[test]
    fn workflow_edge_condition_from_str() {
        assert_eq!(
            WorkflowEdgeCondition::from_str("pass"),
            Some(WorkflowEdgeCondition::Pass)
        );
        assert_eq!(
            WorkflowEdgeCondition::from_str("fail"),
            Some(WorkflowEdgeCondition::Fail)
        );
        assert_eq!(
            WorkflowEdgeCondition::from_str("always"),
            Some(WorkflowEdgeCondition::Always)
        );
        assert!(WorkflowEdgeCondition::from_str("invalid").is_none());
    }

    #[test]
    fn workflow_run_status_as_str() {
        assert_eq!(WorkflowRunStatus::Running.as_str(), "running");
        assert_eq!(WorkflowRunStatus::Pass.as_str(), "pass");
        assert_eq!(WorkflowRunStatus::Fail.as_str(), "fail");
    }

    #[test]
    fn workflow_run_status_from_str() {
        assert_eq!(WorkflowRunStatus::from_str("pass"), WorkflowRunStatus::Pass);
        assert_eq!(WorkflowRunStatus::from_str("fail"), WorkflowRunStatus::Fail);
        assert_eq!(
            WorkflowRunStatus::from_str("invalid"),
            WorkflowRunStatus::Running
        );
    }
}
