//! The Apps SDK component for ChatGPT, and the structured views it shows.
//!
//! `ui://widget/coach.html` is one self-contained page (HTML, CSS and JS; it
//! fetches nothing). It renders the tool's `structuredContent`: a compact job
//! card, a side-by-side comparison, or a list of workflows. Its suggestion
//! buttons only send a follow-up message that starts a coaching step; they
//! never save feedback or guidance. Hosts without widgets get the same
//! information as text.

use serde::Serialize;
use serde_json::{json, Value};

use super::{WORKFLOW_DELETE_TOOL, WORKFLOW_SAVE_TOOL, WORKFLOW_TOOL};

/// The component's resource URI.
pub const WIDGET_URI: &str = "ui://widget/coach.html";
/// The Apps SDK component MIME type.
pub const WIDGET_MIME: &str = "text/html+skybridge";
const WIDGET_HTML: &str = include_str!("coach_widget.html");

/// The component's HTML.
pub fn widget_html() -> &'static str {
    WIDGET_HTML
}

fn widget_resource_meta() -> Value {
    json!({
        "openai/widgetDescription": "A compact Agent Coach Kit card: the workflow's goal, this job and version, what changed and which feedback led to it, what still needs attention, and one-click coaching suggestions. Or two versions side by side.",
        "openai/widgetPrefersBorder": true,
        "openai/widgetCSP": {
            "connect_domains": [],
            "resource_domains": []
        }
    })
}

/// The entry `resources/list` shows.
pub fn widget_resource() -> Value {
    json!({
        "uri": WIDGET_URI,
        "name": "coach-card",
        "title": "Agent Coach Kit card",
        "description": "Compact job card and side-by-side compare for Agent Coach Kit.",
        "mimeType": WIDGET_MIME,
        "_meta": widget_resource_meta()
    })
}

/// What `resources/read` answers for `uri`, if it is the component.
pub fn read_widget_resource(uri: &str) -> Option<Value> {
    (uri == WIDGET_URI).then(|| {
        json!({
            "contents": [{
                "uri": WIDGET_URI,
                "mimeType": WIDGET_MIME,
                "text": WIDGET_HTML,
                "_meta": widget_resource_meta()
            }]
        })
    })
}

/// The OAuth scopes a coach tool needs.
pub fn security_schemes(tool: &str) -> Option<Value> {
    let scopes = match tool {
        WORKFLOW_TOOL => vec![mcp_client::coach::READ_SCOPE],
        WORKFLOW_SAVE_TOOL | WORKFLOW_DELETE_TOOL => vec![mcp_client::coach::WRITE_SCOPE],
        _ => return None,
    };
    Some(json!([{ "type": "oauth2", "scopes": scopes }]))
}

/// Extra descriptor fields for a coach tool in `tools/list`: the security
/// schemes at the top level, and `_meta` keys merged into the tool's `_meta`.
pub fn descriptor_extras(tool: &str) -> Option<(Value, Value)> {
    let schemes = security_schemes(tool)?;
    let mut meta = json!({
        "securitySchemes": schemes.clone(),
        "openai/widgetAccessible": false,
    });
    let (invoking, invoked) = match tool {
        WORKFLOW_TOOL => ("Reading your coaching workflow…", "Read"),
        WORKFLOW_SAVE_TOOL => ("Saving to your coaching workflow…", "Saved"),
        _ => ("Deleting the workflow…", "Deleted"),
    };
    meta["openai/toolInvocation/invoking"] = json!(invoking);
    meta["openai/toolInvocation/invoked"] = json!(invoked);
    if tool != WORKFLOW_DELETE_TOOL {
        meta["openai/outputTemplate"] = json!(WIDGET_URI);
    }
    if tool == WORKFLOW_SAVE_TOOL {
        meta["openai/fileParams"] = json!(["file"]);
    }
    Some((schemes, meta))
}

/// `_meta` for a result the component renders.
pub fn result_meta() -> Value {
    json!({ "openai/outputTemplate": WIDGET_URI })
}

// ============================================================================
// Structured views (no internal ids)
// ============================================================================

#[derive(Debug, Clone, Default, Serialize)]
pub(crate) struct CardWorkflow {
    #[serde(rename = "ref")]
    pub reference: String,
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub goal_version: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub goal: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub job_count: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize)]
pub(crate) struct CardJob {
    #[serde(rename = "ref")]
    pub reference: String,
    pub objective: String,
    pub status: String,
}

#[derive(Debug, Clone, Default, Serialize)]
pub(crate) struct CardVersion {
    #[serde(rename = "ref")]
    pub reference: String,
    pub kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub preview: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub media: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize)]
pub(crate) struct CardChange {
    pub how: String,
    #[serde(rename = "ref")]
    pub reference: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize)]
pub(crate) struct CardFeedback {
    #[serde(rename = "ref")]
    pub reference: String,
    pub kind: String,
    pub scope: String,
    pub you_said: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reading: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub guidance_ref: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize)]
pub(crate) struct CardItem {
    #[serde(rename = "ref")]
    pub reference: String,
    pub text: String,
}

#[derive(Debug, Clone, Default, Serialize)]
pub(crate) struct CardEffort {
    pub effort: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub said: Option<String>,
    pub for_this_job: bool,
}

