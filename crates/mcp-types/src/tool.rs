//! Tool-related types for the MCP server.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::sync::OnceLock;

/// Tool result returned from tool handlers.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolResult {
    /// Content items (text, images, etc.)
    pub content: Vec<ContentItem>,

    /// Structured content for programmatic access
    #[serde(
        rename = "structuredContent",
        alias = "structured_content",
        alias = "structured",
        skip_serializing_if = "Option::is_none"
    )]
    pub structured_content: Option<Value>,

    /// Whether this result is an error
    #[serde(default)]
    pub is_error: bool,
}

impl ToolResult {
    /// Create a successful text result.
    pub fn text(text: impl Into<String>) -> Self {
        Self {
            content: vec![ContentItem::text(text)],
            structured_content: None,
            is_error: false,
        }
    }

    /// Create a successful result with structured content.
    pub fn with_structured(text: impl Into<String>, data: impl Serialize) -> Self {
        Self {
            content: vec![ContentItem::text(text)],
            structured_content: if structured_content_enabled() {
                as_structured_object(data)
            } else {
                None
            },
            is_error: false,
        }
    }

    /// Create an error result.
    pub fn error(message: impl Into<String>) -> Self {
        Self {
            content: vec![ContentItem::text(message)],
            structured_content: None,
            is_error: true,
        }
    }

    /// Create an error result with a code.
    pub fn error_with_code(code: &str, message: impl Into<String>) -> Self {
        let message = message.into();
        let text = format!("[{}] {}", code, message);
        Self {
            content: vec![ContentItem::text(text)],
            structured_content: if structured_content_enabled() {
                Some(serde_json::json!({
                    "success": false,
                    "error": {
                        "code": code,
                        "message": message
                    }
                }))
            } else {
                None
            },
            is_error: true,
        }
    }

    /// Create an error result for credit exhaustion with helpful recovery info.
    pub fn credits_exhausted(required: Option<i32>, available: Option<i32>) -> Self {
        let status_line = match (required, available) {
            (Some(req), Some(avail)) => format!(
                "This operation requires {} credits but you have {} available.",
                req, avail
            ),
            (Some(req), None) => format!(
                "This operation requires {} credits but your balance is empty.",
                req
            ),
            _ => "You've run out of credits.".to_string(),
        };

        let text = format!(
            "[CREDITS_EXHAUSTED] {}\n\n\
             To continue using ContextStream:\n  \
             - Purchase credits or upgrade your plan: https://contextstream.io/pricing\n  \
             - Manage your billing: https://contextstream.io/account/billing\n\n\
             Recommended upgrade paths:\n  \
             - Starter/Free -> Pro: 25,000 operations/month\n  \
             - Starter/Free -> Elite: 100,000 operations/month plus full graph\n  \
             - Pro -> Elite: 4x Pro operations plus full graph\n\n\
             Your existing data (memory, decisions, indexed projects) is safe and will be \
             available when you add more credits.",
            status_line
        );

        Self {
            content: vec![ContentItem::text(text)],
            structured_content: if structured_content_enabled() {
                Some(serde_json::json!({
                    "success": false,
                    "error": {
                        "code": "INSUFFICIENT_CREDITS",
                        "required": required,
                        "available": available,
                        "pricing_url": "https://contextstream.io/pricing",
                        "billing_url": "https://contextstream.io/account/billing",
                        "upgrade_options": [
                            {
                                "from_plans": ["starter", "free"],
                                "target_plan": "pro",
                                "label": "Upgrade to Pro",
                                "monthly_operations": 25000,
                                "billing_url": "https://contextstream.io/account/billing"
                            },
                            {
                                "from_plans": ["starter", "free"],
                                "target_plan": "elite",
                                "label": "Upgrade to Elite",
                                "monthly_operations": 100000,
                                "billing_url": "https://contextstream.io/account/billing",
                                "benefits": ["Full graph", "Enhanced context"]
                            },
                            {
                                "from_plans": ["pro"],
                                "target_plan": "elite",
                                "label": "Upgrade Pro to Elite",
                                "monthly_operations": 100000,
                                "billing_url": "https://contextstream.io/account/billing",
                                "benefits": ["4x Pro operations", "Full graph"]
                            }
                        ],
                        "message": status_line
                    }
                }))
            } else {
                None
            },
            is_error: true,
        }
    }

