//! The Slipstream catalog as ContextStream answers it (`GET /tasks/agents?workspace_id=`):
//! every agent with the workspace's decision about it, its price per run and its input.
//!
//! Parsing is tolerant: a field a worker from before prices or inputs synced leaves out is
//! simply absent, and an entry without an id is skipped.

use serde_json::{Map, Value};
use uuid::Uuid;

/// The catalog for one workspace.
#[derive(Debug, Clone, PartialEq, Default)]
pub(crate) struct Catalog {
    pub agents: Vec<AgentEntry>,
}

/// One agent and whether an owner or admin turned it on in the workspace.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct AgentEntry {
    pub id: String,
    pub version: String,
    pub name: String,
    pub description: String,
    /// Credits one successful run costs; `None` for an agent metered per model call.
    pub price_credits: Option<i64>,
    pub timeout_minutes: Option<i64>,
    pub input_schema: Option<Value>,
    pub examples: Vec<Example>,
    pub enabled: bool,
}

/// One worked example of an agent's input.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Example {
    pub input: Value,
    pub note: String,
}

/// One top-level field of an agent's input.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct InputField {
    pub name: String,
    /// `string`, `integer`, `string[]`, `"a" | "b"`, ...
    pub kind: String,
    pub required: bool,
    pub description: Option<String>,
}

impl Catalog {
    /// Parse the API's answer, with or without its `{"success", "data"}` envelope.
    pub fn from_value(value: &Value) -> Self {
        let value = value
            .get("data")
            .filter(|_| value.get("success").is_some())
            .unwrap_or(value);
        let agents = value
            .get("agents")
            .and_then(Value::as_array)
            .map(|entries| entries.iter().filter_map(AgentEntry::from_value).collect())
            .unwrap_or_default();
        Self { agents }
    }

    /// The agents an owner or admin turned on, by id.
    pub fn enabled(&self) -> Vec<&AgentEntry> {
        let mut enabled: Vec<&AgentEntry> = self.agents.iter().filter(|a| a.enabled).collect();
        enabled.sort_by(|a, b| a.id.cmp(&b.id));
        enabled
    }

    pub fn has_enabled(&self) -> bool {
        self.agents.iter().any(|agent| agent.enabled)
    }

    /// The entry for `agent`, given as `id` or `id@version`.
    pub fn find(&self, agent: &str) -> Option<&AgentEntry> {
        let id = agent_id_of(agent);
        self.agents.iter().find(|entry| entry.id == id)
    }
}

/// The id part of `id` or `id@version`.
pub(crate) fn agent_id_of(agent: &str) -> &str {
    agent.split('@').next().unwrap_or(agent).trim()
}

impl AgentEntry {
    fn from_value(entry: &Value) -> Option<Self> {
        // The consent view nests the agent; a bare agent object is accepted too.
        let agent = entry
            .get("agent")
            .filter(|a| a.is_object())
            .unwrap_or(entry);
        let id = agent.get("id").and_then(Value::as_str)?.trim().to_string();
        if id.is_empty() {
            return None;
        }
        let text = |key: &str| {
            agent
                .get(key)
                .and_then(Value::as_str)
                .map(str::trim)
                .unwrap_or_default()
                .to_string()
        };
        let name = Some(text("name"))
            .filter(|name| !name.is_empty())
            .unwrap_or_else(|| id.clone());
        let examples = agent
            .get("examples")
            .and_then(Value::as_array)
            .map(|examples| {
                examples
                    .iter()
                    .filter_map(|example| {
                        Some(Example {
                            input: example.get("input").filter(|i| i.is_object())?.clone(),
                            note: example
                                .get("note")
                                .and_then(Value::as_str)
                                .unwrap_or_default()
                                .trim()
                                .to_string(),
                        })
                    })
                    .collect()
            })
            .unwrap_or_default();
        Some(Self {
            version: text("version"),
            name,
            description: text("description"),
            price_credits: agent.get("price_credits").and_then(Value::as_i64),
            timeout_minutes: agent.get("timeout_minutes").and_then(Value::as_i64),
            input_schema: agent
                .get("input_schema")
                .filter(|schema| schema.is_object())
                .cloned(),
            examples,
            enabled: entry.get("enabled").and_then(Value::as_bool) == Some(true),
            id,
        })
    }

