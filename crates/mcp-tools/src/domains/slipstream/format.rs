//! Text answers for language models. Many clients drop structured content, so the text
//! carries everything: the summary first, the task id on every line, then the cost, and a
//! follow-up call when the run is still working.

use serde_json::Value;

use super::catalog::{AgentEntry, Catalog};
use super::describe::shorten;
use super::{DEFAULT_RUN_WAIT_MS, MAX_WAIT_MS, TOOL_NAME};

/// The most characters of a result's data shown in text; the structured content has all
/// of it.
pub(crate) const MAX_RESULT_CHARS: usize = 40_000;

/// A run that stopped at its price.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Quote {
    pub agent: String,
    pub name: String,
    pub price: i64,
    /// The person accepted a lower price than the agent now costs.
    pub raised_from: Option<i64>,
    /// The caller's auto-accept cap, when one is set and this price is above it.
    pub auto_accept_max_credits: u32,
}

impl Quote {
    pub fn text(&self, input: &Value) -> String {
        let Self {
            agent, name, price, ..
        } = self;
        let lead = match self.raised_from {
            Some(accepted) => format!(
                "Not run: {agent} ({name}) now costs {price} credits a run, more than the {accepted} accepted."
            ),
            None => format!("Not run yet: {agent} ({name}) costs {price} credits a run."),
        };
        let cap = if self.auto_accept_max_credits > 0 {
            format!(
                " It is above the person's auto-accept cap of {} credits.",
                self.auto_accept_max_credits
            )
        } else {
            String::new()
        };
        format!(
            "{lead}{cap} It is charged only if the run succeeds; a declined or failed run is free.\n\
             Show the person this price. If they accept, call {TOOL_NAME}(action=\"run\", agent=\"{agent}\", \
             input={input}, accepted_price_credits={price}).\n{}",
            super::describe::auto_accept_hint()
        )
    }
}

fn task_id(task: &Value) -> &str {
    task.get("id").and_then(Value::as_str).unwrap_or("?")
}

fn is_done(task: &Value) -> bool {
    task.get("done").and_then(Value::as_bool) == Some(true)
}

