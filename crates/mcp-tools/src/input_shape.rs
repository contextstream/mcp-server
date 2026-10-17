//! Lenient tool-argument shapes, normalized once before any handler parses them.
//!
//! Handlers deserialize their input into typed structs, so a client that sends
//! `"force": "true"`, `"auto_index": 1`, `"workspaceId"`, or
//! `{"workspace_id": {"id": "..."}}` used to fail with `Validation error:
//! invalid type ...` before the call reached the API. In the hosted gateway
//! that was 2,024 tool errors in the seven days to 2026-10-17, from clients
//! whose every call failed, among them `mcporter` and other CLI bridges that
//! pass values as strings.
//!
//! [`normalize_arguments`] rewrites the argument object against the tool's
//! advertised (portable) input schema, where each property has one `type`:
//!
//! - `null` arguments become `{}`, and arguments sent as a JSON string are parsed;
//! - an envelope (`{"arguments": {...}}`, also `args`, `input`, `params`) that
//!   the schema does not define is unwrapped;
//! - camelCase or kebab-case keys are renamed to the schema's snake_case key,
//!   unless that key is already present;
//! - `{"workspace": {"id": ...}}` / `{"project": {"id": ...}}` fill
//!   `workspace_id` / `project_id` when the schema has them and they are absent;
//! - values follow the property's type: booleans from `"true"`/`"false"`/`"1"`/
//!   `"0"`/`"yes"`/`"no"`/`"on"`/`"off"` or `0`/`1`; numbers from numeric
//!   strings; strings from numbers, booleans, `{"id": ...}`, or a one-item
//!   array; arrays from a single scalar or a JSON array string; objects from a
//!   JSON object string.
//!
//! Anything else is left as it was, so a handler still rejects it, now with
//! the field names that do not match the schema ([`schema_type_mismatches`]).
//! Nothing here reads or changes authorization: scope ids are only moved
//! between keys, never invented, and every value is still checked by the
//! handler and by the API.

use serde_json::{Map, Value};

/// One rewrite applied to the arguments: the schema property and what was done.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShapeChange {
    pub field: String,
    pub kind: &'static str,
}

const ENVELOPE_KEYS: &[&str] = &["arguments", "args", "input", "params"];

fn properties(schema: &Value) -> Option<&Map<String, Value>> {
    schema.get("properties").and_then(Value::as_object)
}

fn property_type(property: &Value) -> Option<&str> {
    property.get("type").and_then(Value::as_str)
}

/// `workspaceId`, `workspace-id`, `WorkspaceID` and ` workspace_id ` all map to `workspace_id`.
pub fn snake_case(key: &str) -> String {
    let mut out = String::with_capacity(key.len() + 4);
    let chars: Vec<char> = key.trim().chars().collect();
    for (i, c) in chars.iter().enumerate() {
        if *c == '-' || *c == ' ' || *c == '.' {
            out.push('_');
        } else if c.is_ascii_uppercase() {
            let prev_lower =
                i > 0 && (chars[i - 1].is_ascii_lowercase() || chars[i - 1].is_ascii_digit());
            let next_lower = chars.get(i + 1).is_some_and(|n| n.is_ascii_lowercase());
            let prev_upper = i > 0 && chars[i - 1].is_ascii_uppercase();
            if i > 0 && !out.ends_with('_') && (prev_lower || (prev_upper && next_lower)) {
                out.push('_');
            }
            out.push(c.to_ascii_lowercase());
        } else {
            out.push(*c);
        }
    }
    out
}

fn parse_json_container(text: &str, want_object: bool) -> Option<Value> {
    let trimmed = text.trim();
    let looks = if want_object {
        trimmed.starts_with('{') && trimmed.ends_with('}')
    } else {
        trimmed.starts_with('[') && trimmed.ends_with(']')
    };
    if !looks {
        return None;
    }
    serde_json::from_str::<Value>(trimmed).ok().filter(|v| {
        if want_object {
            v.is_object()
        } else {
            v.is_array()
        }
    })
}