    /// `id@version`, or the id when the version is unknown.
    pub fn agent_ref(&self) -> String {
        if self.version.is_empty() {
            self.id.clone()
        } else {
            format!("{}@{}", self.id, self.version)
        }
    }

    /// A price above zero: the run needs the person's accepted quote.
    pub fn quoted_price(&self) -> Option<i64> {
        self.price_credits.filter(|price| *price > 0)
    }

    /// What a run costs, for people: `400 credits`, `free`, `metered`.
    pub fn price_label(&self) -> String {
        match self.price_credits {
            Some(price) if price > 0 => format!("{price} credits"),
            Some(_) => "free".to_string(),
            None => "metered per model call".to_string(),
        }
    }

    /// The first sentence of the description: what the agent does.
    pub fn job(&self) -> String {
        let description = self.description.trim();
        let end = description
            .char_indices()
            .find(|(index, c)| {
                *c == '.'
                    && description[index + 1..]
                        .chars()
                        .next()
                        .is_none_or(char::is_whitespace)
            })
            .map(|(index, _)| index)
            .unwrap_or(description.len());
        description[..end].trim().to_string()
    }

    /// The input's top-level fields, required ones first, then by name.
    pub fn input_fields(&self) -> Vec<InputField> {
        let Some(schema) = self.input_schema.as_ref() else {
            return Vec::new();
        };
        let Some(properties) = schema.get("properties").and_then(Value::as_object) else {
            return Vec::new();
        };
        let required = required_names(schema);
        let mut fields: Vec<InputField> = properties
            .iter()
            .map(|(name, property)| InputField {
                name: name.clone(),
                kind: kind_label(property),
                required: required.iter().any(|r| r == name),
                description: property
                    .get("description")
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|d| !d.is_empty())
                    .map(str::to_string),
            })
            .collect();
        fields.sort_by(|a, b| {
            b.required
                .cmp(&a.required)
                .then_with(|| a.name.cmp(&b.name))
        });
        fields
    }

    /// Whether the agent's input schema names `project_id` as a required field: the tool
    /// fills it from the session when the caller leaves it out.
    pub fn requires_project_id(&self) -> bool {
        self.input_schema
            .as_ref()
            .is_some_and(|schema| required_names(schema).iter().any(|r| r == "project_id"))
    }
}

