//! Portable JSON Schema for advertised MCP tool inputs.
//!
//! MCP itself accepts any JSON Schema, but the model providers behind coding
//! harnesses validate tool `parameters` far more strictly, and a single
//! rejected schema fails the whole request, not just one tool:
//!
//! - Moonshot (Kimi) accepts only "Moonshot Flavored JSON Schema" and answers
//!   HTTP 400 otherwise (github.com/MoonshotAI/walle, docs/mfjs-spec). Kimi
//!   Code CLI forwards MCP schemas unmodified (MoonshotAI/kimi-code#792).
//! - Gemini-, GLM-, and Qwen-backed harnesses reject overlapping keyword sets
//!   (type unions, `oneOf`, `const`, `format`, `$ref`).
//!
//! Every advertised schema is therefore rewritten once, at registration, into
//! the subset all of them accept:
//!
//! - only `type`, `properties`, `required`, `items`, `anyOf`, `enum`,
//!   `additionalProperties`, `description`, and `default` survive;
//! - `type` is a single string; `["T", "null"]` becomes `T` because
//!   optionality is already carried by `required`; other unions become typed
//!   `anyOf` branches, and `anyOf` never sits beside `type`;
//! - `oneOf` becomes `anyOf`, `allOf` is merged, `const` becomes a one-value
//!   `enum`, and an `enum` holds one scalar type and never `null`;
//! - `format`, numeric and length bounds, `pattern`, and `examples` are folded
//!   into `description`, so models that could use them keep the guidance;
//! - every node has a `type` (inferred from its shape when missing), and
//!   `additionalProperties: {}` becomes `true`;
//! - `$ref` into `definitions`/`$defs` is inlined.
//!
//! The server never validates arguments against these schemas (handlers parse
//! their own input), so normalization changes what models see, never what
//! the server accepts.

use serde_json::{json, Map, Value};

/// Keywords that survive normalization.
pub const PORTABLE_SCHEMA_KEYWORDS: &[&str] = &[
    "type",
    "properties",
    "required",
    "items",
    "anyOf",
    "enum",
    "additionalProperties",
    "description",
    "default",
];

const PORTABLE_TYPES: &[&str] = &[
    "null", "boolean", "object", "array", "number", "integer", "string",
];

/// Types an unconstrained value may take, in the order they are advertised.
const ANY_VALUE_TYPES: &[&str] = &["string", "number", "boolean", "object", "array"];

/// Recursion guard for inlined `$ref`s and pathological nesting.
const MAX_DEPTH: usize = 32;

/// Rewrite a tool input schema into the portable subset (see module docs).
///
/// The result is always an object schema with `properties` (plus an empty
/// `required` when it takes no arguments), and normalization is idempotent.
pub fn portable_input_schema(schema: &Value) -> Value {
    let definitions = collect_definitions(schema);
    let mut root = normalize(schema, &definitions, &mut Vec::new(), 0);

    let Some(object) = root.as_object_mut() else {
        return empty_object_schema();
    };
    if object.contains_key("anyOf") || object.get("type") != Some(&json!("object")) {
        // A root that is not a plain object schema cannot be a tool's
        // arguments; keep only its description.
        let description = object.remove("description");
        root = empty_object_schema();
        if let Some(description) = description {
            root["description"] = description;
        }
        return root;
    }
    object
        .entry("properties")
        .or_insert_with(|| Value::Object(Map::new()));
    let no_properties = object
        .get("properties")
        .and_then(Value::as_object)
        .is_some_and(Map::is_empty);
    if no_properties {
        // Kimi has rejected argument-less tools declared as a bare `{}`
        // object; an explicit empty `required` is accepted everywhere.
        object
            .entry("required")
            .or_insert_with(|| Value::Array(Vec::new()));
    }
    root
}

/// Describe every way `schema` departs from the portable subset.
///
/// Each entry is `"<json-pointer>: <problem>"`; an empty result means the
/// schema is portable. Used by tests and diagnostics, never on the hot path.
pub fn portable_schema_violations(schema: &Value) -> Vec<String> {
    let mut violations = Vec::new();
    match schema.as_object() {
        Some(root) if root.get("type") == Some(&json!("object")) => {
            if !root.get("properties").is_some_and(Value::is_object) {
                violations.push("/: root object schema has no properties map".to_string());
            }
        }
        _ => violations.push("/: root is not an object schema".to_string()),
    }
    collect_violations(schema, "", &mut violations);
    violations
}