fn bool_from_str(text: &str) -> Option<bool> {
    match text.trim().to_ascii_lowercase().as_str() {
        "true" | "1" | "yes" | "y" | "on" => Some(true),
        "false" | "0" | "no" | "n" | "off" => Some(false),
        _ => None,
    }
}

/// The single string an `{"id": ...}`-style object stands for, if it has one.
fn object_scalar_string(object: &Map<String, Value>) -> Option<String> {
    for key in ["id", "uuid", "value"] {
        if let Some(Value::String(s)) = object.get(key) {
            return Some(s.clone());
        }
    }
    if object.len() == 1 {
        if let Some(Value::String(s)) = object.values().next() {
            return Some(s.clone());
        }
    }
    None
}

/// The value coerced to the property's type, with the kind of change, or None to leave it.
fn coerce(field: &str, ty: &str, value: &Value) -> Option<(Value, &'static str)> {
    match (ty, value) {
        ("boolean", Value::String(s)) => {
            bool_from_str(s).map(|b| (Value::Bool(b), "string_to_boolean"))
        }
        ("boolean", Value::Number(n)) => {
            let v = n.as_f64()?;
            if v == 1.0 {
                Some((Value::Bool(true), "number_to_boolean"))
            } else if v == 0.0 {
                Some((Value::Bool(false), "number_to_boolean"))
            } else {
                None
            }
        }
        ("integer", Value::String(s)) => s
            .trim()
            .parse::<i64>()
            .ok()
            .map(|n| (Value::from(n), "string_to_integer")),
        ("integer", Value::Number(n)) if n.is_f64() => n
            .as_f64()
            .filter(|v| v.fract() == 0.0 && v.abs() < 9.0e15)
            .map(|v| (Value::from(v as i64), "float_to_integer")),
        ("number", Value::String(s)) => s
            .trim()
            .parse::<f64>()
            .ok()
            .filter(|v| v.is_finite())
            .and_then(serde_json::Number::from_f64)
            .map(|n| (Value::Number(n), "string_to_number")),
        ("string", Value::Number(n)) => Some((Value::String(n.to_string()), "scalar_to_string")),
        ("string", Value::Bool(b)) => Some((Value::String(b.to_string()), "scalar_to_string")),
        ("string", Value::Object(o)) => {
            if let Some(s) = object_scalar_string(o) {
                Some((Value::String(s), "object_to_string"))
            } else if field.ends_with("_id") || field == "id" {
                // An id object without an id: leave it for a clear rejection.
                None
            } else {
                serde_json::to_string(value)
                    .ok()
                    .map(|s| (Value::String(s), "object_to_json_string"))
            }
        }
        ("string", Value::Array(items)) if items.len() == 1 => match &items[0] {
            Value::String(s) => Some((Value::String(s.clone()), "single_item_array")),
            Value::Number(n) => Some((Value::String(n.to_string()), "single_item_array")),
            Value::Bool(b) => Some((Value::String(b.to_string()), "single_item_array")),
            Value::Object(o) => {
                object_scalar_string(o).map(|s| (Value::String(s), "single_item_array"))
            }
            _ => None,
        },
        ("array", Value::String(s)) => match parse_json_container(s, false) {
            Some(parsed) => Some((parsed, "json_string_to_array")),
            None => Some((Value::Array(vec![value.clone()]), "scalar_to_array")),
        },
        ("array", Value::Number(_) | Value::Bool(_)) => {
            Some((Value::Array(vec![value.clone()]), "scalar_to_array"))
        }
        ("object", Value::String(s)) => {
            parse_json_container(s, true).map(|parsed| (parsed, "json_string_to_object"))
        }
        _ => None,
    }
}

