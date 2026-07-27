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
                "prompt_preset": "implementer"
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
                "prompt_preset": "reviewer"
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
                "prompt_preset": "resilience"
            }),
        ),
    ]
}

/// An ensemble blueprint (F1): unlike [`Blueprint`] (a single node's
/// kind+config), this is a whole ensemble's shared prompt + member list, the
/// pieces `loop_add_ensemble`'s `blueprint` param fills in when the caller
/// omits `prompt_template`/`members` explicitly.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EnsembleBlueprint {
    pub id: String,
    pub name: String,
    pub prompt_template: String,
    /// `(platform, model)` pairs, in the order members are created.
    pub members: Vec<(String, Option<String>)>,
    /// Suggested `min_pass`. `None` means "every member" — the same default
    /// `loop_add_ensemble` uses when the caller doesn't pass `min_pass`.
    pub min_pass: Option<i64>,
    pub builtin: bool,
    pub created_at: DateTime<Utc>,
}

/// `(name, prompt_template, members, min_pass)` — the raw tuple shape a
/// builtin ensemble blueprint spec is defined as, before being seeded into
/// the `ensemble_blueprints` table as an [`EnsembleBlueprint`] row.
pub type EnsembleBlueprintSpec = (
    &'static str,
    &'static str,
    Vec<(&'static str, Option<&'static str>)>,
    Option<i64>,
);

