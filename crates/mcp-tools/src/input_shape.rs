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

/// Arguments that count or page results. The API answers a negative one with
/// a raw parse or database error, so none may be negative.
const COUNT_ARGUMENTS: &[&str] = &["limit", "offset", "page", "page_size", "per_page"];

/// The first count argument with a negative value, as
/// `` `limit` can't be negative (got -1) ``.
pub fn negative_count_argument(input: &Value) -> Option<String> {
    let object = input.as_object()?;
    COUNT_ARGUMENTS.iter().find_map(|field| {
        let value = object.get(*field)?.as_i64()?;
        (value < 0).then(|| format!("`{field}` can't be negative (got {value})"))
    })
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

/// Keys tool inputs accept through a serde alias, so not in any schema.
const ACCEPTED_ALIASES: &[&str] = &["encoding", "superseded_by", "said", "reference", "ref"];

/// The arguments that are not the schema's but one edit away from one of its
/// properties (two for longer names), as `` `limt` (did you mean `limit`?) ``
/// (#423). A misspelled argument was dropped, so `priorty` lost the priority
/// and `limt` returned the default page. Other unknown keys are left alone.
pub fn mistyped_arguments(schema: &Value, input: &Value) -> Vec<String> {
    let (Some(props), Some(object)) = (properties(schema), input.as_object()) else {
        return Vec::new();
    };
    object
        .keys()
        .filter(|key| {
            !props.contains_key(key.as_str())
                && !key.starts_with('_')
                && !ACCEPTED_ALIASES.contains(&key.as_str())
        })
        .filter_map(|key| {
            let near = |name: &&String| {
                let singular_or_plural =
                    name.strip_suffix('s') == Some(key) || key.strip_suffix('s') == Some(name);
                let allowed = if key.chars().count() <= 5 { 1 } else { 2 };
                !singular_or_plural && typo_distance(key, name) <= allowed
            };
            let closest = props
                .keys()
                .filter(near)
                .min_by_key(|name| typo_distance(key, name))?;
            Some(format!("`{key}` (did you mean `{closest}`?)"))
        })
        .collect()
}

/// Edits (insert, delete, change, swap of neighbours) from `a` to `b`.
fn typo_distance(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    let mut d = vec![vec![0usize; b.len() + 1]; a.len() + 1];
    for (i, row) in d.iter_mut().enumerate() {
        row[0] = i;
    }
    for (j, cell) in d[0].iter_mut().enumerate() {
        *cell = j;
    }
    for i in 1..=a.len() {
        for j in 1..=b.len() {
            let cost = usize::from(a[i - 1] != b[j - 1]);
            let mut best = (d[i - 1][j] + 1)
                .min(d[i][j - 1] + 1)
                .min(d[i - 1][j - 1] + cost);
            if i > 1 && j > 1 && a[i - 1] == b[j - 2] && a[i - 2] == b[j - 1] {
                best = best.min(d[i - 2][j - 2] + 1);
            }
            d[i][j] = best;
        }
    }
    d[a.len()][b.len()]
}

/// Why a `tools/call` request's own `params.arguments` is refused as invalid
/// params (`-32602`), or None when it is accepted.
///
/// An object is accepted, and so are the two shapes [`normalize_arguments`]
/// rewrites: absent or `null` arguments (read as no arguments) and a JSON
/// string holding an object. Anything else (a number, a boolean, an array, or
/// a string that is not a JSON object) is refused before the tool runs.
pub fn tools_call_arguments_error(tool: &str, arguments: Option<&Value>) -> Option<String> {
    let got = match arguments? {
        Value::Null | Value::Object(_) => return None,
        Value::String(text) if parse_json_container(text, true).is_some() => return None,
        Value::String(_) => "a string that is not a JSON object".to_string(),
        other => article(json_type(other)),
    };
    Some(format!(
        "Invalid params: tools/call arguments for {tool} must be an object (got {got}). \
         Send the arguments as a JSON object with the fields tools/list advertises for {tool}."
    ))
}

/// `value` as one of the values a schema enum advertises for `field`, for the
/// arguments that store it (#350). Case, surrounding space, and `-` or a space
/// for `_` do not matter, so `"Release-Notes"` is `release_notes`; a blank
/// value is no value. Anything else is refused with the advertised values, and
/// the one that starts the other when there is exactly one (`notes` → `note`).
pub fn enum_argument(
    field: &str,
    value: Option<String>,
    allowed: &[&str],
) -> Result<Option<String>, String> {
    let Some(value) = value else {
        return Ok(None);
    };
    let typed = value.trim();
    if typed.is_empty() {
        return Ok(None);
    }
    let wanted: String = typed
        .chars()
        .map(|c| match c {
            '-' | ' ' => '_',
            c => c.to_ascii_lowercase(),
        })
        .collect();
    if let Some(found) = allowed.iter().find(|candidate| **candidate == wanted) {
        return Ok(Some((*found).to_string()));
    }
    let mut message = format!("{field} \"{typed}\" is not one of: {}.", allowed.join(", "));
    let close: Vec<&str> = allowed
        .iter()
        .copied()
        .filter(|candidate| candidate.starts_with(&wanted) || wanted.starts_with(candidate))
        .collect();
    if let [only] = close.as_slice() {
        message.push_str(&format!(" Did you mean \"{only}\"?"));
    }
    Err(message)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_misspelled_argument_is_named_with_the_one_meant() {
        // #423: limt, priorty, severty, qurey and projet_id were dropped.
        let schema = json!({"properties": {
            "action": {}, "limit": {}, "priority": {}, "severity": {}, "query": {},
            "project_id": {}, "tags": {}, "node_id": {}
        }});
        let input = json!({"action": "x", "limt": 2, "priorty": "urgent", "severty": "high",
                           "qurey": "q", "projet_id": "p"});
        let mut found = mistyped_arguments(&schema, &input);
        found.sort();
        assert_eq!(
            found,
            vec![
                "`limt` (did you mean `limit`?)",
                "`priorty` (did you mean `priority`?)",
                "`projet_id` (did you mean `project_id`?)",
                "`qurey` (did you mean `query`?)",
                "`severty` (did you mean `severity`?)",
            ]
        );
        // Known keys, plurals, aliases, _meta and unrelated keys are left alone.
        let fine = json!({"action": "x", "tag": "t", "node_ids": [], "ref": "r",
                          "_meta": {}, "something_else": 1});
        assert!(mistyped_arguments(&schema, &fine).is_empty());
    }

    #[test]
    fn an_enum_argument_is_one_of_the_advertised_values_or_refused_with_them() {
        const TYPES: &[&str] = &["note", "release_notes", "general"];
        let check = |value: Option<&str>| enum_argument("doc_type", value.map(String::from), TYPES);

        assert_eq!(check(None), Ok(None));
        assert_eq!(check(Some("  ")), Ok(None));
        assert_eq!(check(Some("general")), Ok(Some("general".to_string())));
        assert_eq!(
            check(Some(" Release-Notes ")),
            Ok(Some("release_notes".to_string()))
        );
        assert_eq!(
            check(Some("release notes")),
            Ok(Some("release_notes".to_string()))
        );
        assert_eq!(
            check(Some("bogus_type")),
            Err("doc_type \"bogus_type\" is not one of: note, release_notes, general.".to_string())
        );
        assert_eq!(
            check(Some("notes")),
            Err("doc_type \"notes\" is not one of: note, release_notes, general. Did you mean \"note\"?"
                .to_string())
        );
        assert_eq!(
            check(Some("gen")),
            Err("doc_type \"gen\" is not one of: note, release_notes, general. Did you mean \"general\"?"
                .to_string())
        );
    }

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
    fn tools_call_arguments_must_be_an_object() {
        for accepted in [
            None,
            Some(json!(null)),
            Some(json!({})),
            Some(json!({"action": "list"})),
            Some(json!("{\"action\": \"list\"}")),
        ] {
            assert_eq!(
                tools_call_arguments_error("memory", accepted.as_ref()),
                None,
                "{accepted:?}"
            );
        }
        for (refused, got) in [
            (json!("[1,2]"), "a string that is not a JSON object"),
            (json!("plain text"), "a string that is not a JSON object"),
            (json!(5), "an integer"),
            (json!(1.5), "a number"),
            (json!(true), "a boolean"),
            (json!([1]), "an array"),
        ] {
            let message = tools_call_arguments_error("memory", Some(&refused))
                .unwrap_or_else(|| panic!("{refused} must be refused"));
            assert!(message.contains(&format!("(got {got})")), "{message}");
            assert!(
                message.contains("tools/list advertises for memory"),
                "{message}"
            );
        }
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