fn empty_object_schema() -> Value {
    json!({ "type": "object", "properties": {}, "required": [] })
}

/// "Any JSON value" in portable form. Array elements are scalars or objects
/// so the schema stays finite while every array still declares `items`.
fn any_value_schema() -> Value {
    let branches = ANY_VALUE_TYPES
        .iter()
        .map(|kind| match *kind {
            "array" => json!({ "type": "array", "items": any_element_schema() }),
            kind => json!({ "type": kind }),
        })
        .collect::<Vec<_>>();
    json!({ "anyOf": branches })
}

fn any_element_schema() -> Value {
    let branches = ANY_VALUE_TYPES
        .iter()
        .filter(|kind| **kind != "array")
        .map(|kind| json!({ "type": kind }))
        .collect::<Vec<_>>();
    json!({ "anyOf": branches })
}

fn collect_definitions(schema: &Value) -> Map<String, Value> {
    let mut definitions = Map::new();
    for key in ["definitions", "$defs"] {
        if let Some(defs) = schema.get(key).and_then(Value::as_object) {
            for (name, def) in defs {
                definitions.insert(format!("#/{key}/{name}"), def.clone());
            }
        }
    }
    definitions
}

fn normalize(
    value: &Value,
    definitions: &Map<String, Value>,
    ref_stack: &mut Vec<String>,
    depth: usize,
) -> Value {
    if depth > MAX_DEPTH {
        return json!({ "type": "object" });
    }
    match value {
        // `true` / `{}` accept anything; `false` accepts nothing, which no
        // tool argument means, so treat it the same way.
        Value::Bool(_) => any_value_schema(),
        Value::Object(object) if object.is_empty() => any_value_schema(),
        Value::Object(object) => normalize_object(object, definitions, ref_stack, depth),
        // Not a schema at all; the most honest portable reading is "any".
        _ => any_value_schema(),
    }
}

