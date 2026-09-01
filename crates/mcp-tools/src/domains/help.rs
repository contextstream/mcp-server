//! Help & utility domain tools: tools, auth, billing, version, workflow,
//! editor_rules preview, enable_bundle metadata, team_status.

use async_trait::async_trait;
use mcp_client::{ContextStreamClient, EditorRulesParams};
use mcp_types::{
    build_harness_teaching,
    harness::HarnessId,
    harness_teaching::HarnessTeachingDelivery,
    tool::{ToolAnnotations, ToolCategory, ToolMetadata, ToolResult},
    Error, Result,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::{BTreeMap, HashSet};
use std::sync::Arc;
use uuid::Uuid;

use crate::registry::ToolHandler;
use crate::schema::SchemaBuilder;

// ============================================================================
// Valid Constants
// ============================================================================

/// Valid actions.
const VALID_ACTIONS: &[&str] = &[
    "tools",
    "auth",
    "billing",
    "version",
    "workflow",
    "editor_rules",
    "enable_bundle",
    "team_status",
];

/// Valid tool categories.
const VALID_CATEGORIES: &[&str] = &[
    "session",
    "search",
    "memory",
    "graph",
    "workspace",
    "project",
    "reminders",
    "integrations",
    "utility",
];

/// Valid output formats.
const VALID_FORMATS: &[&str] = &["grouped", "minimal", "full"];
const PUBLIC_MCP_RELEASE_MANIFEST_URL: &str =
    "https://pub-68429b9f7857416c9484b75bf1887b96.r2.dev/mcp/latest/version.json";

/// Valid rule modes.
const VALID_MODES: &[&str] = &["minimal", "full", "bootstrap"];

/// Valid bundles.
const VALID_BUNDLES: &[&str] = &[
    "session",
    "memory",
    "search",
    "graph",
    "workspace",
    "project",
    "reminders",
    "integrations",
];

/// Valid editors.
#[allow(dead_code)]
const VALID_EDITORS: &[&str] = &[
    "codex",
    "opencode",
    "cursor",
    "windsurf",
    "cline",
    "kilo",
    "roo",
    "claude",
    "aider",
    "antigravity",
    "all",
];

// ============================================================================
// Unified Help Tool
// ============================================================================

/// Input for the unified help tool.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HelpInput {
    pub action: String,
    // Tools action fields
    pub category: Option<String>,
    pub format: Option<String>,
    // Workflow action fields
    pub client_name: Option<String>,
    // Editor rules fields
    pub editors: Option<Vec<String>>,
    pub mode: Option<String>,
    pub folder_path: Option<String>,
    pub project_name: Option<String>,
    pub workspace_name: Option<String>,
    pub workspace_id: Option<String>,
    pub additional_rules: Option<String>,
    pub dry_run: Option<bool>,
    pub install_hooks: Option<bool>,
    pub include_pre_compact: Option<bool>,
    pub include_post_write: Option<bool>,
    // Enable bundle fields
    pub bundle: Option<String>,
    pub list_bundles: Option<bool>,
}

/// Unified help tool handler.
pub struct HelpTool {
    client: ContextStreamClient,
}

impl HelpTool {
    pub fn new(client: ContextStreamClient) -> Self {
        Self { client }
    }

    fn parse_workspace_id(input: &Option<String>) -> Result<Option<Uuid>> {
        match input {
            Some(s) => {
                Ok(Some(Uuid::parse_str(s).map_err(|_| {
                    Error::Validation("Invalid workspace_id".to_string())
                })?))
            }
            None => Ok(None),
        }
    }
}

const BILLING_URL: &str = "https://contextstream.io/account/billing";
const PRICING_URL: &str = "https://contextstream.io/pricing";

fn extract_email(result: &Value) -> &str {
    result
        .get("email")
        .or_else(|| result.get("user").and_then(|user| user.get("email")))
        .and_then(|v| v.as_str())
        .unwrap_or("unknown")
}

