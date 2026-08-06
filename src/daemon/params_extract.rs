//! Shared parameter extractor for every canopy MCP tool.
//!
//! `rmcp`'s own `Parameters<T>` extractor turns a deserialization failure
//! into `format!("failed to deserialize parameters: {serde_error}")`, which
//! only ever names a Rust type (e.g. "expected struct SeedDirectives") and
//! never the JSON field that was actually wrong. Every caller of this MCP
//! surface is an LLM re-guessing the argument shape from that message alone,
//! so a message that doesn't name the field and show a fix is, functionally,
//! a broken tool.
//!
//! This module defines a drop-in replacement, also named `Parameters<T>` (the
//! `#[tool]` macro locates the parameter type by literal ident match, so the
//! name must match), that walks a failed deserialization with
//! `serde_path_to_error` and turns it into a message naming the failing
//! field path, what was received, and a minimal valid example derived from
//! `T`'s own `JsonSchema` derive. The success path is untouched: enrichment
//! only runs after `serde_path_to_error` already reports an error.

use std::borrow::Cow;

use rmcp::handler::server::common::FromContextPart;
use rmcp::handler::server::tool::ToolCallContext;
use rmcp::schemars::generate::SchemaSettings;
use rmcp::schemars::{self, JsonSchema};
use rmcp::ErrorData;
use serde::de::DeserializeOwned;
use serde_json::{Map, Value};

/// Maximum depth walked when synthesizing an example instance from a schema.
/// Bounds message size against self-referential or very deep param types.
const MAX_EXAMPLE_DEPTH: usize = 6;
/// Maximum length (in characters) of the rendered "received" value before
/// truncation, so a large payload can't blow up the error message.
const MAX_RECEIVED_CHARS: usize = 200;

/// Parameter extractor for tools and prompts.
///
/// Mirrors `rmcp::handler::server::wrapper::Parameters<T>` in shape and
/// name, but deserializes through [`serde_path_to_error`] so a failure can
/// be enriched with the field path, the received value, and a minimal
/// example before it reaches the client.
#[derive(Debug, Clone)]
pub struct Parameters<P>(pub P);

impl<P: JsonSchema> JsonSchema for Parameters<P> {
    fn schema_name() -> Cow<'static, str> {
        P::schema_name()
    }

    fn json_schema(generator: &mut schemars::SchemaGenerator) -> schemars::Schema {
        P::json_schema(generator)
    }
}

impl<S, P> FromContextPart<ToolCallContext<'_, S>> for Parameters<P>
where
    P: DeserializeOwned + JsonSchema,
{
    fn from_context_part(context: &mut ToolCallContext<S>) -> Result<Self, ErrorData> {
        let arguments = context.arguments.take().unwrap_or_default();
        deserialize_params::<P>(&Value::Object(arguments)).map(Parameters)
    }
}

/// Deserialize `value` into `P`, enriching any failure with the field path,
/// the received value, and a minimal example. Split out from
/// [`FromContextPart::from_context_part`] so it can be exercised directly in
/// tests without constructing a `ToolCallContext`.
fn deserialize_params<P>(value: &Value) -> Result<P, ErrorData>
where
    P: DeserializeOwned + JsonSchema,
{
    match serde_path_to_error::deserialize::<_, P>(value) {
        Ok(params) => Ok(params),
        Err(err) => Err(enrich_error::<P>(&err, value)),
    }
}

fn enrich_error<P: JsonSchema>(
    err: &serde_path_to_error::Error<serde_json::Error>,
    root: &Value,
) -> ErrorData {
    let path = err.path();
    let path_display = path.to_string();
    let field = if path_display == "." {
        "(top-level parameters)".to_string()
    } else {
        path_display
    };

    let received = describe_received(value_at_path(root, path));

    let schema_root = root_schema_value::<P>();
    let defs = schema_defs(&schema_root);
    let target_schema = schema_at_path(&schema_root, &defs, path);
    let example = minimal_example(&target_schema, &defs, MAX_EXAMPLE_DEPTH);
    let example_json = serde_json::to_string(&example).unwrap_or_else(|_| "null".to_string());

    let message = format!(
        "invalid value for parameter `{field}`: expected shape (example) {example_json}, \
         but received {received}. (serde: {inner})",
        field = field,
        example_json = example_json,
        received = received,
        inner = err.inner(),
    );

    ErrorData::invalid_params(message, None)
}