/// One task: summary, status, decline or error, cost, the follow-up, then the result.
pub(crate) fn task_text(task: &Value, catalog: Option<&Catalog>, notes: &[String]) -> String {
    let id = task_id(task);
    let tag = format!("[task {id}]");
    let agent = task.get("agent").and_then(Value::as_str).unwrap_or("?");
    let status = task.get("status").and_then(Value::as_str).unwrap_or("?");
    let done = is_done(task);
    let output = task.get("output").filter(|output| !output.is_null());
    let mut lines = Vec::new();

    if let Some(summary) = output
        .and_then(|o| o.get("summary"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        lines.push(format!("Summary: {} {tag}", one_line(summary)));
    }

    let mut status_line = format!("Status: {status} ({agent}");
    if !done {
        if let Some(stage) = task.get("stage").and_then(Value::as_str) {
            status_line.push_str(&format!(", at {stage}"));
        }
    }
    status_line.push_str(&format!(") {tag}"));
    lines.push(status_line);

    if let Some(declined) = output
        .and_then(|o| o.get("declined"))
        .filter(|d| !d.is_null())
    {
        if let Some(reason) = declined.get("reason").and_then(Value::as_str) {
            lines.push(format!("Declined: {} {tag}", one_line(reason)));
        }
        if let Some(suggestion) = declined
            .get("suggested_agent")
            .and_then(Value::as_str)
            .and_then(|suggested| suggested_agent(catalog, suggested))
        {
            lines.push(format!("{suggestion} {tag}"));
        }
    }
    if let Some(error) = task.get("error").and_then(Value::as_str) {
        lines.push(format!("Error: {} {tag}", one_line(error)));
    }

    let charged = task.get("charged_credits").and_then(Value::as_i64);
    let price = task.get("price_credits").and_then(Value::as_i64);
    let succeeded = status == "succeeded";
    let cost = match (charged, price) {
        (Some(charged), _) if charged > 0 => format!("Cost: charged {charged} credits"),
        (Some(_), Some(price)) if price > 0 && succeeded => {
            format!("Cost: charged 0 credits (price {price}; this account is not billed for runs)")
        }
        (None, Some(price)) if price > 0 && succeeded => {
            format!("Cost: {price} credits for this successful run")
        }
        (_, Some(price)) if price > 0 && done => {
            format!("Cost: not charged (the {price}-credit price applies only to a successful run)")
        }
        (_, Some(price)) if price > 0 => {
            format!("Cost: {price} credits if it succeeds, nothing otherwise")
        }
        (_, Some(_)) => "Cost: free".to_string(),
        (_, None) if done && !succeeded => "Cost: not charged".to_string(),
        (_, None) => "Cost: metered per model call".to_string(),
    };
    lines.push(format!("{cost} {tag}"));

    for note in notes {
        lines.push(format!("Note: {note} {tag}"));
    }

    if !done {
        lines.push(format!(
            "Next: still working. Call {TOOL_NAME}(action=\"get_run\", task_id=\"{id}\", wait_ms={MAX_WAIT_MS}) to wait up to 25 s more, as often as needed {tag}"
        ));
    }

    let mut text = lines.join("\n");
    if let Some(data) = output.and_then(|o| o.get("data")).filter(|d| !d.is_null()) {
        text.push_str(&format!("\n\nResult {tag}:\n{}", render_data(data)));
    }
    text
}

/// A suggested agent, only when it is in this workspace's catalog.
fn suggested_agent(catalog: Option<&Catalog>, suggested: &str) -> Option<String> {
    let entry = catalog?.find(suggested)?;
    let turned_on = if entry.enabled {
        String::new()
    } else {
        " (not turned on in this workspace: an owner or admin turns it on)".to_string()
    };
    Some(format!(
        "Try instead: {} ({}, {}){turned_on}",
        entry.id,
        entry.name,
        entry.price_label()
    ))
}

fn one_line(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// A result's data for reading: long or multi-line strings as text blocks, everything else
/// as compact JSON, cut at [`MAX_RESULT_CHARS`].
pub(crate) fn render_data(data: &Value) -> String {
    let rendered = match data {
        Value::String(text) => text.clone(),
        Value::Object(map) => {
            let mut parts = Vec::new();
            for (key, value) in map {
                match value {
                    Value::String(text) if text.contains('\n') || text.chars().count() > 200 => {
                        parts.push(format!("{key}:\n{}", text.trim_end()));
                    }
                    other => parts.push(format!("{key}: {other}")),
                }
            }
            parts.join("\n")
        }
        other => other.to_string(),
    };
    if rendered.chars().count() > MAX_RESULT_CHARS {
        let cut: String = rendered.chars().take(MAX_RESULT_CHARS).collect();
        format!(
            "{cut}\n… (cut at {MAX_RESULT_CHARS} characters; the structured result has all of it)"
        )
    } else {
        rendered
    }
}

/// `find`: every agent, turned-on ones first, with its job, price and input.
pub(crate) fn catalog_text(catalog: &Catalog, auto_accept_max_credits: u32) -> String {
    if catalog.agents.is_empty() {
        return "No agents are available in this workspace yet.".to_string();
    }
    let mut agents: Vec<&AgentEntry> = catalog.agents.iter().collect();
    agents.sort_by(|a, b| b.enabled.cmp(&a.enabled).then_with(|| a.id.cmp(&b.id)));
    let enabled = agents.iter().filter(|a| a.enabled).count();
    let mut out = format!(
        "{} agent(s), {enabled} turned on here. Each run is charged its price only when it succeeds.\n",
        agents.len()
    );
    if auto_accept_max_credits > 0 {
        out.push_str(&format!(
            "Runs priced at most {auto_accept_max_credits} credits start without a quote (the person's auto-accept setting).\n"
        ));
    }
    for agent in agents {
        let state = if agent.enabled {
            "on"
        } else {
            "off: an owner or admin turns it on"
        };
        out.push_str(&format!(
            "\n**{}** ({}) — {}; {state}\n",
            agent.name,
            agent.agent_ref(),
            agent.price_label()
        ));
        if !agent.description.is_empty() {
            out.push_str(&format!("  {}\n", agent.description));
        }
        for field in agent.input_fields() {
            let required = if field.required { ", required" } else { "" };
            let auto = if field.required && field.name == "project_id" {
                "; defaults to the session's project"
            } else {
                ""
            };
            let about = field
                .description
                .as_deref()
                .map(|d| format!(": {}", shorten(d, 160)))
                .unwrap_or_default();
            out.push_str(&format!(
                "  - {} ({}{required}{auto}){about}\n",
                field.name, field.kind
            ));
        }
        if let Some(example) = agent.examples.first() {
            let note = if example.note.is_empty() {
                String::new()
            } else {
                format!(" — {}", example.note)
            };
            out.push_str(&format!("  Example input: {}{note}\n", example.input));
        }
    }
    out.push_str(&format!(
        "\nRun one: {TOOL_NAME}(action=\"run\", agent=\"<id>\", input={{...}}). A priced agent answers with its price first; run again with accepted_price_credits once the person accepts. It waits up to {} s for the result.",
        DEFAULT_RUN_WAIT_MS / 1000
    ));
    out
}

/// `list_runs`: one line per task, newest first, each with its task id.
pub(crate) fn task_list_text(list: &Value) -> String {
    let tasks = list
        .get("tasks")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    if tasks.is_empty() {
        return "No agent runs in this workspace yet.".to_string();
    }
    let mut out = format!("{} run(s), newest first:\n", tasks.len());
    for task in &tasks {
        let id = task_id(task);
        let agent = task.get("agent").and_then(Value::as_str).unwrap_or("?");
        let status = task.get("status").and_then(Value::as_str).unwrap_or("?");
        let created = task.get("created_at").and_then(Value::as_str).unwrap_or("");
        let cost = match task.get("charged_credits").and_then(Value::as_i64) {
            Some(charged) if charged > 0 => format!(", charged {charged} credits"),
            _ => String::new(),
        };
        out.push_str(&format!(
            "- {agent}: {status}{cost}, asked {created} [task {id}]\n"
        ));
    }
    out.push_str(&format!(
        "Read one with {TOOL_NAME}(action=\"get_run\", task_id=\"...\")."
    ));
    out
}