fn extract_plan_name(result: &Value) -> String {
    result
        .get("plan_name")
        .or_else(|| result.get("plan"))
        .or_else(|| result.get("user").and_then(|user| user.get("plan_name")))
        .or_else(|| {
            result
                .get("subscription")
                .and_then(|subscription| subscription.get("plan_name"))
        })
        .and_then(|v| v.as_str())
        .map(|plan| plan.trim().to_ascii_lowercase())
        .filter(|plan| !plan.is_empty())
        .unwrap_or_else(|| "free".to_string())
}

fn display_plan_name(plan: &str) -> String {
    match plan {
        "starter" => "Starter".to_string(),
        "free" => "Free".to_string(),
        "pro" => "Pro".to_string(),
        "elite" => "Elite".to_string(),
        "team" => "Team".to_string(),
        "enterprise" => "Enterprise".to_string(),
        other => {
            let mut chars = other.chars();
            match chars.next() {
                Some(first) => format!("{}{}", first.to_ascii_uppercase(), chars.as_str()),
                None => "Free".to_string(),
            }
        }
    }
}

fn upgrade_options_for_plan(plan: &str) -> Vec<Value> {
    match plan {
        "free" | "starter" => vec![
            json!({
                "from_plan": plan,
                "target_plan": "pro",
                "label": "Upgrade to Pro",
                "monthly_operations": 25000,
                "billing_url": BILLING_URL,
                "benefits": ["25,000 operations/month", "Unlimited projects", "Basic graph analysis"]
            }),
            json!({
                "from_plan": plan,
                "target_plan": "elite",
                "label": "Upgrade to Elite",
                "monthly_operations": 100000,
                "billing_url": BILLING_URL,
                "benefits": ["100,000 operations/month", "Full graph", "Enhanced context"]
            }),
        ],
        "pro" => vec![json!({
            "from_plan": "pro",
            "target_plan": "elite",
            "label": "Upgrade to Elite",
            "monthly_operations": 100000,
            "billing_url": BILLING_URL,
            "benefits": ["4x Pro operations", "Full graph", "Enhanced context"]
        })],
        _ => Vec::new(),
    }
}

fn upgrade_summary(options: &[Value]) -> String {
    let labels: Vec<&str> = options
        .iter()
        .filter_map(|option| option.get("label").and_then(|label| label.as_str()))
        .collect();

    if labels.is_empty() {
        "No self-serve upgrade path is available from this plan.".to_string()
    } else {
        format!("Upgrade options: {}.", labels.join(", "))
    }
}

fn add_billing_guidance(result: &mut Value) -> (String, Vec<Value>) {
    let plan = extract_plan_name(result);
    let options = upgrade_options_for_plan(&plan);

    if let Some(obj) = result.as_object_mut() {
        obj.insert("current_plan".to_string(), Value::String(plan.clone()));
        obj.insert(
            "billing_url".to_string(),
            Value::String(BILLING_URL.to_string()),
        );
        obj.insert(
            "pricing_url".to_string(),
            Value::String(PRICING_URL.to_string()),
        );
        obj.insert("upgrade_options".to_string(), Value::Array(options.clone()));
    }

    (plan, options)
}

fn value_string_list(value: Option<&Value>) -> Vec<String> {
    value
        .and_then(Value::as_array)
        .map(|values| {
            values
                .iter()
                .filter_map(Value::as_str)
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(ToString::to_string)
                .collect()
        })
        .unwrap_or_default()
}