/// Rewrite `input` in place to the shapes `schema` describes; returns what changed.
pub fn normalize_arguments(schema: &Value, input: &mut Value) -> Vec<ShapeChange> {
    let mut changes = Vec::new();
    match input {
        Value::Null => {
            *input = Value::Object(Map::new());
            changes.push(ShapeChange {
                field: String::new(),
                kind: "null_arguments",
            });
        }
        Value::String(text) => {
            if let Some(parsed) = parse_json_container(text, true) {
                *input = parsed;
                changes.push(ShapeChange {
                    field: String::new(),
                    kind: "json_string_arguments",
                });
            }
        }
        _ => {}
    }
    let Some(props) = properties(schema) else {
        return changes;
    };

    // {"arguments": {...}} and friends, when the schema has no such property.
    if let Some(object) = input.as_object() {
        if object.len() == 1 {
            let (key, inner) = object.iter().next().expect("one key");
            if ENVELOPE_KEYS.contains(&key.as_str()) && !props.contains_key(key) {
                let unwrapped = match inner {
                    Value::Object(_) => Some(inner.clone()),
                    Value::String(text) => parse_json_container(text, true),
                    _ => None,
                };
                if let Some(unwrapped) = unwrapped {
                    changes.push(ShapeChange {
                        field: key.clone(),
                        kind: "arguments_envelope",
                    });
                    *input = unwrapped;
                }
            }
        }
    }

    let Some(object) = input.as_object_mut() else {
        return changes;
    };

    // Key aliases: camelCase / kebab-case / padded keys onto the schema's key.
    let unknown: Vec<String> = object
        .keys()
        .filter(|k| !props.contains_key(k.as_str()))
        .cloned()
        .collect();
    for key in unknown {
        let canonical = snake_case(&key);
        if canonical != key && props.contains_key(&canonical) && !object.contains_key(&canonical) {
            if let Some(value) = object.remove(&key) {
                object.insert(canonical.clone(), value);
                changes.push(ShapeChange {
                    field: canonical,
                    kind: "key_alias",
                });
            }
        }
    }

    // {"workspace": {"id": ...}} -> workspace_id (same for project).
    for (nested, flat) in [("workspace", "workspace_id"), ("project", "project_id")] {
        if props.contains_key(flat) && !props.contains_key(nested) && !object.contains_key(flat) {
            let id = object
                .get(nested)
                .and_then(Value::as_object)
                .and_then(|o| o.get("id"))
                .and_then(Value::as_str)
                .map(str::to_string);
            if let Some(id) = id {
                object.insert(flat.to_string(), Value::String(id));
                changes.push(ShapeChange {
                    field: flat.to_string(),
                    kind: "nested_scope_id",
                });
            }
        }
    }

    // Values to the property's type.
    for (field, property) in props {
        let Some(ty) = property_type(property) else {
            continue;
        };
        let Some(value) = object.get(field) else {
            continue;
        };
        if value.is_null() {
            continue;
        }
        if let Some((coerced, kind)) = coerce(field, ty, value) {
            object.insert(field.clone(), coerced);
            changes.push(ShapeChange {
                field: field.clone(),
                kind,
            });
        }
    }
    changes
}