fn normalize_object(
    source: &Map<String, Value>,
    definitions: &Map<String, Value>,
    ref_stack: &mut Vec<String>,
    depth: usize,
) -> Value {
    let mut node = source.clone();

    if let Some(reference) = node.remove("$ref") {
        let resolved = reference
            .as_str()
            .filter(|reference| !ref_stack.iter().any(|seen| seen == reference))
            .and_then(|reference| definitions.get(reference).map(|def| (reference, def)));
        match resolved {
            Some((reference, definition)) => {
                ref_stack.push(reference.to_string());
                let inlined = normalize(definition, definitions, ref_stack, depth + 1);
                ref_stack.pop();
                // Sibling keywords (usually a local description) win.
                if let Value::Object(inlined) = inlined {
                    for (key, value) in inlined {
                        node.entry(key).or_insert(value);
                    }
                }
            }
            // Unresolvable or recursive: an object is the safest stand-in
            // for the structured values our definitions describe.
            None => {
                node.entry("type").or_insert_with(|| json!("object"));
            }
        }
    }

    // Conditional requirements (`if`/`then`, alone or inside `allOf`) have no
    // portable form; keep them as prose so models still see the rule.
    let mut conditional_notes: Vec<String> =
        conditional_requirement_note(&node).into_iter().collect();
    if let Some(Value::Array(branches)) = node.remove("allOf") {
        for branch in branches {
            match branch.as_object() {
                Some(conditional) if conditional.contains_key("if") => {
                    conditional_notes.extend(conditional_requirement_note(conditional));
                }
                _ => merge_all_of_branch(&mut node, &branch, definitions, ref_stack, depth),
            }
        }
    }
    for key in ["if", "then", "else"] {
        node.remove(key);
    }
    if !conditional_notes.is_empty() {
        let description = node
            .get("description")
            .and_then(Value::as_str)
            .unwrap_or_default();
        node.insert(
            "description".to_string(),
            json!(append_sentences(description, &conditional_notes.join(" "))),
        );
    }

    if let Some(Value::Array(mut branches)) = node.remove("oneOf") {
        if let Some(Value::Array(existing)) = node.remove("anyOf") {
            branches.extend(existing);
        }
        node.insert("anyOf".to_string(), Value::Array(branches));
    }

    if let Some(constant) = node.remove("const") {
        node.insert("enum".to_string(), Value::Array(vec![constant]));
    }

    if node.get("title").is_some() && node.get("description").is_none() {
        if let Some(title) = node.remove("title") {
            node.insert("description".to_string(), title);
        }
    }
    fold_constraints_into_description(&mut node);

    let mut out = Map::new();
    if let Some(description) = node.get("description").and_then(Value::as_str) {
        if !description.is_empty() {
            out.insert("description".to_string(), json!(description));
        }
    }

    // Single-string type, or a union expressed as typed anyOf branches.
    let mut kind = None;
    let mut union_branches = None;
    match node.get("type") {
        Some(Value::String(t)) if PORTABLE_TYPES.contains(&t.as_str()) => kind = Some(t.clone()),
        Some(Value::Array(types)) => {
            let mut concrete: Vec<String> = types
                .iter()
                .filter_map(Value::as_str)
                .filter(|t| PORTABLE_TYPES.contains(t) && *t != "null")
                .map(str::to_string)
                .collect();
            concrete.dedup();
            match concrete.len() {
                0 if types.iter().any(|t| t == "null") => kind = Some("null".to_string()),
                0 => {}
                1 => kind = concrete.pop(),
                _ => union_branches = Some(concrete),
            }
        }
        _ => {}
    }

    if let Some(Value::Array(branches)) = node.get("anyOf") {
        let mut normalized: Vec<Value> = branches
            .iter()
            .map(|branch| normalize(branch, definitions, ref_stack, depth + 1))
            .collect();
        // `anyOf: [T, null]` is how generators spell "optional T".
        if normalized.len() > 1 {
            normalized.retain(|branch| branch.get("type") != Some(&json!("null")));
        }
        dedupe_values(&mut normalized);
        if normalized.len() == 1 {
            let only = normalized.pop().expect("one branch");
            if let Value::Object(only) = only {
                for (key, value) in only {
                    if key == "description" && out.contains_key("description") {
                        continue;
                    }
                    out.insert(key, value);
                }
            }
            return finish_node(out);
        }
        if !normalized.is_empty() {
            out.insert("anyOf".to_string(), Value::Array(normalized));
            return finish_node(out);
        }
    }

    if let Some(types) = union_branches {
        let branches = types
            .into_iter()
            .map(|t| {
                let mut branch = node.clone();
                branch.insert("type".to_string(), json!(t));
                branch.remove("description");
                normalize(&Value::Object(branch), definitions, ref_stack, depth + 1)
            })
            .collect();
        out.insert("anyOf".to_string(), Value::Array(branches));
        return finish_node(out);
    }

    if let Some(Value::Object(properties)) = node.get("properties") {
        let normalized: Map<String, Value> = properties
            .iter()
            .map(|(name, schema)| {
                (
                    name.clone(),
                    normalize(schema, definitions, ref_stack, depth + 1),
                )
            })
            .collect();
        out.insert("properties".to_string(), Value::Object(normalized));
    }

    if let Some(Value::Array(required)) = node.get("required") {
        let known = out.get("properties").and_then(Value::as_object);
        let mut names: Vec<Value> = Vec::new();
        for name in required.iter().filter_map(Value::as_str) {
            let listed = known.is_some_and(|properties| properties.contains_key(name));
            if listed && !names.iter().any(|seen| seen == name) {
                names.push(json!(name));
            }
        }
        out.insert("required".to_string(), Value::Array(names));
    }

    match node.get("items") {
        // Tuple form: the first position is the best single description.
        Some(Value::Array(tuple)) => {
            let first = tuple.first().cloned().unwrap_or(Value::Bool(true));
            out.insert(
                "items".to_string(),
                normalize(&first, definitions, ref_stack, depth + 1),
            );
        }
        Some(items) => {
            out.insert(
                "items".to_string(),
                normalize(items, definitions, ref_stack, depth + 1),
            );
        }
        None => {}
    }

    match node.get("additionalProperties") {
        Some(Value::Bool(allowed)) => {
            out.insert("additionalProperties".to_string(), json!(allowed));
        }
        Some(Value::Object(schema)) if schema.is_empty() => {
            out.insert("additionalProperties".to_string(), json!(true));
        }
        Some(schema @ Value::Object(_)) => {
            out.insert(
                "additionalProperties".to_string(),
                normalize(schema, definitions, ref_stack, depth + 1),
            );
        }
        _ => {}
    }

    if let Some(Value::Array(values)) = node.get("enum") {
        let (values, enum_kind) = portable_enum(values);
        if let Some(enum_kind) = enum_kind {
            let compatible = match kind.as_deref() {
                None => true,
                Some(current) => {
                    current == enum_kind || (current == "number" && enum_kind == "integer")
                }
            };
            if compatible {
                kind.get_or_insert_with(|| enum_kind.to_string());
                out.insert("enum".to_string(), Value::Array(values));
            } else if kind.as_deref() == Some("boolean") {
                // A boolean enum carries no information beyond the type.
            } else {
                kind = Some("string".to_string());
                let strings = values.iter().map(|v| json!(scalar_to_string(v))).collect();
                out.insert("enum".to_string(), Value::Array(strings));
            }
        } else if kind.is_none() && values.iter().any(Value::is_boolean) {
            kind = Some("boolean".to_string());
        }
    }

    if let Some(default) = node.get("default") {
        if !default.is_null() {
            out.insert("default".to_string(), default.clone());
        }
    }

    let kind = kind.unwrap_or_else(|| infer_kind(&out).to_string());
    if kind == "any" {
        let mut any = any_value_schema();
        if let Some(description) = out.remove("description") {
            any["description"] = description;
        }
        return any;
    }
    out.insert("type".to_string(), json!(kind));
    finish_node(out)
}