fn required_names(schema: &Value) -> Vec<String> {
    schema
        .get("required")
        .and_then(Value::as_array)
        .map(|names| {
            names
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

/// A property's type as one short label.
fn kind_label(property: &Value) -> String {
    if let Some(values) = property.get("enum").and_then(Value::as_array) {
        return values
            .iter()
            .map(Value::to_string)
            .collect::<Vec<_>>()
            .join(" | ");
    }
    match property.get("type").and_then(Value::as_str) {
        Some("array") => {
            let items = property.get("items").unwrap_or(&Value::Null);
            if let Some(values) = items.get("enum").and_then(Value::as_array) {
                let values: Vec<String> = values.iter().map(Value::to_string).collect();
                format!("({})[]", values.join(" | "))
            } else {
                format!(
                    "{}[]",
                    items.get("type").and_then(Value::as_str).unwrap_or("value")
                )
            }
        }
        Some(kind) => kind.to_string(),
        None => "value".to_string(),
    }
}

/// Fill what the session knows and check the input against the agent's schema before
/// anything is sent, so a wrong field is caught in the call instead of in a declined run.
///
/// - A required `project_id` the caller left out is the session's project.
/// - A string that spells an integer, number or boolean is taken as one.
/// - A missing required field, a field the agent does not take (when its schema is closed),
///   a value of the wrong type or outside an enum is refused with the agent's input listed.
///
/// An agent with no input schema (synced before inputs were) is passed through unchanged.
pub(crate) fn prepare_input(
    entry: &AgentEntry,
    input: Map<String, Value>,
    session_project_id: Option<Uuid>,
) -> std::result::Result<PreparedInput, String> {
    let mut input = input;
    let mut notes = Vec::new();
    let Some(schema) = entry.input_schema.as_ref() else {
        return Ok(PreparedInput {
            input: Value::Object(input),
            notes,
        });
    };
    let properties = schema
        .get("properties")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();

    if entry.requires_project_id() && !input.contains_key("project_id") {
        if let Some(project_id) = session_project_id {
            input.insert(
                "project_id".to_string(),
                Value::String(project_id.to_string()),
            );
            notes.push(format!(
                "project_id is this session's project ({project_id})."
            ));
        }
    }

    let mut problems = Vec::new();
    let missing: Vec<String> = required_names(schema)
        .into_iter()
        .filter(|name| input.get(name).is_none_or(Value::is_null))
        .collect();
    if !missing.is_empty() {
        problems.push(format!("missing {}", missing.join(", ")));
    }
    if schema.get("additionalProperties") == Some(&Value::Bool(false)) {
        let mut unknown: Vec<&String> = input
            .keys()
            .filter(|key| !properties.contains_key(*key))
            .collect();
        unknown.sort();
        if !unknown.is_empty() {
            problems.push(format!(
                "{} does not take {}",
                entry.id,
                unknown
                    .iter()
                    .map(|k| k.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }
    }
    for (name, property) in &properties {
        let Some(value) = input.get_mut(name) else {
            continue;
        };
        if value.is_null() {
            continue;
        }
        if let Some(problem) = check_value(name, property, value) {
            problems.push(problem);
        }
    }
    if problems.is_empty() {
        return Ok(PreparedInput {
            input: Value::Object(input),
            notes,
        });
    }

    let fields = entry
        .input_fields()
        .iter()
        .map(|field| {
            if field.required {
                format!("{} ({}, required)", field.name, field.kind)
            } else {
                format!("{} ({})", field.name, field.kind)
            }
        })
        .collect::<Vec<_>>()
        .join(", ");
    let mut message = format!(
        "{} input: {}. {} takes: {}.",
        entry.id,
        problems.join("; "),
        entry.id,
        if fields.is_empty() {
            "no fields"
        } else {
            fields.as_str()
        }
    );
    if let Some(example) = entry.examples.first() {
        message.push_str(&format!(" Example input: {}", example.input));
    }
    Err(message)
}

/// The prepared input and what the tool filled in.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct PreparedInput {
    pub input: Value,
    pub notes: Vec<String>,
}

/// Check (and, for a string spelling a scalar, coerce) one value; a problem as text.
fn check_value(name: &str, property: &Value, value: &mut Value) -> Option<String> {
    if let Some(allowed) = property.get("enum").and_then(Value::as_array) {
        return (!allowed.contains(value)).then(|| {
            format!(
                "{name} is one of {}",
                allowed
                    .iter()
                    .map(Value::to_string)
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        });
    }
    let expected = property.get("type").and_then(Value::as_str)?;
    if let Value::String(text) = &*value {
        let trimmed = text.trim();
        let coerced = match expected {
            "integer" => trimmed.parse::<i64>().ok().map(Value::from),
            "number" => trimmed
                .parse::<f64>()
                .ok()
                .and_then(serde_json::Number::from_f64)
                .map(Value::Number),
            "boolean" => match trimmed {
                "true" => Some(Value::Bool(true)),
                "false" => Some(Value::Bool(false)),
                _ => None,
            },
            _ => None,
        };
        if let Some(coerced) = coerced {
            *value = coerced;
        }
    }
    let matches = match expected {
        "string" => value.is_string(),
        "integer" => {
            value.is_i64()
                || value.is_u64()
                || value
                    .as_f64()
                    .is_some_and(|n| n.fract() == 0.0 && n.is_finite())
        }
        "number" => value.is_number(),
        "boolean" => value.is_boolean(),
        "array" => value.is_array(),
        "object" => value.is_object(),
        _ => true,
    };
    (!matches).then(|| format!("{name} must be {}", kind_label(property)))
}