fn render_tool_catalog(result: &Value, format: &str) -> String {
    let tools = result
        .get("tools")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();

    if tools.is_empty() {
        return "No tools matched the requested category.".to_string();
    }

    if format == "minimal" {
        let names = tools
            .iter()
            .filter_map(|tool| tool.get("name").and_then(Value::as_str))
            .collect::<Vec<_>>();
        return format!("Available tools ({}): {}", names.len(), names.join(", "));
    }

    if format == "full" {
        let mut output = format!("Available tools ({}):\n", tools.len());
        for tool in &tools {
            let name = tool
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or("unknown");
            let category = tool
                .get("category")
                .and_then(Value::as_str)
                .unwrap_or("other");
            let description = tool
                .get("description")
                .and_then(Value::as_str)
                .unwrap_or("No description provided.");
            output.push_str(&format!("\n{name} [{category}]\n  {description}\n"));

            let actions = value_string_list(tool.get("actions"));
            if !actions.is_empty() {
                output.push_str(&format!("  Actions: {}\n", actions.join(", ")));
            }
            let parameters = value_string_list(tool.get("key_parameters"));
            if !parameters.is_empty() {
                output.push_str(&format!("  Key parameters: {}\n", parameters.join(", ")));
            }
            if let Some(example) = tool.get("example").and_then(Value::as_str) {
                output.push_str(&format!("  Example: {example}\n"));
            }
        }
        return output.trim_end().to_string();
    }

    let mut groups: BTreeMap<&str, Vec<&Value>> = BTreeMap::new();
    for tool in &tools {
        let category = tool
            .get("category")
            .and_then(Value::as_str)
            .unwrap_or("other");
        groups.entry(category).or_default().push(tool);
    }

    let mut output = format!("Available tools ({}):", tools.len());
    for (category, entries) in groups {
        output.push_str(&format!("\n\n{category}:\n"));
        for tool in entries {
            let name = tool
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or("unknown");
            let description = tool
                .get("description")
                .and_then(Value::as_str)
                .unwrap_or("No description provided.");
            output.push_str(&format!("- {name}: {description}\n"));
        }
    }
    output.trim_end().to_string()
}

fn collect_release_note_strings(value: &Value, output: &mut Vec<String>) {
    match value {
        Value::String(value) => {
            let value = value.trim();
            if !value.is_empty() {
                output.push(value.to_string());
            }
        }
        Value::Array(values) => {
            for value in values {
                collect_release_note_strings(value, output);
            }
        }
        Value::Object(values) => {
            for value in values.values() {
                collect_release_note_strings(value, output);
            }
        }
        _ => {}
    }
}

fn release_note_lines(result: &Value) -> Vec<String> {
    let mut lines = Vec::new();
    for key in ["changelog", "release_notes", "notes"] {
        if let Some(value) = result.get(key) {
            collect_release_note_strings(value, &mut lines);
        }
    }

    let mut seen = HashSet::new();
    lines.retain(|line| seen.insert(line.clone()));
    lines
}

fn render_version_info(result: &mut Value) -> String {
    let version = result
        .get("version")
        .and_then(Value::as_str)
        .unwrap_or("unknown")
        .to_string();
    let latest_version = result
        .get("latest_version")
        .and_then(Value::as_str)
        .map(ToString::to_string)
        .unwrap_or_else(|| version.clone());
    let release_url = result
        .get("release_url")
        .and_then(Value::as_str)
        .unwrap_or(PUBLIC_MCP_RELEASE_MANIFEST_URL)
        .to_string();
    let release_notes = release_note_lines(result);

    if let Some(metadata) = result.as_object_mut() {
        metadata.insert(
            "runtime_type".to_string(),
            Value::String("rust-mcp".to_string()),
        );
        metadata.insert(
            "release_metadata_format".to_string(),
            Value::String("json".to_string()),
        );
        metadata.insert(
            "release_notes_available".to_string(),
            Value::Bool(!release_notes.is_empty()),
        );
    }

    let mut output = format!(
        "Runtime: Rust MCP (hosted HTTP and installed Rust binary release line).\nServer version: {version}. Latest available: {latest_version}.\nMachine-readable release metadata: {release_url}"
    );
    if release_notes.is_empty() {
        output.push_str("\nRelease notes: not published in the release metadata for this build.");
    } else {
        output.push_str("\nRelease notes:");
        for line in release_notes {
            output.push_str(&format!("\n- {line}"));
        }
    }
    output
}