    /// Create an error result for plan-gated features with upgrade guidance.
    pub fn plan_restricted(
        feature: impl Into<String>,
        current_plan: Option<&str>,
        required_tier: &str,
        fallback_available: bool,
    ) -> Self {
        let feature = feature.into();
        let current_plan = current_plan.unwrap_or("unknown");
        let required_label = display_plan_label(required_tier);
        let fallback_line = if fallback_available {
            "\n\nA lower-tier fallback was used when possible. Upgrade for the richer result."
        } else {
            ""
        };
        let text = format!(
            "[PLAN_RESTRICTED] {} requires {}. Current plan: {}.{}\n\n\
             Upgrade or manage billing: https://contextstream.io/account/billing\n\
             Compare plans: https://contextstream.io/pricing",
            feature,
            required_label,
            display_plan_label(current_plan),
            fallback_line
        );

        Self {
            content: vec![ContentItem::text(text)],
            structured_content: if structured_content_enabled() {
                Some(serde_json::json!({
                    "success": false,
                    "error": {
                        "code": "PLAN_RESTRICTED",
                        "feature": feature,
                        "current_plan": current_plan,
                        "required_tier": required_tier,
                        "required_label": required_label,
                        "fallback_available": fallback_available,
                        "pricing_url": "https://contextstream.io/pricing",
                        "billing_url": "https://contextstream.io/account/billing",
                        "upgrade_options": upgrade_options_for_restriction(current_plan, required_tier)
                    }
                }))
            } else {
                None
            },
            is_error: true,
        }
    }

    /// Add a prefix to the text content.
    pub fn with_prefix(mut self, prefix: impl AsRef<str>) -> Self {
        if let Some(ContentItem::Text { text }) = self.content.first_mut() {
            *text = format!("{}{}", prefix.as_ref(), text);
        }
        self
    }
}

fn display_plan_label(plan: &str) -> &'static str {
    match plan.to_ascii_lowercase().as_str() {
        "free" => "Free",
        "starter" => "Starter",
        "pro" | "lite" => "Pro",
        "elite" | "full" | "semantic" => "Elite",
        "team" => "Team",
        "enterprise" => "Enterprise",
        _ => "a paid plan",
    }
}

fn upgrade_options_for_restriction(current_plan: &str, required_tier: &str) -> Value {
    let required = match required_tier.to_ascii_lowercase().as_str() {
        "team" | "enterprise" => "team",
        "elite" | "full" | "semantic" => "elite",
        _ => "pro",
    };
    let options = match (current_plan.to_ascii_lowercase().as_str(), required) {
        ("pro", "elite") => vec![serde_json::json!({
            "from_plan": "pro",
            "target_plan": "elite",
            "label": "Upgrade to Elite",
            "billing_url": "https://contextstream.io/account/billing",
            "benefits": ["Full graph", "Enhanced context", "100,000 operations/month"]
        })],
        ("pro", "team") | ("elite", "team") => vec![serde_json::json!({
            "from_plan": current_plan,
            "target_plan": "team",
            "label": "Upgrade to Team",
            "billing_url": "https://contextstream.io/account/billing",
            "benefits": ["Shared team context", "Admin roles", "Pooled operations"]
        })],
        _ => vec![
            serde_json::json!({
                "from_plan": current_plan,
                "target_plan": "pro",
                "label": "Upgrade to Pro",
                "billing_url": "https://contextstream.io/account/billing",
                "benefits": ["25,000 operations/month", "Integrations", "Graph-Lite"]
            }),
            serde_json::json!({
                "from_plan": current_plan,
                "target_plan": required,
                "label": format!("Upgrade to {}", display_plan_label(required)),
                "billing_url": "https://contextstream.io/account/billing"
            }),
        ],
    };
    Value::Array(options)
}

/// MCP `structuredContent` must be a JSON object.
///
/// Cursor and other JS MCP clients reject arrays with
/// `expected record, received array`. List endpoints that unwrap
/// `ApiResponse.data` into a bare array have to be coerced here.
pub fn as_structured_object(data: impl Serialize) -> Option<Value> {
    let value = serde_json::to_value(data).ok()?;
    Some(match value {
        Value::Null => serde_json::json!({}),
        Value::Object(_) => value,
        Value::Array(items) => serde_json::json!({ "items": items }),
        other => serde_json::json!({ "value": other }),
    })
}

