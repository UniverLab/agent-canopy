//! Node blueprints — predesigned, reusable node templates that let loops be
//! assembled by connecting known modules instead of pasting full configs.
//!
//! A blueprint is just a name plus a node `kind` and a config template; the
//! engine never special-cases any particular blueprint name, which is what
//! lets new variants (a TDD implementer, per-language gates, ...) be added as
//! plain data instead of engine changes.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::domain::loops::LoopNodeKind;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Blueprint {
    pub id: String,
    pub name: String,
    pub kind: LoopNodeKind,
    /// Config template. `loop_add_node` uses this as-is, or shallow-merges
    /// `config_overrides` on top of it (override keys win).
    pub config: Value,
    /// Builtins are seeded automatically and can't be deleted — see
    /// [`builtin_blueprint_specs`].
    pub builtin: bool,
    pub created_at: DateTime<Utc>,
}

/// The proven 5-node pattern, seeded as builtins if missing at daemon
/// startup: an implementer, a gate check, a reviewer/committer, a commit
/// check, and a resilience/unblock step.
pub fn builtin_blueprint_specs() -> Vec<(&'static str, LoopNodeKind, Value)> {
    vec![
        (
            "implementer-claude",
            LoopNodeKind::Agent,
            serde_json::json!({
                "platform": "claude",
                "prompt": "Implement this spec:\n\n{{spec_content}}\n\nPrevious feedback (if any): {{previous_feedback}}"
            }),
        ),
        (
            "cargo-gates",
            LoopNodeKind::Check,
            serde_json::json!({
                "command": "cargo fmt --all && cargo clippy --all-targets -- -D warnings && cargo test"
            }),
        ),
        (
            "reviewer-committer-mimo",
            LoopNodeKind::Agent,
            serde_json::json!({
                "platform": "mimo",
                "prompt": "Review the changes made for this spec:\n\n{{spec_content}}\n\nIf they look correct, commit them. Otherwise, describe what's wrong so the implementer can address it."
            }),
        ),
        (
            "commit-check",
            LoopNodeKind::Check,
            serde_json::json!({
                "command": "test \"$(git rev-parse HEAD)\" != \"{{spec_start_head}}\""
            }),
        ),
        (
            "resilience-mimo",
            LoopNodeKind::Agent,
            serde_json::json!({
                "platform": "mimo",
                "prompt": "A node in this loop failed or reported a blocker: {{previous_feedback}}\n\nDiagnose the root cause and resolve it, or escalate with a clear explanation if it needs a human."
            }),
        ),
    ]
}

/// Shallow-merge `overrides` onto `template`: keys present in `overrides` win,
/// every other key from `template` is preserved. Non-object inputs are
/// treated as empty objects rather than rejected — callers validate node
/// config shape separately (see `validate_node_config`).
pub fn merge_blueprint_config(template: &Value, overrides: Option<&Value>) -> Value {
    let mut merged = template.as_object().cloned().unwrap_or_default();
    if let Some(overrides) = overrides.and_then(Value::as_object) {
        for (key, value) in overrides {
            merged.insert(key.clone(), value.clone());
        }
    }
    Value::Object(merged)
}

/// Refuse to delete a builtin blueprint, with an actionable explanation.
pub fn validate_blueprint_deletable(blueprint: &Blueprint) -> Result<(), String> {
    if blueprint.builtin {
        return Err(format!(
            "Blueprint '{}' is a builtin and cannot be deleted. Builtins are reseeded automatically at daemon startup if missing; create a custom blueprint under a different name instead.",
            blueprint.name
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn merge_blueprint_config_shallow_merges_override_keys_win() {
        let template = serde_json::json!({
            "platform": "claude",
            "prompt": "base prompt"
        });
        let overrides = serde_json::json!({
            "prompt": "custom prompt",
            "model": "opus"
        });

        let merged = merge_blueprint_config(&template, Some(&overrides));

        assert_eq!(merged["platform"], "claude");
        assert_eq!(merged["prompt"], "custom prompt");
        assert_eq!(merged["model"], "opus");
    }

    #[test]
    fn merge_blueprint_config_with_no_overrides_returns_template() {
        let template = serde_json::json!({ "command": "cargo test" });
        let merged = merge_blueprint_config(&template, None);
        assert_eq!(merged, template);
    }

    #[test]
    fn builtin_blueprint_specs_has_the_five_proven_nodes() {
        let names: Vec<&str> = builtin_blueprint_specs()
            .into_iter()
            .map(|(name, _, _)| name)
            .collect();
        assert_eq!(
            names,
            vec![
                "implementer-claude",
                "cargo-gates",
                "reviewer-committer-mimo",
                "commit-check",
                "resilience-mimo",
            ]
        );
    }

    #[test]
    fn validate_blueprint_deletable_refuses_builtins() {
        let bp = Blueprint {
            id: "1".to_string(),
            name: "implementer-claude".to_string(),
            kind: LoopNodeKind::Agent,
            config: serde_json::json!({}),
            builtin: true,
            created_at: Utc::now(),
        };
        let error = validate_blueprint_deletable(&bp).unwrap_err();
        assert!(error.contains("implementer-claude"));
        assert!(error.contains("cannot be deleted"));
    }

    #[test]
    fn validate_blueprint_deletable_allows_custom() {
        let bp = Blueprint {
            id: "1".to_string(),
            name: "my-custom".to_string(),
            kind: LoopNodeKind::Check,
            config: serde_json::json!({}),
            builtin: false,
            created_at: Utc::now(),
        };
        assert!(validate_blueprint_deletable(&bp).is_ok());
    }
}