#[async_trait]
impl ToolHandler for HelpTool {
    async fn execute(&self, input: Value) -> Result<ToolResult> {
        let input: HelpInput =
            serde_json::from_value(input).map_err(|e| Error::Validation(e.to_string()))?;

        let action = input.action.to_lowercase();

        match action.as_str() {
            "tools" => {
                let result = self
                    .client
                    .help_tools(input.category.as_deref(), input.format.as_deref())
                    .await?;
                let format = input.format.as_deref().unwrap_or("grouped");
                Ok(ToolResult::with_structured(
                    render_tool_catalog(&result, format),
                    result,
                ))
            }

            "auth" => {
                let mut result = self.client.help_auth().await?;
                let email = extract_email(&result).to_string();
                let (plan, upgrade_options) = add_billing_guidance(&mut result);
                Ok(ToolResult::with_structured(
                    format!(
                        "Authenticated as: {} (plan: {}). {}",
                        email,
                        display_plan_name(&plan),
                        upgrade_summary(&upgrade_options)
                    ),
                    result,
                ))
            }

            "billing" => {
                let mut result = self.client.help_auth().await?;
                let (plan, upgrade_options) = add_billing_guidance(&mut result);
                Ok(ToolResult::with_structured(
                    format!(
                        "Billing plan: {}. {}",
                        display_plan_name(&plan),
                        upgrade_summary(&upgrade_options)
                    ),
                    result,
                ))
            }

            "version" => {
                let mut result = self.client.help_version().await?;
                let text = render_version_info(&mut result);
                Ok(ToolResult::with_structured(text, result))
            }

            "workflow" => {
                let harness_id = input
                    .client_name
                    .as_deref()
                    .and_then(HarnessId::from_client_hint);
                let contract =
                    build_harness_teaching(harness_id, HarnessTeachingDelivery::HelpWorkflow);
                let summary = if let Some(harness_id) = harness_id {
                    format!(
                        "ContextStream workflow {} for {}.",
                        contract.teaching_version,
                        harness_id.display_name()
                    )
                } else {
                    format!(
                        "ContextStream workflow {} with conservative generic client guidance.",
                        contract.teaching_version
                    )
                };
                let structured = serde_json::to_value(&contract)?;
                Ok(ToolResult::with_structured(
                    format!("{summary}\n\n{}", contract.rendered_guidance),
                    structured,
                ))
            }

            "editor_rules" => {
                let workspace_id = Self::parse_workspace_id(&input.workspace_id)?;
                let params = EditorRulesParams {
                    editors: input.editors,
                    mode: input.mode,
                    folder_path: input.folder_path,
                    project_name: input.project_name,
                    workspace_name: input.workspace_name,
                    workspace_id,
                    additional_rules: input.additional_rules,
                    dry_run: input.dry_run,
                    install_hooks: input.install_hooks,
                    include_pre_compact: input.include_pre_compact,
                    include_post_write: input.include_post_write,
                };
                let result = self.client.help_editor_rules(params).await?;

                Ok(ToolResult::with_structured(
                    "Editor rules preview generated; no files or hooks were changed.".to_string(),
                    result,
                ))
            }

            "enable_bundle" => {
                // Check if listing bundles
                if input.list_bundles.unwrap_or(false) {
                    let bundles = serde_json::json!({
                        "bundles": VALID_BUNDLES,
                        "description": "Available tool bundles for progressive disclosure mode"
                    });
                    return Ok(ToolResult::with_structured(
                        "Available bundles listed.".to_string(),
                        bundles,
                    ));
                }

                let bundle = input.bundle.ok_or_else(|| {
                    Error::Validation("bundle is required for enable_bundle".to_string())
                })?;

                if !VALID_BUNDLES.contains(&bundle.as_str()) {
                    return Err(Error::Validation(format!(
                        "Invalid bundle: {}. Valid bundles: {}",
                        bundle,
                        VALID_BUNDLES.join(", ")
                    )));
                }

                let result = self.client.help_enable_bundle(&bundle).await?;
                Ok(ToolResult::with_structured(
                    format!(
                        "Bundle '{}' previewed; the live tool registry was not changed.",
                        bundle
                    ),
                    result,
                ))
            }

            "team_status" => {
                let workspace_id = Self::parse_workspace_id(&input.workspace_id)?;
                let result = self.client.help_team_status(workspace_id).await?;
                let plan = result
                    .get("plan")
                    .and_then(|v| v.as_str())
                    .unwrap_or("unknown");
                Ok(ToolResult::with_structured(
                    format!("Team plan: {}", plan),
                    result,
                ))
            }

            _ => Err(Error::Validation(format!(
                "Unknown action: {}. Available actions: {}",
                action,
                VALID_ACTIONS.join(", ")
            ))),
        }
    }