/// Best-effort walk of the submitted JSON value along `path`. Stops and
/// returns the deepest value it could reach if the path and the actual
/// submitted shape diverge (e.g. a wrong type earlier along the path than
/// where the error itself was raised).
fn value_at_path<'a>(root: &'a Value, path: &serde_path_to_error::Path) -> &'a Value {
    let mut current = root;
    for segment in path {
        let next = match segment {
            serde_path_to_error::Segment::Map { key } => current.get(key),
            serde_path_to_error::Segment::Seq { index } => current.get(*index),
            serde_path_to_error::Segment::Enum { .. } | serde_path_to_error::Segment::Unknown => {
                None
            }
        };
        match next {
            Some(value) => current = value,
            None => break,
        }
    }
    current
}

fn describe_received(value: &Value) -> String {
    let kind = json_type_name(value);
    let mut rendered = serde_json::to_string(value).unwrap_or_else(|_| "<unserializable>".into());
    if rendered.chars().count() > MAX_RECEIVED_CHARS {
        rendered = rendered
            .chars()
            .take(MAX_RECEIVED_CHARS)
            .collect::<String>()
            + "…(truncated)";
    }
    format!("{kind} {rendered}")
}

fn json_type_name(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

/// Root JSON Schema (draft 2020-12, with `$defs` for nested types) for `P`,
/// generated straight from its `JsonSchema` derive so the example generator
/// can never drift from the real accepted shape.
fn root_schema_value<P: JsonSchema>() -> Value {
    let generator = SchemaSettings::draft2020_12().into_generator();
    generator.into_root_schema_for::<P>().to_value()
}

fn schema_defs(root: &Value) -> Map<String, Value> {
    root.get("$defs")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default()
}

/// Resolves a `$ref` chain (if any) against `defs`, returning the concrete
/// schema. Bounded so a (currently impossible, but future) cyclic ref can't
/// loop forever.
fn resolve_schema(schema: &Value, defs: &Map<String, Value>) -> Value {
    let mut current = schema.clone();
    for _ in 0..16 {
        let Some(reference) = current.get("$ref").and_then(Value::as_str) else {
            break;
        };
        let key = reference.rsplit('/').next().unwrap_or_default();
        match defs.get(key) {
            Some(next) => current = next.clone(),
            None => break,
        }
    }
    current
}

/// Walks `root`'s schema along the same field path the deserialization error
/// reported, resolving `$ref`s as it goes. Stops and returns the deepest
/// schema it could resolve if the path runs out of matching schema
/// structure (e.g. the mismatch itself is what broke the path).
fn schema_at_path(
    root: &Value,
    defs: &Map<String, Value>,
    path: &serde_path_to_error::Path,
) -> Value {
    let mut current = resolve_schema(root, defs);
    for segment in path {
        let next = match segment {
            serde_path_to_error::Segment::Map { key } => {
                current.get("properties").and_then(|p| p.get(key)).cloned()
            }
            serde_path_to_error::Segment::Seq { .. } => current.get("items").cloned(),
            serde_path_to_error::Segment::Enum { .. } | serde_path_to_error::Segment::Unknown => {
                None
            }
        };
        match next {
            Some(next_schema) => current = resolve_schema(&next_schema, defs),
            None => break,
        }
    }
    current
}

/// Synthesizes a minimal valid JSON instance for `schema`, recursing through
/// objects/arrays/`$ref`s/`anyOf`-nullables up to `depth` levels.
fn minimal_example(schema: &Value, defs: &Map<String, Value>, depth: usize) -> Value {
    if depth == 0 {
        return Value::Null;
    }
    let schema = resolve_schema(schema, defs);

    if let Some(enum_values) = schema.get("enum").and_then(Value::as_array) {
        return enum_values.first().cloned().unwrap_or(Value::Null);
    }
    if let Some(const_value) = schema.get("const") {
        return const_value.clone();
    }
    for combinator in ["anyOf", "oneOf"] {
        if let Some(variants) = schema.get(combinator).and_then(Value::as_array) {
            let chosen = variants
                .iter()
                .find(|variant| variant.get("type").and_then(Value::as_str) != Some("null"))
                .or_else(|| variants.first());
            if let Some(chosen) = chosen {
                return minimal_example(chosen, defs, depth - 1);
            }
        }
    }

    match schema_primary_type(&schema).as_deref() {
        Some("object") => {
            let mut obj = Map::new();
            if let Some(props) = schema.get("properties").and_then(Value::as_object) {
                for (key, prop_schema) in props {
                    obj.insert(key.clone(), minimal_example(prop_schema, defs, depth - 1));
                }
            }
            Value::Object(obj)
        }
        Some("array") => match schema.get("items") {
            Some(items) => Value::Array(vec![minimal_example(items, defs, depth - 1)]),
            None => Value::Array(Vec::new()),
        },
        Some("string") => Value::String("...".to_string()),
        Some("integer") | Some("number") => Value::from(0),
        Some("boolean") => Value::Bool(true),
        _ => Value::Null,
    }
}

fn schema_primary_type(schema: &Value) -> Option<String> {
    match schema.get("type") {
        Some(Value::String(single)) => Some(single.clone()),
        Some(Value::Array(candidates)) => candidates
            .iter()
            .filter_map(Value::as_str)
            .find(|candidate| *candidate != "null")
            .or_else(|| candidates.first().and_then(Value::as_str))
            .map(str::to_string),
        _ => {
            if schema.get("properties").is_some() {
                Some("object".to_string())
            } else if schema.get("items").is_some() {
                Some("array".to_string())
            } else {
                None
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::daemon::params::{
        CreateSeedParams, IntelligenceNodeParams, IntelligenceUpsertParams,
    };
    use rmcp::model::ErrorCode;

    fn message_of(result: Result<impl std::fmt::Debug, ErrorData>) -> String {
        match result {
            Ok(value) => panic!("expected a deserialization error, got Ok({value:?})"),
            Err(err) => {
                assert_eq!(
                    err.code,
                    ErrorCode::INVALID_PARAMS,
                    "wrong JSON-RPC error code"
                );
                err.message.to_string()
            }
        }
    }

    #[test]
    fn nested_wrong_type_names_field_path_and_example() {
        // `directives` should be `{"general": [...]}`, not a bare array.
        let value = serde_json::json!({
            "name": "seed-1",
            "directives": ["Be concise", "Prefer tests"],
        });

        let message = message_of(deserialize_params::<CreateSeedParams>(&value));

        assert!(
            message.contains("directives"),
            "message should name the failing field `directives`: {message}"
        );
        assert!(
            message.contains(r#"{"general":["..."]}"#),
            "message should show the minimal example shape: {message}"
        );
        assert!(
            message.contains("string"),
            "message should say what JSON type was actually received: {message}"
        );
        assert!(
            message.contains("Be concise"),
            "message should echo (a truncated form of) the received value: {message}"
        );

        // Rust type names may appear, but only as a trailing, clearly
        // separated detail — not as the sentence's main vocabulary.
        let primary_sentence = message.split("(serde:").next().unwrap();
        assert!(
            !primary_sentence.contains("SeedDirectives"),
            "Rust type name leaked into the primary message: {message}"
        );
    }

    #[test]
    fn array_of_structs_wrong_element_type_names_indexed_path() {
        // `node_data.relations[0].relation` must be a string, not a number.
        let value = serde_json::json!({
            "node_data": {
                "kind": "fact",
                "title": "t",
                "body": "b",
                "relations": [
                    { "to_node_id": "n1", "relation": 42 }
                ],
            }
        });

        let message = message_of(deserialize_params::<IntelligenceUpsertParams>(&value));

        assert!(
            message.contains("node_data.relations[0].relation"),
            "message should name the exact indexed nested path: {message}"
        );
        assert!(
            message.contains("number"),
            "message should say a number was received where a string was expected: {message}"
        );
    }

    #[test]
    fn wrong_scalar_type_on_flat_field() {
        let value = serde_json::json!({
            "node_data": {
                "kind": "fact",
                "title": "t",
                "body": "b",
                "id": 12345,
            }
        });

        let message = message_of(deserialize_params::<IntelligenceUpsertParams>(&value));

        assert!(
            message.contains("node_data.id"),
            "message should name the scalar field path: {message}"
        );
        assert!(
            message.contains("number"),
            "message should describe the received scalar's JSON type: {message}"
        );
    }

    #[test]
    fn well_formed_flat_params_still_deserialize() {
        let value = serde_json::json!({ "seed_id": "abc" });
        let params: crate::daemon::params::RemoveSeedParams =
            deserialize_params(&value).expect("flat scalar params should still deserialize");
        assert_eq!(params.seed_id, "abc");
    }

    #[test]
    fn well_formed_nested_params_still_deserialize() {
        let value = serde_json::json!({
            "node_data": {
                "kind": "fact",
                "title": "t",
                "body": "b",
                "metadata": { "k": "v" },
                "relations": [
                    { "to_node_id": "n1", "relation": "depends_on", "weight": 0.5 }
                ],
            }
        });
        let params: IntelligenceUpsertParams =
            deserialize_params(&value).expect("well-formed nested params should still deserialize");
        assert_eq!(params.node_data.kind, "fact");
        let relations = params.node_data.relations.expect("relations present");
        assert_eq!(relations.len(), 1);
        assert_eq!(relations[0].relation, "depends_on");
    }

    #[test]
    fn minimal_example_generator_matches_node_params_shape() {
        let schema_root = root_schema_value::<IntelligenceNodeParams>();
        let defs = schema_defs(&schema_root);
        let example = minimal_example(&schema_root, &defs, MAX_EXAMPLE_DEPTH);
        let obj = example.as_object().expect("object example");
        assert!(obj.contains_key("kind"));
        assert!(obj.contains_key("title"));
        assert!(obj.contains_key("relations"));
    }

    #[test]
    fn node_data_advertises_inline_object_schema_not_bare_ref() {
        // Agents that build tool arguments from a shallow read of the
        // property schema (never resolving `$ref`) need `node_data` to
        // self-declare its type. A bare `{"$ref": ...}` with no sibling
        // `type` is exactly what caused this parameter to be sent as a
        // JSON-encoded string instead of an object.
        let root = root_schema_value::<IntelligenceUpsertParams>();
        let node_data = root
            .get("properties")
            .and_then(|p| p.get("node_data"))
            .expect("node_data property present");

        assert!(
            node_data.get("$ref").is_none(),
            "node_data should be inlined, not a bare $ref: {node_data}"
        );
        assert_eq!(
            node_data.get("type").and_then(Value::as_str),
            Some("object"),
            "node_data should declare type object: {node_data}"
        );
        assert!(
            node_data
                .get("properties")
                .and_then(Value::as_object)
                .is_some(),
            "node_data's properties should be readable without resolving a $ref: {node_data}"
        );
    }

    #[test]
    fn metadata_fields_declare_a_type() {
        let intel_root = root_schema_value::<IntelligenceUpsertParams>();
        let node_data_metadata = intel_root
            .get("properties")
            .and_then(|p| p.get("node_data"))
            .and_then(|n| n.get("properties"))
            .and_then(|p| p.get("metadata"))
            .expect("node_data.metadata property present");
        assert!(
            node_data_metadata.get("type").is_some(),
            "IntelligenceNodeParams.metadata should declare a type: {node_data_metadata}"
        );

        let broadcast_root = root_schema_value::<crate::daemon::params::SyncBroadcastParams>();
        let broadcast_metadata = broadcast_root
            .get("properties")
            .and_then(|p| p.get("metadata"))
            .expect("metadata property present");
        assert!(
            broadcast_metadata.get("type").is_some(),
            "SyncBroadcastParams.metadata should declare a type: {broadcast_metadata}"
        );
    }

    #[test]
    fn stringified_node_data_error_names_parameter_and_expected_shape() {
        let value = serde_json::json!({
            "node_data": "{\"kind\":\"fact\",\"title\":\"t\",\"body\":\"b\"}",
        });

        let message = message_of(deserialize_params::<IntelligenceUpsertParams>(&value));

        assert!(
            message.contains("node_data"),
            "message should name the parameter `node_data`: {message}"
        );
        assert!(
            message.contains("expected shape"),
            "message should describe the expected object shape: {message}"
        );
        assert!(
            message.contains("string"),
            "message should say a string was received where an object was expected: {message}"
        );
    }
}
