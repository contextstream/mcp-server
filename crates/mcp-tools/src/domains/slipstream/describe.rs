//! The `slipstream` tool as a client sees it, written from the workspace's enabled catalog.
//!
//! - The tool description names each enabled agent: id, price, required input and its job,
//!   then how to run one. It stays within [`DESCRIPTION_BUDGET`] characters (OpenAI-compatible
//!   endpoints refuse function descriptions over 1024): jobs are shortened word by word, then
//!   dropped, then the input lists, and only then are agents left to `find`.
//! - The `input` parameter carries every agent's fields with their types and one example,
//!   within [`INPUT_REFERENCE_BUDGET`].
//! - The `agent` parameter lists the ids.

use serde_json::Value;

use super::catalog::{AgentEntry, Catalog};
use super::{AUTO_ACCEPT_ENV, TOOL_NAME};

/// The most characters the generated tool description may have.
pub(crate) const DESCRIPTION_BUDGET: usize = 1000;
/// The most characters the `input` parameter's per-agent reference may have.
pub(crate) const INPUT_REFERENCE_BUDGET: usize = 3000;

/// The description when no catalog is known (router and discovery surfaces).
pub(crate) const STATIC_DESCRIPTION: &str = "Slipstream specialist agents: hand one job (review a pull request, write release notes, read an image, make a PDF, ...) to a ContextStream agent that works with this workspace's context, and read its result. Actions: find (the agents turned on here, their price and input), run (ask an agent; a priced agent quotes first: show the person the price, then run again with accepted_price_credits), get_run (a task and its result, waiting up to 25 s), list_runs, cancel_run. Charged only when a run succeeds; declined or failed runs are free.";

const STATIC_INPUT: &str = "The agent's input object (run). find lists each agent's fields; a required project_id defaults to the session's project.";
const STATIC_AGENT: &str =
    "Agent id, or id@version (run; a filter for list_runs). find lists them.";

/// Everything the listing writes from one catalog.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Described {
    pub description: String,
    pub agent_parameter: String,
    pub input_parameter: String,
}

pub(crate) fn describe(catalog: &Catalog, auto_accept_max_credits: u32) -> Described {
    let agents = catalog.enabled();
    Described {
        description: tool_description(&agents, auto_accept_max_credits),
        agent_parameter: agent_parameter(&agents),
        input_parameter: input_parameter(&agents),
    }
}

/// Put the generated text into a copy of the tool's input schema.
pub(crate) fn schema_with(schema: &Value, described: &Described) -> Value {
    let mut schema = schema.clone();
    if let Some(properties) = schema.get_mut("properties").and_then(Value::as_object_mut) {
        for (name, text) in [
            ("agent", &described.agent_parameter),
            ("input", &described.input_parameter),
        ] {
            if let Some(property) = properties.get_mut(name).and_then(Value::as_object_mut) {
                property.insert("description".to_string(), Value::String(text.clone()));
            }
        }
    }
    schema
}

fn header() -> &'static str {
    "Slipstream agents turned on here. Each does one job with this workspace's context and returns the result. Credits per successful run, free if declined or failed:"
}

fn footer(auto_accept_max_credits: u32) -> String {
    let accept = if auto_accept_max_credits > 0 {
        format!(
            "A run priced at most {auto_accept_max_credits} credits starts at once (the person's auto-accept cap); a pricier one first returns its price: show it, then run again with accepted_price_credits."
        )
    } else {
        "It first returns the price: show it to the person, then run again with accepted_price_credits.".to_string()
    };
    format!(
        "Run: {TOOL_NAME}(action=\"run\", agent, input). {accept} An unfinished run returns a task_id for get_run (waits up to 25 s). Fields: see input. Also: find, list_runs, cancel_run."
    )
}

/// `- id (price) [required, fields]: job`, with the job cut to at most `job_chars`
/// (`None`: the whole first sentence; `Some(0)`: no job) and the fields left out when
/// `with_fields` is false.
fn agent_line(agent: &AgentEntry, job_chars: Option<usize>, with_fields: bool) -> String {
    let price = match agent.price_credits {
        Some(price) if price > 0 => price.to_string(),
        Some(_) => "free".to_string(),
        None => "metered".to_string(),
    };
    let mut line = format!("- {} ({price})", agent.id);
    if with_fields {
        let required: Vec<String> = agent
            .input_fields()
            .into_iter()
            .filter(|field| field.required && field.name != "project_id")
            .map(|field| field.name)
            .collect();
        if !required.is_empty() {
            line.push_str(&format!(" [{}]", required.join(", ")));
        }
    }
    let job = agent.job();
    let job = match job_chars {
        None => job,
        Some(limit) => shorten(&job, limit),
    };
    if !job.is_empty() {
        line.push_str(": ");
        line.push_str(&job);
    }
    line
}

/// Cut `text` to at most `limit` characters at a word boundary, ending with `…`.
pub(crate) fn shorten(text: &str, limit: usize) -> String {
    if text.chars().count() <= limit {
        return text.to_string();
    }
    if limit < 8 {
        return String::new();
    }
    let cut: String = text.chars().take(limit - 1).collect();
    let cut = match cut.rfind(' ') {
        Some(space) if space > limit / 2 => &cut[..space],
        _ => cut.as_str(),
    };
    format!(
        "{}…",
        cut.trim_end_matches(|c: char| c == ',' || c == ';' || c.is_whitespace())
    )
}

fn joined(lines: &[String]) -> usize {
    lines.iter().map(|line| line.chars().count() + 1).sum()
}