/// Final per-node invariants shared by every return path.
fn finish_node(mut out: Map<String, Value>) -> Value {
    if out.contains_key("anyOf") {
        // Branches carry the types; a sibling `type` is rejected by some
        // providers and contradicts mixed branches.
        out.remove("type");
        out.remove("enum");
        return Value::Object(out);
    }
    match out.get("type").and_then(Value::as_str) {
        Some("object") => {
            out.remove("items");
        }
        Some("array") => {
            out.remove("properties");
            out.remove("required");
            out.remove("additionalProperties");
            // Gemini requires `items` on every array.
            out.entry("items").or_insert_with(any_value_schema);
        }
        _ => {
            out.remove("properties");
            out.remove("required");
            out.remove("additionalProperties");
            out.remove("items");
        }
    }
    Value::Object(out)
}

fn merge_all_of_branch(
    node: &mut Map<String, Value>,
    branch: &Value,
    definitions: &Map<String, Value>,
    ref_stack: &mut Vec<String>,
    depth: usize,
) {
    let Value::Object(branch) = normalize(branch, definitions, ref_stack, depth + 1) else {
        return;
    };
    // Only object shapes can be intersected into their parent; a branch that
    // normalizes to a union or a scalar adds nothing portable.
    if branch.get("type") != Some(&json!("object")) {
        return;
    }
    for (key, value) in branch {
        match (key.as_str(), node.get_mut(&key)) {
            ("properties", Some(Value::Object(existing))) => {
                if let Value::Object(extra) = value {
                    for (name, schema) in extra {
                        existing.entry(name).or_insert(schema);
                    }
                }
            }
            ("required", Some(Value::Array(existing))) => {
                if let Value::Array(extra) = value {
                    for name in extra {
                        if !existing.contains(&name) {
                            existing.push(name);
                        }
                    }
                }
            }
            (_, Some(_)) => {}
            (_, None) => {
                node.insert(key, value);
            }
        }
    }
}

/// Keep one scalar enum type, never `null`, never booleans.
fn portable_enum(values: &[Value]) -> (Vec<Value>, Option<&'static str>) {
    let values: Vec<Value> = values
        .iter()
        .filter(|value| !value.is_null() && !value.is_boolean())
        .cloned()
        .collect();
    if values.is_empty() {
        return (values, None);
    }
    if values.iter().all(Value::is_string) {
        return (values, Some("string"));
    }
    if values.iter().all(|v| v.is_i64() || v.is_u64()) {
        return (values, Some("integer"));
    }
    if values.iter().all(Value::is_number) {
        return (values, Some("number"));
    }
    let strings = values.iter().map(|v| json!(scalar_to_string(v))).collect();
    (strings, Some("string"))
}