#[derive(Debug, Clone, Default, Serialize)]
pub(crate) struct CompareSide {
    #[serde(rename = "ref")]
    pub reference: String,
    pub job_ref: String,
    pub objective: String,
    pub kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub media: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub carry_note: Option<String>,
    pub changed: Vec<CardChange>,
    pub feedback: Vec<CardFeedback>,
}

#[derive(Debug, Clone, Default, Serialize)]
pub(crate) struct CardCompare {
    pub left: CompareSide,
    pub right: CompareSide,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct Suggestion {
    pub label: &'static str,
    pub prompt: String,
}

/// The structured content of every coach reply.
#[derive(Debug, Clone, Default, Serialize)]
pub(crate) struct Card {
    /// `job_card`, `workflow`, `compare`, `list` or `status`.
    pub view: &'static str,
    /// `saved`, `already_saved`, `pending`, `failed`, `conflict` or `read`.
    pub state: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status_line: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub operation_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub workflow: Option<CardWorkflow>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub job: Option<CardJob>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version: Option<CardVersion>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub carry_note: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub changed: Vec<CardChange>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub feedback: Option<CardFeedback>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub attention: Vec<CardItem>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub effort: Option<CardEffort>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub compare: Option<CardCompare>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub workflows: Vec<CardWorkflow>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub suggestions: Vec<Suggestion>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

impl Card {
    pub(crate) fn new(view: &'static str, state: &'static str) -> Self {
        Self {
            view,
            state,
            ..Self::default()
        }
    }

    pub(crate) fn to_value(&self) -> Value {
        serde_json::to_value(self).unwrap_or_else(|_| json!({ "view": self.view }))
    }
}

/// One-click coaching steps. Each sends a message that starts the step in the
/// chat; none saves anything by itself.
pub(crate) fn suggestions(job: Option<&str>, version: Option<&str>) -> Vec<Suggestion> {
    let Some(version) = version else {
        return Vec::new();
    };
    let mut items = Vec::new();
    if let Some(job) = job {
        items.push(Suggestion {
            label: "Compare two versions",
            prompt: format!("Compare the last two versions of {job} side by side."),
        });
    }
    items.push(Suggestion {
        label: "Help me explain what's missing",
        prompt: format!(
            "Help me explain what's missing in {version}: ask me one useful question, or show me two short options to choose from."
        ),
    });
    items.push(Suggestion {
        label: "Quick revision",
        prompt: format!("Quick revision of {version}: apply my feedback and save the new version, without more questions this time."),
    });
    items.push(Suggestion {
        label: "Explore alternatives",
        prompt: format!("Explore two different alternatives to {version} before we revise it."),
    });
    items
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_widget_is_self_contained_and_accessible() {
        let html = widget_html();
        for external in [
            "http://",
            "https://",
            "fetch(",
            "XMLHttpRequest",
            "import(",
            "<link",
            "src=",
        ] {
            assert!(
                !html.contains(external),
                "the widget must not load {external}"
            );
        }
        assert!(html.contains("sendFollowUpMessage"));
        assert!(html.contains(":focus-visible"));
        assert!(html.contains("aria-label"));
        assert!(html.contains("prefers-color-scheme: dark"));
        assert!(html.contains("max-width: 560px"));
        // Data is placed with textContent, never parsed as HTML.
        assert!(!html.contains("innerHTML"));
        assert!(
            !html.contains("callTool"),
            "the widget never saves anything itself"
        );
    }

    #[test]
    fn the_resource_reads_back_with_the_skybridge_type() {
        let listed = widget_resource();
        assert_eq!(listed["uri"], WIDGET_URI);
        assert_eq!(listed["mimeType"], WIDGET_MIME);
        let read = read_widget_resource(WIDGET_URI).expect("served");
        assert_eq!(read["contents"][0]["mimeType"], "text/html+skybridge");
        assert_eq!(read["contents"][0]["text"], widget_html());
        assert_eq!(
            read["contents"][0]["_meta"]["openai/widgetCSP"]["connect_domains"],
            json!([])
        );
        assert!(read_widget_resource("ui://widget/other.html").is_none());
    }

    #[test]
    fn descriptors_name_the_template_scopes_and_file_input() {
        let (schemes, meta) = descriptor_extras(WORKFLOW_SAVE_TOOL).unwrap();
        assert_eq!(schemes[0]["scopes"], json!(["write:coach"]));
        assert_eq!(meta["openai/outputTemplate"], WIDGET_URI);
        assert_eq!(meta["openai/fileParams"], json!(["file"]));
        let (schemes, meta) = descriptor_extras(WORKFLOW_TOOL).unwrap();
        assert_eq!(schemes[0]["scopes"], json!(["read:coach"]));
        assert!(meta.get("openai/fileParams").is_none());
        let (_, meta) = descriptor_extras(WORKFLOW_DELETE_TOOL).unwrap();
        assert!(meta.get("openai/outputTemplate").is_none());
        assert!(descriptor_extras("memory").is_none());
    }

    #[test]
    fn suggestions_start_steps_and_name_the_version() {
        let items = suggestions(Some("weekly-note#4"), Some("weekly-note#4/v2"));
        let labels: Vec<_> = items.iter().map(|s| s.label).collect();
        assert_eq!(
            labels,
            [
                "Compare two versions",
                "Help me explain what's missing",
                "Quick revision",
                "Explore alternatives"
            ]
        );
        assert!(items.iter().all(|s| s.prompt.contains("weekly-note#4")));
        assert!(suggestions(Some("weekly-note#4"), None).is_empty());
    }
}