fn tool_description(agents: &[&AgentEntry], auto_accept_max_credits: u32) -> String {
    let header = header();
    let footer = footer(auto_accept_max_credits);
    let fixed = header.chars().count() + footer.chars().count() + 2;
    let room = DESCRIPTION_BUDGET.saturating_sub(fixed);

    let lines = fit_lines(agents, room);
    let mut description = String::with_capacity(DESCRIPTION_BUDGET);
    description.push_str(header);
    description.push('\n');
    for line in &lines {
        description.push_str(line);
        description.push('\n');
    }
    description.push_str(&footer);
    if description.chars().count() > DESCRIPTION_BUDGET {
        // Only reachable with a header and footer that do not fit on their own.
        description = shorten(&description, DESCRIPTION_BUDGET);
    }
    description
}

/// The shortest job worth keeping; below it the line keeps only id, price and fields.
const MIN_JOB_CHARS: usize = 16;

/// The agent lines that fit in `room` characters, losing detail before losing agents.
fn fit_lines(agents: &[&AgentEntry], room: usize) -> Vec<String> {
    let lines_with = |job_chars: Option<usize>, with_fields: bool| -> Vec<String> {
        agents
            .iter()
            .map(|agent| agent_line(agent, job_chars, with_fields))
            .collect()
    };
    let full = lines_with(None, true);
    if joined(&full) <= room {
        return full;
    }
    // The longest job every agent can keep.
    let longest_job = agents
        .iter()
        .map(|agent| agent.job().chars().count())
        .max()
        .unwrap_or(0);
    let (mut low, mut high) = (MIN_JOB_CHARS, longest_job);
    let mut best = None;
    while low <= high {
        let mid = (low + high) / 2;
        let lines = lines_with(Some(mid), true);
        if joined(&lines) <= room {
            best = Some(lines);
            low = mid + 1;
        } else {
            high = mid - 1;
        }
    }
    if let Some(lines) = best {
        return lines;
    }
    for with_fields in [true, false] {
        let lines = lines_with(Some(0), with_fields);
        if joined(&lines) <= room {
            return lines;
        }
    }
    // Still too many: as many agents as fit, then a pointer to find. Each step keeps room
    // for the pointer line that would follow it.
    let more = |count: usize| format!("- and {count} more: {TOOL_NAME}(action=\"find\")");
    let mut lines = Vec::new();
    for (index, agent) in agents.iter().enumerate() {
        let line = agent_line(agent, Some(0), false);
        let left_after = agents.len() - index - 1;
        let tail = if left_after > 0 {
            more(left_after).chars().count() + 1
        } else {
            0
        };
        if joined(&lines) + line.chars().count() + 1 + tail > room {
            lines.push(more(agents.len() - index));
            return lines;
        }
        lines.push(line);
    }
    lines
}

fn agent_parameter(agents: &[&AgentEntry]) -> String {
    if agents.is_empty() {
        return STATIC_AGENT.to_string();
    }
    let ids: Vec<&str> = agents.iter().map(|agent| agent.id.as_str()).collect();
    format!(
        "Agent id, or id@version (run; a filter for list_runs). Turned on here: {}.",
        ids.join(", ")
    )
}

/// One agent's input: `- review: repo* (string), pull* (integer), focus (string). e.g. {...}`.
fn input_line(agent: &AgentEntry, with_optional: bool, with_example: bool) -> String {
    let fields = agent.input_fields();
    let mut line = format!("- {}:", agent.id);
    if agent.input_schema.is_none() {
        line.push_str(" any object (see find)");
        return line;
    }
    let listed: Vec<String> = fields
        .iter()
        .filter(|field| field.required || with_optional)
        .map(|field| {
            let star = if field.required { "*" } else { "" };
            let auto = if field.required && field.name == "project_id" {
                ", defaults to the session's"
            } else {
                ""
            };
            format!("{}{star} ({}{auto})", field.name, field.kind)
        })
        .collect();
    if listed.is_empty() {
        line.push_str(if fields.is_empty() {
            " no fields, send {}"
        } else {
            " nothing required"
        });
    } else {
        line.push(' ');
        line.push_str(&listed.join(", "));
        if !with_optional && fields.iter().any(|field| !field.required) {
            line.push_str(", ...");
        }
    }
    if with_example {
        if let Some(example) = agent.examples.first() {
            // A required project_id is filled from the session: an example's placeholder id
            // would only be copied.
            let mut input = example.input.clone();
            if agent.requires_project_id() {
                if let Some(map) = input.as_object_mut() {
                    map.remove("project_id");
                }
            }
            line.push_str(&format!(". e.g. {input}"));
        }
    }
    line
}

fn input_parameter(agents: &[&AgentEntry]) -> String {
    if agents.is_empty() {
        return STATIC_INPUT.to_string();
    }
    let header = "The agent's input object (run). Its fields per agent (* required); find explains each field:";
    let room = INPUT_REFERENCE_BUDGET.saturating_sub(header.chars().count() + 1);
    for (with_optional, with_example) in [(true, true), (true, false), (false, false)] {
        let lines: Vec<String> = agents
            .iter()
            .map(|agent| input_line(agent, with_optional, with_example))
            .collect();
        if joined(&lines) <= room {
            return format!("{header}\n{}", lines.join("\n"));
        }
    }
    let mut out = header.to_string();
    for agent in agents {
        let line = input_line(agent, false, false);
        if out.chars().count() + line.chars().count() + 40 > INPUT_REFERENCE_BUDGET {
            out.push_str(&format!("\n- the rest: {TOOL_NAME}(action=\"find\")"));
            break;
        }
        out.push('\n');
        out.push_str(&line);
    }
    out
}

/// Where the auto-accept setting is described to a person.
pub(crate) fn auto_accept_hint() -> String {
    format!(
        "To let runs up to a price start without this step, the person sets {AUTO_ACCEPT_ENV} (a hosted connection sends the header X-ContextStream-Slipstream-Auto-Accept-Max-Credits)."
    )
}