fn scalar_to_string(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        other => other.to_string(),
    }
}

fn infer_kind(out: &Map<String, Value>) -> &'static str {
    if out.contains_key("properties")
        || out.contains_key("additionalProperties")
        || out.contains_key("required")
    {
        return "object";
    }
    if out.contains_key("items") {
        return "array";
    }
    match out.get("default") {
        Some(Value::String(_)) => "string",
        Some(Value::Bool(_)) => "boolean",
        Some(Value::Number(n)) if n.is_i64() || n.is_u64() => "integer",
        Some(Value::Number(_)) => "number",
        Some(Value::Array(_)) => "array",
        Some(Value::Object(_)) => "object",
        _ => "any",
    }
}

fn dedupe_values(values: &mut Vec<Value>) {
    let mut seen: Vec<Value> = Vec::with_capacity(values.len());
    values.retain(|value| {
        if seen.contains(value) {
            false
        } else {
            seen.push(value.clone());
            true
        }
    });
}

/// Move non-portable validation keywords into prose so the guidance survives.
fn fold_constraints_into_description(node: &mut Map<String, Value>) {
    let description = node
        .get("description")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let lowered = description.to_ascii_lowercase();
    let mut notes: Vec<String> = Vec::new();

    let number =
        |node: &Map<String, Value>, key: &str| node.get(key).filter(|v| v.is_number()).cloned();

    if let Some(format) = node.get("format").and_then(Value::as_str) {
        let label = format_label(format);
        let mentioned = lowered.contains(&format.to_ascii_lowercase())
            || lowered.contains(&label.to_ascii_lowercase());
        if !mentioned {
            notes.push(label);
        }
    }

    let minimum = number(node, "minimum");
    let maximum = number(node, "maximum");
    match (&minimum, &maximum) {
        (Some(min), Some(max)) => notes.push(format!("range {min}-{max}")),
        (Some(min), None) => notes.push(format!("minimum {min}")),
        (None, Some(max)) => notes.push(format!("maximum {max}")),
        (None, None) => {}
    }
    if let Some(min) = number(node, "exclusiveMinimum") {
        notes.push(format!("greater than {min}"));
    }
    if let Some(max) = number(node, "exclusiveMaximum") {
        notes.push(format!("less than {max}"));
    }
    if let Some(step) = number(node, "multipleOf") {
        notes.push(format!("multiple of {step}"));
    }
    match (number(node, "minLength"), number(node, "maxLength")) {
        (Some(min), Some(max)) => notes.push(format!("{min}-{max} characters")),
        (Some(min), None) => notes.push(format!("at least {min} characters")),
        (None, Some(max)) => notes.push(format!("at most {max} characters")),
        (None, None) => {}
    }
    if let Some(pattern) = node.get("pattern").and_then(Value::as_str) {
        notes.push(format!("pattern {pattern}"));
    }
    match (number(node, "minItems"), number(node, "maxItems")) {
        (Some(min), Some(max)) => notes.push(format!("{min}-{max} items")),
        (Some(min), None) => notes.push(format!("at least {min} items")),
        (None, Some(max)) => notes.push(format!("at most {max} items")),
        (None, None) => {}
    }
    if node.get("uniqueItems") == Some(&Value::Bool(true)) {
        notes.push("unique items".to_string());
    }
    if let Some(example) = node
        .get("examples")
        .and_then(Value::as_array)
        .and_then(|examples| examples.first())
    {
        notes.push(format!("e.g. {}", scalar_to_string(example)));
    }

    if notes.is_empty() {
        return;
    }
    let notes = notes.join("; ");
    let trimmed = description.trim_end();
    let merged = if trimmed.ends_with(['.', '!', '?']) || trimmed.is_empty() {
        append_sentences(trimmed, &format!("{}.", capitalize(&notes)))
    } else {
        format!("{trimmed} ({notes})")
    };
    node.insert("description".to_string(), json!(merged));
}