fn json_type(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(n) if n.is_i64() || n.is_u64() => "integer",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

/// Properties whose present, non-null value still does not have the schema's
/// type, as `` `field` must be a T (got U) `` (names and types only, no values).
pub fn schema_type_mismatches(schema: &Value, input: &Value) -> Vec<String> {
    let (Some(props), Some(object)) = (properties(schema), input.as_object()) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for (field, property) in props {
        let (Some(ty), Some(value)) = (property_type(property), object.get(field)) else {
            continue;
        };
        let got = json_type(value);
        let ok = got == "null" || got == ty || (ty == "number" && got == "integer");
        if !ok {
            out.push(format!("`{field}` must be {} (got {got})", article(ty)));
        }
    }
    out
}

fn article(ty: &str) -> String {
    match ty {
        "integer" | "object" | "array" => format!("an {ty}"),
        _ => format!("a {ty}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn init_schema() -> Value {
        json!({
            "type": "object",
            "properties": {
                "workspace_id": {"type": "string"},
                "project_id": {"type": "string"},
                "folder_path": {"type": "string"},
                "session_id": {"type": "string"},
                "context_hint": {"type": "string"},
                "include_recent_memory": {"type": "boolean"},
                "force": {"type": "boolean"},
                "auto_index": {"type": "boolean"},
                "limit": {"type": "integer"},
                "threshold": {"type": "number"},
                "tags": {"type": "array", "items": {"type": "string"}},
                "metadata": {"type": "object"}
            }
        })
    }

    fn kinds(changes: &[ShapeChange]) -> Vec<(&str, &str)> {
        changes.iter().map(|c| (c.field.as_str(), c.kind)).collect()
    }

    const W: &str = "11111111-1111-4111-8111-111111111111";
    const P: &str = "22222222-2222-4222-8222-222222222222";

    #[test]
    fn well_formed_arguments_are_untouched() {
        let mut input = json!({"workspace_id": W, "force": true, "limit": 5, "tags": ["a"]});
        let before = input.clone();
        assert!(normalize_arguments(&init_schema(), &mut input).is_empty());
        assert_eq!(input, before);
    }

    #[test]
    fn booleans_sent_as_strings_or_numbers() {
        let mut input = json!({"force": "true", "include_recent_memory": "False", "auto_index": 1});
        let changes = normalize_arguments(&init_schema(), &mut input);
        assert_eq!(
            input,
            json!({"force": true, "include_recent_memory": false, "auto_index": true})
        );
        assert_eq!(changes.len(), 3);
        for word in ["yes", "on", "1", " TRUE "] {
            let mut input = json!({"force": word});
            normalize_arguments(&init_schema(), &mut input);
            assert_eq!(input["force"], json!(true), "{word}");
        }
        for word in ["no", "off", "0"] {
            let mut input = json!({"force": word});
            normalize_arguments(&init_schema(), &mut input);
            assert_eq!(input["force"], json!(false), "{word}");
        }
    }

    #[test]
    fn unparseable_boolean_is_left_for_a_clear_rejection() {
        let mut input = json!({"force": "maybe", "auto_index": 7});
        assert!(normalize_arguments(&init_schema(), &mut input).is_empty());
        let mismatches = schema_type_mismatches(&init_schema(), &input);
        assert!(mismatches.contains(&"`force` must be a boolean (got string)".to_string()));
        assert!(mismatches.contains(&"`auto_index` must be a boolean (got integer)".to_string()));
    }

    #[test]
    fn numbers_sent_as_strings() {
        let mut input = json!({"limit": " 12 ", "threshold": "0.25"});
        normalize_arguments(&init_schema(), &mut input);
        assert_eq!(input, json!({"limit": 12, "threshold": 0.25}));
        let mut input = json!({"limit": 3.0});
        assert_eq!(
            kinds(&normalize_arguments(&init_schema(), &mut input)),
            vec![("limit", "float_to_integer")]
        );
        assert_eq!(input["limit"], json!(3));
    }

    #[test]
    fn strings_sent_as_numbers_booleans_objects_or_one_item_arrays() {
        let mut input = json!({
            "session_id": 12345,
            "context_hint": true,
            "workspace_id": {"id": W},
            "folder_path": ["/home/u/project"],
            "project_id": [P],
        });
        normalize_arguments(&init_schema(), &mut input);
        assert_eq!(
            input,
            json!({
                "session_id": "12345",
                "context_hint": "true",
                "workspace_id": W,
                "folder_path": "/home/u/project",
                "project_id": P,
            })
        );
    }

    #[test]
    fn text_field_object_becomes_json_text_but_id_object_without_id_is_left() {
        let mut input = json!({"context_hint": {"text": "fix the login bug", "lang": "en"}, "workspace_id": {"name": "x", "other": 1}});
        let changes = normalize_arguments(&init_schema(), &mut input);
        assert_eq!(
            kinds(&changes),
            vec![("context_hint", "object_to_json_string")]
        );
        assert!(input["context_hint"]
            .as_str()
            .unwrap()
            .contains("fix the login bug"));
        assert!(input["workspace_id"].is_object());
        assert_eq!(
            schema_type_mismatches(&init_schema(), &input),
            vec!["`workspace_id` must be a string (got object)".to_string()]
        );
    }

    #[test]
    fn camel_and_kebab_keys_map_to_schema_keys() {
        let mut input = json!({"workspaceId": W, "project-id": P, "IncludeRecentMemory": "true", " folder_path ": "/x"});
        normalize_arguments(&init_schema(), &mut input);
        assert_eq!(
            input,
            json!({"workspace_id": W, "project_id": P, "include_recent_memory": true, "folder_path": "/x"})
        );
    }

    #[test]
    fn alias_never_overwrites_the_canonical_key() {
        let mut input = json!({"workspace_id": W, "workspaceId": P});
        normalize_arguments(&init_schema(), &mut input);
        assert_eq!(input["workspace_id"], json!(W));
        assert_eq!(
            input["workspaceId"],
            json!(P),
            "the unknown key is left as it was"
        );
    }

    #[test]
    fn unknown_keys_are_left_in_place() {
        let mut input = json!({"workspace_id": W, "trace": {"a": 1}, "client": "bot"});
        assert!(normalize_arguments(&init_schema(), &mut input).is_empty());
        assert_eq!(input["trace"], json!({"a": 1}));
    }

    #[test]
    fn envelopes_and_string_arguments_are_unwrapped() {
        let mut input = json!({"arguments": {"workspace_id": W, "force": "true"}});
        let changes = normalize_arguments(&init_schema(), &mut input);
        assert_eq!(input, json!({"workspace_id": W, "force": true}));
        assert_eq!(changes[0].kind, "arguments_envelope");

        let mut input =
            json!({"params": "{\"workspace_id\": \"11111111-1111-4111-8111-111111111111\"}"});
        normalize_arguments(&init_schema(), &mut input);
        assert_eq!(input, json!({"workspace_id": W}));

        let mut input = Value::String(format!("{{\"workspace_id\": \"{W}\"}}"));
        normalize_arguments(&init_schema(), &mut input);
        assert_eq!(input, json!({"workspace_id": W}));

        let mut input = Value::Null;
        normalize_arguments(&init_schema(), &mut input);
        assert_eq!(input, json!({}));
    }

    #[test]
    fn an_envelope_key_the_schema_defines_is_not_unwrapped() {
        let schema = json!({"type": "object", "properties": {"input": {"type": "string"}}});
        let mut input = json!({"input": {"text": "hello"}});
        normalize_arguments(&schema, &mut input);
        assert_eq!(
            input,
            json!({"input": "hello"}),
            "coerced as the property, not unwrapped"
        );
    }

    #[test]
    fn nested_workspace_and_project_objects_fill_flat_ids() {
        let mut input = json!({"workspace": {"id": W, "name": "Eng"}, "project": {"id": P}});
        let changes = normalize_arguments(&init_schema(), &mut input);
        assert_eq!(input["workspace_id"], json!(W));
        assert_eq!(input["project_id"], json!(P));
        assert_eq!(
            changes
                .iter()
                .filter(|c| c.kind == "nested_scope_id")
                .count(),
            2
        );

        let mut explicit = json!({"workspace_id": W, "workspace": {"id": P}});
        normalize_arguments(&init_schema(), &mut explicit);
        assert_eq!(
            explicit["workspace_id"],
            json!(W),
            "an explicit id always wins"
        );
    }

    #[test]
    fn arrays_and_objects_from_scalars_and_json_text() {
        let mut input = json!({"tags": "deploy", "metadata": "{\"k\": 1}"});
        normalize_arguments(&init_schema(), &mut input);
        assert_eq!(input, json!({"tags": ["deploy"], "metadata": {"k": 1}}));
        let mut input = json!({"tags": "[\"a\", \"b\"]"});
        normalize_arguments(&init_schema(), &mut input);
        assert_eq!(input["tags"], json!(["a", "b"]));
    }

    #[test]
    fn nulls_stay_null() {
        let mut input = json!({"workspace_id": null, "force": null});
        assert!(normalize_arguments(&init_schema(), &mut input).is_empty());
        assert!(schema_type_mismatches(&init_schema(), &input).is_empty());
    }

    #[test]
    fn snake_case_covers_common_spellings() {
        assert_eq!(snake_case("workspaceId"), "workspace_id");
        assert_eq!(snake_case("WorkspaceID"), "workspace_id");
        assert_eq!(snake_case("include-recent-memory"), "include_recent_memory");
        assert_eq!(snake_case("contextHint"), "context_hint");
        assert_eq!(snake_case("project_id"), "project_id");
        assert_eq!(snake_case("isPostCompact"), "is_post_compact");
    }
}