/// Whether MCP responses should include structured content payloads.
///
/// Set `CONTEXTSTREAM_INCLUDE_STRUCTURED_CONTENT=false` to suppress structured
/// payloads and return text-only results (helpful for clients that print full
/// JSON payloads inline).
pub fn structured_content_enabled() -> bool {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| {
        std::env::var("CONTEXTSTREAM_INCLUDE_STRUCTURED_CONTENT")
            .map(|v| match v.trim().to_ascii_lowercase().as_str() {
                "0" | "false" | "no" | "off" => false,
                "1" | "true" | "yes" | "on" => true,
                _ => true,
            })
            .unwrap_or(true)
    })
}

/// Content item in a tool result.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum ContentItem {
    /// Text content
    Text { text: String },
    /// Image content
    Image { data: String, mime_type: String },
    /// Resource reference
    Resource {
        uri: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        mime_type: Option<String>,
    },
}

impl ContentItem {
    /// Create a text content item.
    pub fn text(text: impl Into<String>) -> Self {
        Self::Text { text: text.into() }
    }

    /// Create an image content item.
    pub fn image(data: impl Into<String>, mime_type: impl Into<String>) -> Self {
        Self::Image {
            data: data.into(),
            mime_type: mime_type.into(),
        }
    }

    /// Create a resource reference.
    pub fn resource(uri: impl Into<String>) -> Self {
        Self::Resource {
            uri: uri.into(),
            mime_type: None,
        }
    }
}

/// Tool annotations for hints about tool behavior.
///
/// These map to the standard MCP tool annotations in `tools/list`, plus
/// ContextStream-only execution hints. Defaults intentionally follow MCP's
/// pessimistic posture: an unspecified tool may write, may be destructive,
/// is not safe to retry, and may interact with the open world.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ToolAnnotations {
    /// Tool is read-only (doesn't modify state)
    #[serde(default)]
    pub read_only: bool,

    /// Tool performs destructive operations
    #[serde(default = "default_true")]
    pub destructive: bool,

    /// Tool requires confirmation before execution
    #[serde(default = "default_true")]
    pub requires_confirmation: bool,

    /// Tool is idempotent (safe to retry)
    #[serde(default)]
    pub idempotent: bool,

    /// Tool may take a long time to execute
    #[serde(default)]
    pub long_running: bool,

    /// Tool may interact with external entities or untrusted content.
    #[serde(default = "default_true")]
    pub open_world: bool,
}

const fn default_true() -> bool {
    true
}

impl Default for ToolAnnotations {
    fn default() -> Self {
        Self {
            read_only: false,
            destructive: true,
            requires_confirmation: true,
            idempotent: false,
            long_running: false,
            open_world: true,
        }
    }
}

impl ToolAnnotations {
    /// Create annotations for a read-only tool.
    pub fn read_only() -> Self {
        Self {
            read_only: true,
            destructive: false,
            requires_confirmation: false,
            idempotent: true,
            long_running: false,
            open_world: true,
        }
    }

    /// Create annotations for an additive, non-destructive write tool.
    pub fn write() -> Self {
        Self {
            read_only: false,
            destructive: false,
            requires_confirmation: true,
            idempotent: false,
            long_running: false,
            open_world: true,
        }
    }

    /// Create annotations for a destructive tool.
    pub fn destructive() -> Self {
        Self {
            read_only: false,
            destructive: true,
            requires_confirmation: true,
            idempotent: false,
            long_running: false,
            open_world: true,
        }
    }

    /// Mark a tool as operating only on a closed, trusted domain.
    pub fn closed_world(mut self) -> Self {
        self.open_world = false;
        self
    }

    /// Mark a tool as safe to retry with the same arguments.
    pub fn idempotent(mut self) -> Self {
        self.idempotent = true;
        self
    }

    /// Mark a tool as potentially long-running.
    pub fn long_running(mut self) -> Self {
        self.long_running = true;
        self
    }
}