fn format_label(format: &str) -> String {
    match format.to_ascii_lowercase().as_str() {
        "uuid" => "UUID".to_string(),
        "date-time" => "ISO 8601 date-time".to_string(),
        "date" => "ISO 8601 date".to_string(),
        "time" => "ISO 8601 time".to_string(),
        "uri" | "url" | "uri-reference" => "URL".to_string(),
        "email" => "email address".to_string(),
        _ => format!("format {format}"),
    }
}

fn capitalize(text: &str) -> String {
    let mut chars = text.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
        None => String::new(),
    }
}

fn append_sentences(description: &str, sentences: &str) -> String {
    let description = description.trim_end();
    if description.is_empty() {
        sentences.to_string()
    } else if description.ends_with(['.', '!', '?']) {
        format!("{description} {sentences}")
    } else {
        format!("{description}. {sentences}")
    }
}

/// Summarize `if: {properties: {p: {const: v}}}, then/else: {required: [...]}`
/// as "When p=v, also send a, b." Anything more elaborate is dropped.
fn conditional_requirement_note(branch: &Map<String, Value>) -> Option<String> {
    let condition = branch.get("if")?.get("properties")?.as_object()?;
    let when: Vec<String> = condition
        .iter()
        .filter_map(|(name, schema)| {
            let value = schema.get("const").or_else(|| {
                schema
                    .get("enum")
                    .and_then(Value::as_array)
                    .filter(|values| values.len() == 1)
                    .and_then(|values| values.first())
            })?;
            Some(format!("{name}={}", scalar_to_string(value)))
        })
        .collect();
    if when.is_empty() {
        return None;
    }
    let required = |key: &str| {
        let names: Vec<&str> = branch
            .get(key)?
            .get("required")?
            .as_array()?
            .iter()
            .filter_map(Value::as_str)
            .collect();
        (!names.is_empty()).then(|| names.join(", "))
    };
    let mut sentences = Vec::new();
    if let Some(names) = required("then") {
        sentences.push(format!("When {}, also send {names}.", when.join(" and ")));
    }
    if let Some(names) = required("else") {
        sentences.push(format!("Otherwise, also send {names}."));
    }
    (!sentences.is_empty()).then(|| sentences.join(" "))
}