    fn metadata(&self) -> &ToolMetadata {
        static METADATA: std::sync::OnceLock<ToolMetadata> = std::sync::OnceLock::new();
        METADATA.get_or_init(|| ToolMetadata {
            name: "help".to_string(),
            title: "Help & Utility".to_string(),
            description: "Read-only utility and help. Actions: tools (list available tools), auth (current user and plan), billing (plan upgrade options), version (server version), workflow (versioned ContextStream harness workflow; optional exact client_name), editor_rules (preview an editor-rules request; never writes files or installs hooks), enable_bundle (preview bundle metadata), team_status (team subscription info).".to_string(),
            category: ToolCategory::Utility,
            annotations: ToolAnnotations::read_only(),
            is_pro: false,
            required_tier: None,
        })
    }

    fn input_schema(&self) -> Value {
        SchemaBuilder::new()
            .description("Help and utility operations")
            .string_enum("action", "Action to perform", VALID_ACTIONS, true)
            // Tools action fields
            .string_enum(
                "category",
                "Filter tools by category",
                VALID_CATEGORIES,
                false,
            )
            .string_enum("format", "Output format", VALID_FORMATS, false)
            // Workflow action fields
            .string(
                "client_name",
                "Exact MCP client or harness name for workflow syntax/capabilities; unknown values receive conservative generic guidance",
                false,
            )
            // Editor rules fields
            .array(
                "editors",
                "Which editors to generate rules for",
                "string",
                false,
            )
            .string_enum("mode", "Rule verbosity mode", VALID_MODES, false)
            .string("folder_path", "Absolute path to project folder", false)
            .string("project_name", "Project name to include in rules", false)
            .string(
                "workspace_name",
                "Workspace name to include in rules",
                false,
            )
            .uuid("workspace_id", "Workspace ID", false)
            .string(
                "additional_rules",
                "Additional project-specific rules",
                false,
            )
            .boolean(
                "dry_run",
                "Compatibility flag; editor_rules is always a no-write preview",
                false,
            )
            .boolean(
                "install_hooks",
                "Preview requested hook settings only; no hooks are installed",
                false,
            )
            .boolean("include_pre_compact", "Include PreCompact hook", false)
            .boolean(
                "include_post_write",
                "Include PostToolUse hook for file indexing",
                false,
            )
            // Enable bundle fields
            .string_enum("bundle", "Bundle to enable", VALID_BUNDLES, false)
            .boolean("list_bundles", "List available bundles", false)
            .build()
    }
}

/// Register all help tools.
pub fn register_help_tools(
    registry: &mut crate::registry::ToolRegistry,
    client: ContextStreamClient,
) {
    registry.register("help", Arc::new(HelpTool::new(client)));
}

#[cfg(test)]
#[path = "help_tests.rs"]
mod tests;