/// The builtin "ensemble-proposers" pattern (F1's canonical use (a)): 3 free
/// OpenRouter models draft a solution for the same spec in parallel, so an
/// implementer downstream spends its (potentially non-free) quota only once,
/// on a pre-digested task instead of a blank spec.
pub fn builtin_ensemble_blueprint_specs() -> Vec<EnsembleBlueprintSpec> {
    vec![(
        "ensemble-proposers",
        "Draft a complete solution for this spec. Be concrete and specific — an \
         implementer downstream may build directly from your draft without ever \
         seeing this spec itself, so leave nothing implicit:\n\n{{spec_content}}\n\n\
         Previous feedback (if any): {{previous_feedback}}",
        vec![
            ("openrouter", Some("deepseek/deepseek-chat-v3.1:free")),
            ("openrouter", Some("qwen/qwen3-coder:free")),
            ("openrouter", Some("meta-llama/llama-3.3-70b-instruct:free")),
        ],
        None,
    )]
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

    /// S2: a blueprint may carry pinned `skills` in its config template like
    /// any other key. With no override, the template's array passes through
    /// untouched.
    #[test]
    fn merge_blueprint_config_preserves_skills_array_from_template() {
        let template = serde_json::json!({
            "platform": "claude",
            "skills": ["coder", "rust-idiomatic-patterns"]
        });
        let merged = merge_blueprint_config(&template, None);
        assert_eq!(
            merged["skills"],
            serde_json::json!(["coder", "rust-idiomatic-patterns"])
        );
    }

    /// S2: an override's `skills` key replaces the template's wholesale —
    /// shallow merge, not an elementwise union — exactly like every other
    /// overridden key.
    #[test]
    fn merge_blueprint_config_override_replaces_template_skills_array() {
        let template = serde_json::json!({
            "platform": "claude",
            "skills": ["coder"]
        });
        let overrides = serde_json::json!({ "skills": ["reviewer", "coder"] });

        let merged = merge_blueprint_config(&template, Some(&overrides));

        assert_eq!(merged["skills"], serde_json::json!(["reviewer", "coder"]));
        assert_eq!(merged["platform"], "claude");
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
    fn builtin_agent_blueprints_carry_prompt_preset_not_inline_prompt() {
        let agent_specs: Vec<(&str, Value)> = builtin_blueprint_specs()
            .into_iter()
            .filter(|(_, kind, _)| *kind == LoopNodeKind::Agent)
            .map(|(name, _, config)| (name, config))
            .collect();

        assert_eq!(agent_specs.len(), 3);
        for (name, config) in &agent_specs {
            let preset = config["prompt_preset"]
                .as_str()
                .unwrap_or_else(|| panic!("blueprint '{name}' must carry a prompt_preset"));
            assert!(!preset.is_empty());
            assert!(
                config.get("prompt").is_none(),
                "blueprint '{name}' must not carry an inline 'prompt' key"
            );
        }
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
    fn builtin_ensemble_blueprint_specs_has_ensemble_proposers_with_valid_member_count() {
        let specs = builtin_ensemble_blueprint_specs();
        let (name, prompt_template, members, min_pass) = &specs[0];
        assert_eq!(*name, "ensemble-proposers");
        assert!(!prompt_template.is_empty());
        assert!(members.len() >= 2 && members.len() <= 8);
        assert!(min_pass.is_none());
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

    #[test]
    fn builtin_blueprint_specs_returns_non_empty_list() {
        let specs = builtin_blueprint_specs();
        assert!(!specs.is_empty());
        assert_eq!(specs.len(), 5);
    }

    #[test]
    fn builtin_blueprint_specs_each_has_name_kind_config() {
        for (name, _kind, config) in builtin_blueprint_specs() {
            assert!(!name.is_empty(), "blueprint name must not be empty");
            assert!(
                config.is_object(),
                "blueprint '{name}' config must be a JSON object"
            );
        }
    }

    #[test]
    fn builtin_blueprint_specs_kinds_are_agent_or_check() {
        for (name, kind, _) in builtin_blueprint_specs() {
            assert!(
                kind == LoopNodeKind::Agent || kind == LoopNodeKind::Check,
                "blueprint '{name}' kind must be Agent or Check, got {kind:?}"
            );
        }
    }

    #[test]
    fn merge_blueprint_config_with_empty_overrides_returns_template() {
        let template = serde_json::json!({ "a": 1, "b": 2 });
        let overrides = serde_json::json!({});
        let merged = merge_blueprint_config(&template, Some(&overrides));
        assert_eq!(merged, template);
    }

    #[test]
    fn merge_blueprint_config_with_non_object_overrides_returns_template() {
        let template = serde_json::json!({ "a": 1 });
        let overrides = serde_json::json!("just a string");
        let merged = merge_blueprint_config(&template, Some(&overrides));
        assert_eq!(merged, template);
    }

    #[test]
    fn merge_blueprint_config_non_object_template_treated_as_empty() {
        let template = serde_json::json!("not an object");
        let overrides = serde_json::json!({ "key": "val" });
        let merged = merge_blueprint_config(&template, Some(&overrides));
        assert_eq!(merged["key"], "val");
        assert!(merged.as_object().unwrap().len() == 1);
    }

    #[test]
    fn merge_blueprint_config_override_does_not_mutate_template() {
        let template = serde_json::json!({ "platform": "claude" });
        let overrides = serde_json::json!({ "platform": "mimo" });
        let _ = merge_blueprint_config(&template, Some(&overrides));
        assert_eq!(template["platform"], "claude");
    }

    #[test]
    fn blueprint_serde_roundtrip() {
        let bp = Blueprint {
            id: "test-id".to_string(),
            name: "test-bp".to_string(),
            kind: LoopNodeKind::Agent,
            config: serde_json::json!({ "platform": "claude", "prompt_preset": "implementer" }),
            builtin: true,
            created_at: Utc::now(),
        };

        let json = serde_json::to_string(&bp).unwrap();
        let deserialized: Blueprint = serde_json::from_str(&json).unwrap();

        assert_eq!(deserialized.id, bp.id);
        assert_eq!(deserialized.name, bp.name);
        assert_eq!(deserialized.kind, bp.kind);
        assert_eq!(deserialized.config, bp.config);
        assert_eq!(deserialized.builtin, bp.builtin);
    }

    #[test]
    fn ensemble_blueprint_serde_roundtrip() {
        let ebp = EnsembleBlueprint {
            id: "ens-1".to_string(),
            name: "test-ensemble".to_string(),
            prompt_template: "Do the thing\n\n{{spec_content}}".to_string(),
            members: vec![
                (
                    "openrouter".to_string(),
                    Some("deepseek/deepseek-chat-v3.1:free".to_string()),
                ),
                ("claude".to_string(), None),
            ],
            min_pass: Some(1),
            builtin: false,
            created_at: Utc::now(),
        };

        let json = serde_json::to_string(&ebp).unwrap();
        let deserialized: EnsembleBlueprint = serde_json::from_str(&json).unwrap();

        assert_eq!(deserialized.id, ebp.id);
        assert_eq!(deserialized.name, ebp.name);
        assert_eq!(deserialized.prompt_template, ebp.prompt_template);
        assert_eq!(deserialized.members, ebp.members);
        assert_eq!(deserialized.min_pass, ebp.min_pass);
        assert_eq!(deserialized.builtin, ebp.builtin);
    }

    #[test]
    fn builtin_blueprint_specs_check_blueprints_have_command() {
        let mut count = 0;
        for (name, kind, config) in builtin_blueprint_specs() {
            if kind == LoopNodeKind::Check {
                count += 1;
                assert!(
                    config.get("command").is_some(),
                    "check blueprint '{name}' must have a 'command' field"
                );
                assert!(
                    !config["command"].as_str().unwrap_or("").is_empty(),
                    "check blueprint '{name}' must have a non-empty 'command'"
                );
            }
        }
        assert_eq!(count, 2);
    }
}