fn collect_violations(value: &Value, pointer: &str, violations: &mut Vec<String>) {
    let at = if pointer.is_empty() { "/" } else { pointer };
    let Some(object) = value.as_object() else {
        violations.push(format!("{at}: schema is not an object"));
        return;
    };
    for key in object.keys() {
        if !PORTABLE_SCHEMA_KEYWORDS.contains(&key.as_str()) {
            violations.push(format!("{at}: keyword `{key}` is not portable"));
        }
    }
    match object.get("type") {
        Some(Value::String(kind)) if PORTABLE_TYPES.contains(&kind.as_str()) => {
            if object.contains_key("anyOf") {
                violations.push(format!("{at}: `type` beside `anyOf`"));
            }
            if kind == "array" && !object.contains_key("items") {
                violations.push(format!("{at}: array without `items`"));
            }
        }
        Some(other) => violations.push(format!(
            "{at}: `type` must be one portable string, got {other}"
        )),
        None if object.contains_key("anyOf") => {}
        None => violations.push(format!("{at}: node has neither `type` nor `anyOf`")),
    }
    if let Some(values) = object.get("enum") {
        let Some(values) = values.as_array() else {
            violations.push(format!("{at}: `enum` is not an array"));
            return;
        };
        let (portable, _) = portable_enum(values);
        if portable.as_slice() != values.as_slice() {
            violations.push(format!(
                "{at}: `enum` must hold one scalar type without null/bool"
            ));
        }
    }
    if let Some(Value::Object(properties)) = object.get("properties") {
        for (name, schema) in properties {
            collect_violations(schema, &format!("{pointer}/properties/{name}"), violations);
        }
    }
    if let Some(items) = object.get("items") {
        collect_violations(items, &format!("{pointer}/items"), violations);
    }
    if let Some(additional) = object.get("additionalProperties") {
        if !additional.is_boolean() {
            collect_violations(
                additional,
                &format!("{pointer}/additionalProperties"),
                violations,
            );
        }
    }
    if let Some(branches) = object.get("anyOf") {
        match branches.as_array() {
            Some(branches) => {
                for (index, branch) in branches.iter().enumerate() {
                    if branch.get("type").is_none() {
                        violations.push(format!("{pointer}/anyOf/{index}: branch has no `type`"));
                    }
                    collect_violations(branch, &format!("{pointer}/anyOf/{index}"), violations);
                }
            }
            None => violations.push(format!("{at}: `anyOf` is not an array")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn portable(schema: Value) -> Value {
        let out = portable_input_schema(&schema);
        let violations = portable_schema_violations(&out);
        assert!(violations.is_empty(), "{violations:#?}\n{out:#}");
        assert_eq!(
            portable_input_schema(&out),
            out,
            "normalization must be idempotent"
        );
        out
    }

    #[test]
    fn uuid_format_is_dropped_without_repeating_the_description() {
        let out = portable(json!({
            "type": "object",
            "properties": {
                "workspace_id": {"type": "string", "format": "uuid", "description": "Workspace ID (UUID)"},
                "since": {"type": "string", "format": "date-time", "description": "Lower bound"}
            }
        }));
        assert_eq!(
            out["properties"]["workspace_id"],
            json!({"type": "string", "description": "Workspace ID (UUID)"})
        );
        assert_eq!(
            out["properties"]["since"]["description"],
            json!("Lower bound (ISO 8601 date-time)")
        );
        assert!(
            out.get("required").is_none(),
            "no churn for schemas with arguments"
        );
    }

    #[test]
    fn bounds_move_into_the_description() {
        let out = portable(json!({
            "type": "object",
            "properties": {
                "limit": {"type": "integer", "minimum": 1, "maximum": 100, "description": "Max results"},
                "tags": {"type": "array", "items": {"type": "string"}, "maxItems": 10, "uniqueItems": true},
                "name": {"type": "string", "maxLength": 64, "pattern": "^[a-z]+$"}
            }
        }));
        assert_eq!(
            out["properties"]["limit"]["description"],
            json!("Max results (range 1-100)")
        );
        assert_eq!(
            out["properties"]["tags"]["description"],
            json!("At most 10 items; unique items.")
        );
        assert_eq!(
            out["properties"]["name"]["description"],
            json!("At most 64 characters; pattern ^[a-z]+$.")
        );
    }

    #[test]
    fn one_of_and_const_become_typed_any_of_and_enum() {
        let out = portable(json!({
            "type": "object",
            "properties": {
                "target": {
                    "oneOf": [
                        {"type": "object", "properties": {"kind": {"const": "item"}, "item_id": {"type": "string", "format": "uuid"}}, "required": ["kind", "item_id"]},
                        {"type": "object", "properties": {"kind": {"const": "citation"}}, "required": ["kind"]}
                    ]
                }
            }
        }));
        let branches = out["properties"]["target"]["anyOf"]
            .as_array()
            .expect("anyOf");
        assert_eq!(branches.len(), 2);
        assert_eq!(
            branches[0]["properties"]["kind"],
            json!({"type": "string", "enum": ["item"]})
        );
        assert!(out["properties"]["target"].get("type").is_none());
    }

    #[test]
    fn untyped_any_of_branches_and_empty_additional_properties_get_types() {
        let out = portable(json!({
            "type": "object",
            "properties": {
                "alternatives": {"type": "array", "items": {"anyOf": [{"type": "string"}, {"type": "object"}]}},
                "data": {"type": "object", "additionalProperties": {}},
                "anything": {}
            }
        }));
        assert_eq!(
            out["properties"]["data"],
            json!({"type": "object", "additionalProperties": true})
        );
        assert_eq!(
            out["properties"]["anything"]["anyOf"]
                .as_array()
                .map(Vec::len),
            Some(ANY_VALUE_TYPES.len())
        );
    }

    #[test]
    fn nullable_type_unions_and_optional_any_of_collapse() {
        let out = portable(json!({
            "type": "object",
            "properties": {
                "a": {"type": ["string", "null"], "description": "Optional"},
                "b": {"anyOf": [{"type": "integer"}, {"type": "null"}]},
                "c": {"type": ["string", "integer"]},
                "d": {"enum": ["x", "y", null]}
            }
        }));
        assert_eq!(
            out["properties"]["a"],
            json!({"type": "string", "description": "Optional"})
        );
        assert_eq!(out["properties"]["b"], json!({"type": "integer"}));
        assert_eq!(
            out["properties"]["c"],
            json!({"anyOf": [{"type": "string"}, {"type": "integer"}]})
        );
        assert_eq!(
            out["properties"]["d"],
            json!({"type": "string", "enum": ["x", "y"]})
        );
    }

    #[test]
    fn schemars_refs_are_inlined_and_meta_keys_dropped() {
        let out = portable(json!({
            "$schema": "http://json-schema.org/draft-07/schema#",
            "title": "Params",
            "type": "object",
            "properties": {"mode": {"$ref": "#/definitions/Mode", "description": "How to run"}},
            "required": ["mode", "ghost"],
            "definitions": {"Mode": {"type": "string", "enum": ["fast", "deep"]}}
        }));
        assert_eq!(
            out["properties"]["mode"],
            json!({"type": "string", "enum": ["fast", "deep"], "description": "How to run"})
        );
        assert_eq!(out["required"], json!(["mode"]));
        assert_eq!(out["description"], json!("Params"));
    }

    #[test]
    fn conditional_requirements_become_prose_without_losing_properties() {
        let out = portable(json!({
            "type": "object",
            "description": "Ask a question.",
            "additionalProperties": false,
            "properties": {
                "action": {"type": "string", "enum": ["ask", "receipt"]},
                "request_id": {"type": "string", "format": "uuid", "description": "Receipt to recover."}
            },
            "allOf": [
                {"if": {"properties": {"action": {"const": "receipt"}}, "required": ["action"]},
                 "then": {"required": ["request_id"]}}
            ]
        }));
        assert_eq!(
            out["description"],
            json!("Ask a question. When action=receipt, also send request_id.")
        );
        assert_eq!(out["additionalProperties"], json!(false));
        assert_eq!(
            out["properties"]["request_id"]["description"],
            json!("Receipt to recover. UUID.")
        );
        assert!(out["properties"]["action"].is_object());
    }

    #[test]
    fn recursive_refs_terminate() {
        let out = portable(json!({
            "type": "object",
            "properties": {"node": {"$ref": "#/$defs/Node"}},
            "$defs": {"Node": {"type": "object", "properties": {"next": {"$ref": "#/$defs/Node"}}}}
        }));
        assert_eq!(
            out["properties"]["node"]["properties"]["next"]["type"],
            json!("object")
        );
    }

    #[test]
    fn mixed_enums_become_string_enums() {
        let out = portable(json!({
            "type": "object",
            "properties": {"level": {"enum": [1, "high"]}, "flag": {"type": "boolean", "enum": [true]}}
        }));
        assert_eq!(
            out["properties"]["level"],
            json!({"type": "string", "enum": ["1", "high"]})
        );
        assert_eq!(out["properties"]["flag"], json!({"type": "boolean"}));
    }

    #[test]
    fn non_object_roots_become_empty_argument_objects() {
        for schema in [
            json!(true),
            json!({}),
            json!({"type": "string"}),
            json!(null),
        ] {
            assert_eq!(portable(schema), empty_object_schema());
        }
    }

    #[test]
    fn violations_report_non_portable_input() {
        let violations = portable_schema_violations(&json!({
            "type": "object",
            "properties": {
                "id": {"type": "string", "format": "uuid"},
                "u": {"type": ["string", "null"]},
                "x": {"description": "untyped"},
                "list": {"type": "array"}
            }
        }));
        let joined = violations.join("\n");
        assert!(
            joined.contains("/properties/id: keyword `format`"),
            "{joined}"
        );
        assert!(
            joined.contains("/properties/u: `type` must be one portable string"),
            "{joined}"
        );
        assert!(
            joined.contains("/properties/x: node has neither"),
            "{joined}"
        );
        assert!(
            joined.contains("/properties/list: array without `items`"),
            "{joined}"
        );
    }
}