/// Tool category for organization.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ToolCategory {
    Session,
    Search,
    Memory,
    Graph,
    Workspace,
    Project,
    Ai,
    Reminders,
    Integrations,
    Utility,
}

impl ToolCategory {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Session => "session",
            Self::Search => "search",
            Self::Memory => "memory",
            Self::Graph => "graph",
            Self::Workspace => "workspace",
            Self::Project => "project",
            Self::Ai => "ai",
            Self::Reminders => "reminders",
            Self::Integrations => "integrations",
            Self::Utility => "utility",
        }
    }
}

/// Metadata about a registered tool.
#[derive(Debug, Clone)]
pub struct ToolMetadata {
    /// Tool name
    pub name: String,

    /// Display title
    pub title: String,

    /// Tool description
    pub description: String,

    /// Tool category
    pub category: ToolCategory,

    /// Tool annotations
    pub annotations: ToolAnnotations,

    /// Whether this is a PRO-only tool
    pub is_pro: bool,

    /// Required plan tier (none, lite, full)
    pub required_tier: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_tool_result_text() {
        let result = ToolResult::text("Hello, world!");
        assert!(!result.is_error);
        assert_eq!(result.content.len(), 1);
    }

    #[test]
    fn with_structured_wraps_arrays_as_objects() {
        let result = ToolResult::with_structured(
            "Found 2 items",
            vec![
                serde_json::json!({"id": "a"}),
                serde_json::json!({"id": "b"}),
            ],
        );
        let structured = result.structured_content.expect("structured payload");
        assert!(structured.is_object());
        assert_eq!(structured["items"].as_array().map(Vec::len), Some(2));
        assert_eq!(structured["items"][0]["id"], "a");
    }

    #[test]
    fn with_structured_leaves_objects_unchanged() {
        let result = ToolResult::with_structured(
            "ok",
            serde_json::json!({"items": [{"id": "a"}], "total": 1}),
        );
        let structured = result.structured_content.expect("structured payload");
        assert_eq!(structured["total"], 1);
        assert_eq!(structured["items"][0]["id"], "a");
    }

    #[test]
    fn test_tool_result_error() {
        let result = ToolResult::error_with_code("BAD_REQUEST", "Invalid input");
        assert!(result.is_error);
        assert!(result.structured_content.is_some());
    }

    #[test]
    fn test_plan_restricted_has_upgrade_payload() {
        let result = ToolResult::plan_restricted("Full graph", Some("pro"), "elite", true);
        assert!(result.is_error);
        let structured = result.structured_content.expect("structured payload");
        assert_eq!(structured["error"]["code"], "PLAN_RESTRICTED");
        assert_eq!(structured["error"]["current_plan"], "pro");
        assert_eq!(structured["error"]["required_tier"], "elite");
        assert_eq!(structured["error"]["fallback_available"], true);
    }

    #[test]
    fn tool_annotation_defaults_are_pessimistic() {
        let annotations = ToolAnnotations::default();
        assert!(!annotations.read_only);
        assert!(annotations.destructive);
        assert!(annotations.requires_confirmation);
        assert!(!annotations.idempotent);
        assert!(annotations.open_world);
    }

    #[test]
    fn read_only_annotations_clear_destructive_defaults() {
        let annotations = ToolAnnotations::read_only().closed_world();
        assert!(annotations.read_only);
        assert!(!annotations.destructive);
        assert!(!annotations.requires_confirmation);
        assert!(annotations.idempotent);
        assert!(!annotations.open_world);
    }

    #[test]
    fn deserializing_missing_annotations_keeps_pessimistic_defaults() {
        let annotations: ToolAnnotations = serde_json::from_value(serde_json::json!({})).unwrap();
        assert!(!annotations.read_only);
        assert!(annotations.destructive);
        assert!(annotations.requires_confirmation);
        assert!(!annotations.idempotent);
        assert!(annotations.open_world);
    }

    #[test]
    fn test_tool_result_prefix() {
        let result = ToolResult::text("world!").with_prefix("Hello, ");
        if let Some(ContentItem::Text { text }) = result.content.first() {
            assert_eq!(text, "Hello, world!");
        } else {
            panic!("Expected text content");
        }
    }
}
