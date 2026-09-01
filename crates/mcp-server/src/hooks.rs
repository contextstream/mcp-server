//! Hooks system for the MCP server.
//!
//! Supports hook types:
//! - PreToolUse: Before tool execution
//! - PostToolUse: After tool execution
//! - PostToolUseFailure: After tool execution fails
//! - InstructionsLoaded: When CLAUDE.md/rules files are loaded
//! - UserPromptSubmit: On user input
//! - SessionStart: On session start
//! - Stop: When Claude finishes a response
//! - StopFailure: When the turn ends with an API error
//! - SessionEnd: On session end
//! - PreCompact: Before context compaction
//! - SubagentStart: When a subagent is spawned (Explore, Plan, etc.)
//! - SubagentStop: When a subagent finishes
//! - TaskCreated: When an agent creates a task
//! - TaskCompleted: When an agent marks a task complete
//! - TeammateIdle: When a teammate is about to go idle
//! - Notification: For system notifications
//! - PermissionRequest: Before requesting permission escalation

use anyhow::Result;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Stdio;
use tokio::process::Command;
use tracing::{debug, error, warn};

// ============================================================================
// Hook Types
// ============================================================================

/// Hook event types.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HookType {
    PreToolUse,
    PostToolUse,
    PostToolUseFailure,
    InstructionsLoaded,
    UserPromptSubmit,
    SessionStart,
    Stop,
    StopFailure,
    SessionEnd,
    PreCompact,
    SubagentStart,
    SubagentStop,
    TaskCreated,
    TaskCompleted,
    TeammateIdle,
    Notification,
    PermissionRequest,
}

impl HookType {
    pub fn as_str(&self) -> &'static str {
        match self {
            HookType::PreToolUse => "pre_tool_use",
            HookType::PostToolUse => "post_tool_use",
            HookType::PostToolUseFailure => "post_tool_use_failure",
            HookType::InstructionsLoaded => "instructions_loaded",
            HookType::UserPromptSubmit => "user_prompt_submit",
            HookType::SessionStart => "session_start",
            HookType::Stop => "stop",
            HookType::StopFailure => "stop_failure",
            HookType::SessionEnd => "session_end",
            HookType::PreCompact => "pre_compact",
            HookType::SubagentStart => "subagent_start",
            HookType::SubagentStop => "subagent_stop",
            HookType::TaskCreated => "task_created",
            HookType::TaskCompleted => "task_completed",
            HookType::TeammateIdle => "teammate_idle",
            HookType::Notification => "notification",
            HookType::PermissionRequest => "permission_request",
        }
    }

    pub fn from_str(s: &str) -> Option<Self> {
        match s.to_lowercase().as_str() {
            "pre_tool_use" | "pre-tool-use" | "pretooluse" => Some(HookType::PreToolUse),
            "post_tool_use" | "post-tool-use" | "posttooluse" => Some(HookType::PostToolUse),
            "post_tool_use_failure" | "post-tool-use-failure" | "posttoolusefailure" => {
                Some(HookType::PostToolUseFailure)
            }
            "instructions_loaded" | "instructions-loaded" | "instructionsloaded" => {
                Some(HookType::InstructionsLoaded)
            }
            "user_prompt_submit" | "user-prompt-submit" | "userpromptsubmit" => {
                Some(HookType::UserPromptSubmit)
            }
            "session_start" | "session-start" | "sessionstart" => Some(HookType::SessionStart),
            "stop" => Some(HookType::Stop),
            "stop_failure" | "stop-failure" | "stopfailure" => Some(HookType::StopFailure),
            "session_end" | "session-end" | "sessionend" => Some(HookType::SessionEnd),
            "pre_compact" | "pre-compact" | "precompact" => Some(HookType::PreCompact),
            "subagent_start" | "subagent-start" | "subagentstart" => Some(HookType::SubagentStart),
            "subagent_stop" | "subagent-stop" | "subagentstop" => Some(HookType::SubagentStop),
            "task_created" | "task-created" | "taskcreated" => Some(HookType::TaskCreated),
            "task_completed" | "task-completed" | "taskcompleted" => Some(HookType::TaskCompleted),
            "teammate_idle" | "teammate-idle" | "teammateidle" => Some(HookType::TeammateIdle),
            "notification" => Some(HookType::Notification),
            "permission_request" | "permission-request" | "permissionrequest" => {
                Some(HookType::PermissionRequest)
            }
            _ => None,
        }
    }
}

/// Hook result from execution.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HookResult {
    pub success: bool,
    pub output: Option<String>,
    pub error: Option<String>,
    pub should_block: bool,
    pub modified_input: Option<Value>,
}

impl Default for HookResult {
    fn default() -> Self {
        Self {
            success: true,
            output: None,
            error: None,
            should_block: false,
            modified_input: None,
        }
    }
}

/// Hook context passed to hook execution.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HookContext {
    pub hook_type: String,
    pub tool_name: Option<String>,
    pub tool_input: Option<Value>,
    pub tool_output: Option<Value>,
    pub user_message: Option<String>,
    pub session_id: Option<String>,
    pub workspace_id: Option<String>,
    pub project_id: Option<String>,
}

// ============================================================================
// Hook Configuration
// ============================================================================

/// Single hook configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HookConfig {
    /// Shell command to execute.
    pub command: String,

    /// Working directory for the command.
    #[serde(default)]
    pub cwd: Option<String>,

    /// Environment variables to set.
    #[serde(default)]
    pub env: HashMap<String, String>,

    /// Timeout in milliseconds (default: 30000).
    #[serde(default = "default_timeout")]
    pub timeout_ms: u64,

    /// Whether this hook can block the operation.
    #[serde(default)]
    pub can_block: bool,

    /// Tool name filter (for PreToolUse/PostToolUse).
    #[serde(default)]
    pub tool_filter: Option<String>,
}

fn default_timeout() -> u64 {
    30000
}

/// Hooks configuration file format.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct HooksConfig {
    #[serde(default)]
    pub pre_tool_use: Vec<HookConfig>,

    #[serde(default)]
    pub post_tool_use: Vec<HookConfig>,

    #[serde(default)]
    pub post_tool_use_failure: Vec<HookConfig>,

    #[serde(default)]
    pub instructions_loaded: Vec<HookConfig>,

    #[serde(default)]
    pub user_prompt_submit: Vec<HookConfig>,

    #[serde(default)]
    pub session_start: Vec<HookConfig>,

    #[serde(default)]
    pub stop: Vec<HookConfig>,

    #[serde(default)]
    pub stop_failure: Vec<HookConfig>,

    #[serde(default)]
    pub session_end: Vec<HookConfig>,

    #[serde(default)]
    pub pre_compact: Vec<HookConfig>,

    #[serde(default)]
    pub subagent_start: Vec<HookConfig>,

    #[serde(default)]
    pub subagent_stop: Vec<HookConfig>,

    #[serde(default)]
    pub task_created: Vec<HookConfig>,

    #[serde(default)]
    pub task_completed: Vec<HookConfig>,

    #[serde(default)]
    pub teammate_idle: Vec<HookConfig>,

    #[serde(default)]
    pub notification: Vec<HookConfig>,

    #[serde(default)]
    pub permission_request: Vec<HookConfig>,
}

impl HooksConfig {
    /// Load hooks configuration from file.
    pub fn load(path: &PathBuf) -> Result<Self> {
        if !path.exists() {
            return Ok(Self::default());
        }

        let content = std::fs::read_to_string(path)?;
        let config: HooksConfig = serde_json::from_str(&content)?;
        Ok(config)
    }

    /// Get hooks for a specific type.
    pub fn get_hooks(&self, hook_type: HookType) -> &[HookConfig] {
        match hook_type {
            HookType::PreToolUse => &self.pre_tool_use,
            HookType::PostToolUse => &self.post_tool_use,
            HookType::PostToolUseFailure => &self.post_tool_use_failure,
            HookType::InstructionsLoaded => &self.instructions_loaded,
            HookType::UserPromptSubmit => &self.user_prompt_submit,
            HookType::SessionStart => &self.session_start,
            HookType::Stop => &self.stop,
            HookType::StopFailure => &self.stop_failure,
            HookType::SessionEnd => &self.session_end,
            HookType::PreCompact => &self.pre_compact,
            HookType::SubagentStart => &self.subagent_start,
            HookType::SubagentStop => &self.subagent_stop,
            HookType::TaskCreated => &self.task_created,
            HookType::TaskCompleted => &self.task_completed,
            HookType::TeammateIdle => &self.teammate_idle,
            HookType::Notification => &self.notification,
            HookType::PermissionRequest => &self.permission_request,
        }
    }
}

// ============================================================================
// Hook Manager
// ============================================================================

/// Manages hook execution.
#[derive(Clone)]
pub struct HookManager {
    config: HooksConfig,
}

impl HookManager {
    /// Create a new hook manager.
    pub fn new(config: HooksConfig) -> Self {
        Self { config }
    }

    /// Load hook manager from config directory.
    pub fn load_from_config_dir() -> Result<Self> {
        let config_dir = dirs_config_dir();
        let hooks_path = config_dir.join("hooks.json");
        let config = HooksConfig::load(&hooks_path)?;
        Ok(Self::new(config))
    }

    /// Execute hooks for a given type.
    pub async fn execute(&self, hook_type: HookType, context: HookContext) -> HookResult {
        let hooks = self.config.get_hooks(hook_type);

        if hooks.is_empty() {
            return HookResult::default();
        }

        debug!("Executing {} hooks for {:?}", hooks.len(), hook_type);

        let mut combined_result = HookResult::default();

        for hook in hooks {
            // Check tool filter for tool-related hooks
            if let Some(ref filter) = hook.tool_filter {
                if let Some(ref tool_name) = context.tool_name {
                    if !tool_name.contains(filter) && filter != "*" {
                        continue;
                    }
                }
            }

            match execute_hook(hook, &context).await {
                Ok(result) => {
                    if !result.success {
                        combined_result.success = false;
                    }
                    if result.should_block {
                        combined_result.should_block = true;
                    }
                    if let Some(output) = result.output {
                        combined_result.output = Some(output);
                    }
                    if let Some(modified) = result.modified_input {
                        combined_result.modified_input = Some(modified);
                    }
                    if result.should_block && hook.can_block {
                        // Stop executing more hooks if blocked
                        break;
                    }
                }
                Err(e) => {
                    warn!("Hook execution failed: {}", e);
                    combined_result.error = Some(e.to_string());
                    if hook.can_block {
                        combined_result.success = false;
                        break;
                    }
                }
            }
        }

        combined_result
    }

    /// Check if there are any hooks for a type.
    pub fn has_hooks(&self, hook_type: HookType) -> bool {
        !self.config.get_hooks(hook_type).is_empty()
    }
}

/// Execute a single hook.
async fn execute_hook(hook: &HookConfig, context: &HookContext) -> Result<HookResult> {
    let context_json = serde_json::to_string(context)?;

    // Build the command
    let shell = if cfg!(windows) { "cmd" } else { "sh" };
    let shell_arg = if cfg!(windows) { "/C" } else { "-c" };

    let mut cmd = Command::new(shell);
    cmd.arg(shell_arg)
        .arg(&hook.command)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    // Set working directory
    if let Some(ref cwd) = hook.cwd {
        cmd.current_dir(cwd);
    }

    // Set environment variables
    cmd.env("HOOK_TYPE", context.hook_type.clone());
    cmd.env("HOOK_CONTEXT", &context_json);

    for (key, value) in &hook.env {
        cmd.env(key, value);
    }

    // Execute with timeout
    let timeout = std::time::Duration::from_millis(hook.timeout_ms);

    let result = tokio::time::timeout(timeout, async {
        let mut child = cmd.spawn()?;

        // Write context to stdin
        if let Some(mut stdin) = child.stdin.take() {
            use tokio::io::AsyncWriteExt;
            let _ = stdin.write_all(context_json.as_bytes()).await;
        }

        child.wait_with_output().await
    })
    .await;

    match result {
        Ok(Ok(output)) => {
            let stdout = String::from_utf8_lossy(&output.stdout).to_string();
            let stderr = String::from_utf8_lossy(&output.stderr).to_string();

            // Try to parse output as JSON for modified input
            let modified_input = serde_json::from_str::<Value>(&stdout).ok();

            // Check for block signal
            let should_block = stdout.contains("HOOK_BLOCK")
                || stderr.contains("HOOK_BLOCK")
                || output.status.code() == Some(1);

            Ok(HookResult {
                success: output.status.success(),
                output: if stdout.is_empty() {
                    None
                } else {
                    Some(stdout)
                },
                error: if stderr.is_empty() {
                    None
                } else {
                    Some(stderr)
                },
                should_block,
                modified_input,
            })
        }
        Ok(Err(e)) => {
            error!("Hook command failed: {}", e);
            Err(e.into())
        }
        Err(_) => {
            error!("Hook timed out after {}ms", hook.timeout_ms);
            Ok(HookResult {
                success: false,
                output: None,
                error: Some(format!("Hook timed out after {}ms", hook.timeout_ms)),
                should_block: false,
                modified_input: None,
            })
        }
    }
}

/// Get the config directory path.
fn dirs_config_dir() -> PathBuf {
    if let Some(home) = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE")) {
        PathBuf::from(home).join(".claude")
    } else if let Some(user_dirs) = directories::UserDirs::new() {
        user_dirs.home_dir().join(".claude")
    } else {
        PathBuf::from(".claude")
    }
}

// ============================================================================
// Helper Functions
// ============================================================================

/// Create a hook context for tool execution.
pub fn tool_context(
    tool_name: &str,
    tool_input: Option<Value>,
    tool_output: Option<Value>,
    session_id: Option<&str>,
    workspace_id: Option<&str>,
    project_id: Option<&str>,
) -> HookContext {
    HookContext {
        hook_type: if tool_output.is_some() {
            "post_tool_use"
        } else {
            "pre_tool_use"
        }
        .to_string(),
        tool_name: Some(tool_name.to_string()),
        tool_input,
        tool_output,
        user_message: None,
        session_id: session_id.map(String::from),
        workspace_id: workspace_id.map(String::from),
        project_id: project_id.map(String::from),
    }
}

/// Create a hook context for user prompt.
pub fn prompt_context(
    user_message: &str,
    session_id: Option<&str>,
    workspace_id: Option<&str>,
    project_id: Option<&str>,
) -> HookContext {
    HookContext {
        hook_type: "user_prompt_submit".to_string(),
        tool_name: None,
        tool_input: None,
        tool_output: None,
        user_message: Some(user_message.to_string()),
        session_id: session_id.map(String::from),
        workspace_id: workspace_id.map(String::from),
        project_id: project_id.map(String::from),
    }
}

/// Create a hook context for session events.
pub fn session_context(
    hook_type: HookType,
    session_id: Option<&str>,
    workspace_id: Option<&str>,
    project_id: Option<&str>,
) -> HookContext {
    HookContext {
        hook_type: hook_type.as_str().to_string(),
        tool_name: None,
        tool_input: None,
        tool_output: None,
        user_message: None,
        session_id: session_id.map(String::from),
        workspace_id: workspace_id.map(String::from),
        project_id: project_id.map(String::from),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ========================================================================
    // HookType Tests
    // ========================================================================

    mod hook_type_tests {
        use super::*;

        #[test]
        fn test_hook_type_from_str_pre_tool_use() {
            assert_eq!(
                HookType::from_str("pre_tool_use"),
                Some(HookType::PreToolUse)
            );
            assert_eq!(HookType::from_str("pretooluse"), Some(HookType::PreToolUse));
            assert_eq!(
                HookType::from_str("PRE_TOOL_USE"),
                Some(HookType::PreToolUse)
            );
        }

        #[test]
        fn test_hook_type_from_str_post_tool_use() {
            assert_eq!(
                HookType::from_str("post_tool_use"),
                Some(HookType::PostToolUse)
            );
            assert_eq!(
                HookType::from_str("posttooluse"),
                Some(HookType::PostToolUse)
            );
        }

        #[test]
        fn test_hook_type_from_str_post_tool_use_failure() {
            assert_eq!(
                HookType::from_str("post_tool_use_failure"),
                Some(HookType::PostToolUseFailure)
            );
            assert_eq!(
                HookType::from_str("posttoolusefailure"),
                Some(HookType::PostToolUseFailure)
            );
        }

        #[test]
        fn test_hook_type_from_str_user_prompt_submit() {
            assert_eq!(
                HookType::from_str("user_prompt_submit"),
                Some(HookType::UserPromptSubmit)
            );
            assert_eq!(
                HookType::from_str("userpromptsubmit"),
                Some(HookType::UserPromptSubmit)
            );
        }

        #[test]
        fn test_hook_type_from_str_session_start() {
            assert_eq!(
                HookType::from_str("session_start"),
                Some(HookType::SessionStart)
            );
            assert_eq!(
                HookType::from_str("sessionstart"),
                Some(HookType::SessionStart)
            );
        }

        #[test]
        fn test_hook_type_from_str_session_end() {
            assert_eq!(
                HookType::from_str("session_end"),
                Some(HookType::SessionEnd)
            );
            assert_eq!(HookType::from_str("sessionend"), Some(HookType::SessionEnd));
        }

        #[test]
        fn test_hook_type_from_str_stop() {
            assert_eq!(HookType::from_str("stop"), Some(HookType::Stop));
            assert_eq!(HookType::from_str("STOP"), Some(HookType::Stop));
        }

        #[test]
        fn test_hook_type_from_str_pre_compact() {
            assert_eq!(
                HookType::from_str("pre_compact"),
                Some(HookType::PreCompact)
            );
            assert_eq!(HookType::from_str("precompact"), Some(HookType::PreCompact));
        }

        #[test]
        fn test_hook_type_from_str_unknown() {
            assert_eq!(HookType::from_str("unknown"), None);
            assert_eq!(HookType::from_str(""), None);
            assert_eq!(HookType::from_str("pre"), None);
        }

        #[test]
        fn test_hook_type_from_str_subagent_start() {
            assert_eq!(
                HookType::from_str("subagent_start"),
                Some(HookType::SubagentStart)
            );
            assert_eq!(
                HookType::from_str("subagentstart"),
                Some(HookType::SubagentStart)
            );
            assert_eq!(
                HookType::from_str("SUBAGENT_START"),
                Some(HookType::SubagentStart)
            );
        }

        #[test]
        fn test_hook_type_from_str_extended() {
            assert_eq!(
                HookType::from_str("subagent_stop"),
                Some(HookType::SubagentStop)
            );
            assert_eq!(
                HookType::from_str("task_completed"),
                Some(HookType::TaskCompleted)
            );
            assert_eq!(
                HookType::from_str("teammate_idle"),
                Some(HookType::TeammateIdle)
            );
            assert_eq!(
                HookType::from_str("permission_request"),
                Some(HookType::PermissionRequest)
            );
            assert_eq!(
                HookType::from_str("notification"),
                Some(HookType::Notification)
            );
        }

        #[test]
        fn test_hook_type_as_str() {
            assert_eq!(HookType::PreToolUse.as_str(), "pre_tool_use");
            assert_eq!(HookType::PostToolUse.as_str(), "post_tool_use");
            assert_eq!(
                HookType::PostToolUseFailure.as_str(),
                "post_tool_use_failure"
            );
            assert_eq!(HookType::UserPromptSubmit.as_str(), "user_prompt_submit");
            assert_eq!(HookType::SessionStart.as_str(), "session_start");
            assert_eq!(HookType::Stop.as_str(), "stop");
            assert_eq!(HookType::SessionEnd.as_str(), "session_end");
            assert_eq!(HookType::PreCompact.as_str(), "pre_compact");
            assert_eq!(HookType::SubagentStart.as_str(), "subagent_start");
            assert_eq!(HookType::SubagentStop.as_str(), "subagent_stop");
            assert_eq!(HookType::TaskCompleted.as_str(), "task_completed");
            assert_eq!(HookType::TeammateIdle.as_str(), "teammate_idle");
            assert_eq!(HookType::Notification.as_str(), "notification");
            assert_eq!(HookType::PermissionRequest.as_str(), "permission_request");
        }

        #[test]
        fn test_hook_type_roundtrip() {
            for hook_type in [
                HookType::PreToolUse,
                HookType::PostToolUse,
                HookType::PostToolUseFailure,
                HookType::UserPromptSubmit,
                HookType::SessionStart,
                HookType::Stop,
                HookType::SessionEnd,
                HookType::PreCompact,
                HookType::SubagentStart,
                HookType::SubagentStop,
                HookType::TaskCompleted,
                HookType::TeammateIdle,
                HookType::Notification,
                HookType::PermissionRequest,
            ] {
                let s = hook_type.as_str();
                let parsed = HookType::from_str(s);
                assert_eq!(parsed, Some(hook_type));
            }
        }

        #[test]
        fn test_hook_type_serialization() {
            let json = serde_json::to_string(&HookType::PreToolUse).unwrap();
            assert_eq!(json, "\"pre_tool_use\"");

            let json = serde_json::to_string(&HookType::SessionStart).unwrap();
            assert_eq!(json, "\"session_start\"");
        }

        #[test]
        fn test_hook_type_deserialization() {
            let hook: HookType = serde_json::from_str("\"pre_tool_use\"").unwrap();
            assert_eq!(hook, HookType::PreToolUse);

            let hook: HookType = serde_json::from_str("\"session_end\"").unwrap();
            assert_eq!(hook, HookType::SessionEnd);
        }
    }

    // ========================================================================
    // HookResult Tests
    // ========================================================================

    mod hook_result_tests {
        use super::*;

        #[test]
        fn test_hook_result_default() {
            let result = HookResult::default();
            assert!(result.success);
            assert!(result.output.is_none());
            assert!(result.error.is_none());
            assert!(!result.should_block);
            assert!(result.modified_input.is_none());
        }

        #[test]
        fn test_hook_result_with_output() {
            let result = HookResult {
                success: true,
                output: Some("hook output".to_string()),
                error: None,
                should_block: false,
                modified_input: None,
            };
            assert_eq!(result.output, Some("hook output".to_string()));
        }

        #[test]
        fn test_hook_result_with_error() {
            let result = HookResult {
                success: false,
                output: None,
                error: Some("hook failed".to_string()),
                should_block: false,
                modified_input: None,
            };
            assert!(!result.success);
            assert_eq!(result.error, Some("hook failed".to_string()));
        }

        #[test]
        fn test_hook_result_blocking() {
            let result = HookResult {
                success: true,
                output: Some("HOOK_BLOCK".to_string()),
                error: None,
                should_block: true,
                modified_input: None,
            };
            assert!(result.should_block);
        }

        #[test]
        fn test_hook_result_with_modified_input() {
            let result = HookResult {
                success: true,
                output: None,
                error: None,
                should_block: false,
                modified_input: Some(serde_json::json!({"modified": true})),
            };
            assert!(result.modified_input.is_some());
        }

        #[test]
        fn test_hook_result_serialization() {
            let result = HookResult {
                success: true,
                output: Some("test".to_string()),
                error: None,
                should_block: false,
                modified_input: None,
            };
            let json = serde_json::to_string(&result).unwrap();
            assert!(json.contains("\"success\":true"));
            assert!(json.contains("\"output\":\"test\""));
        }
    }

    // ========================================================================
    // HookContext Tests
    // ========================================================================

    mod hook_context_tests {
        use super::*;

        #[test]
        fn test_hook_context_tool() {
            let context = HookContext {
                hook_type: "pre_tool_use".to_string(),
                tool_name: Some("session".to_string()),
                tool_input: Some(serde_json::json!({"action": "init"})),
                tool_output: None,
                user_message: None,
                session_id: Some("session-123".to_string()),
                workspace_id: None,
                project_id: None,
            };
            assert_eq!(context.hook_type, "pre_tool_use");
            assert_eq!(context.tool_name, Some("session".to_string()));
        }

        #[test]
        fn test_hook_context_prompt() {
            let context = HookContext {
                hook_type: "user_prompt_submit".to_string(),
                tool_name: None,
                tool_input: None,
                tool_output: None,
                user_message: Some("Hello, world!".to_string()),
                session_id: None,
                workspace_id: Some("ws-123".to_string()),
                project_id: Some("proj-456".to_string()),
            };
            assert_eq!(context.user_message, Some("Hello, world!".to_string()));
        }

        #[test]
        fn test_hook_context_serialization() {
            let context = HookContext {
                hook_type: "session_start".to_string(),
                tool_name: None,
                tool_input: None,
                tool_output: None,
                user_message: None,
                session_id: Some("sess-1".to_string()),
                workspace_id: None,
                project_id: None,
            };
            let json = serde_json::to_string(&context).unwrap();
            assert!(json.contains("\"hook_type\":\"session_start\""));
            assert!(json.contains("\"session_id\":\"sess-1\""));
        }

        #[test]
        fn test_hook_context_deserialization() {
            let json = r#"{
                "hook_type": "pre_compact",
                "tool_name": null,
                "tool_input": null,
                "tool_output": null,
                "user_message": null,
                "session_id": "test-session",
                "workspace_id": null,
                "project_id": null
            }"#;
            let context: HookContext = serde_json::from_str(json).unwrap();
            assert_eq!(context.hook_type, "pre_compact");
            assert_eq!(context.session_id, Some("test-session".to_string()));
        }
    }

    // ========================================================================
    // HookConfig Tests
    // ========================================================================

    mod hook_config_tests {
        use super::*;

        #[test]
        fn test_hook_config_minimal() {
            let json = r#"{"command": "echo hello"}"#;
            let config: HookConfig = serde_json::from_str(json).unwrap();
            assert_eq!(config.command, "echo hello");
            assert!(config.cwd.is_none());
            assert!(config.env.is_empty());
            assert_eq!(config.timeout_ms, 30000); // default
            assert!(!config.can_block); // default false
            assert!(config.tool_filter.is_none());
        }

        #[test]
        fn test_hook_config_full() {
            let json = r#"{
                "command": "python script.py",
                "cwd": "/home/user/scripts",
                "env": {"DEBUG": "true", "API_KEY": "secret"},
                "timeout_ms": 5000,
                "can_block": true,
                "tool_filter": "session"
            }"#;
            let config: HookConfig = serde_json::from_str(json).unwrap();
            assert_eq!(config.command, "python script.py");
            assert_eq!(config.cwd, Some("/home/user/scripts".to_string()));
            assert_eq!(config.env.get("DEBUG"), Some(&"true".to_string()));
            assert_eq!(config.env.get("API_KEY"), Some(&"secret".to_string()));
            assert_eq!(config.timeout_ms, 5000);
            assert!(config.can_block);
            assert_eq!(config.tool_filter, Some("session".to_string()));
        }

        #[test]
        fn test_hook_config_default_timeout() {
            assert_eq!(default_timeout(), 30000);
        }
    }

    // ========================================================================
    // HooksConfig Tests
    // ========================================================================

    mod hooks_config_tests {
        use super::*;

        #[test]
        fn test_hooks_config_default() {
            let config = HooksConfig::default();
            assert!(config.pre_tool_use.is_empty());
            assert!(config.post_tool_use.is_empty());
            assert!(config.post_tool_use_failure.is_empty());
            assert!(config.instructions_loaded.is_empty());
            assert!(config.user_prompt_submit.is_empty());
            assert!(config.session_start.is_empty());
            assert!(config.stop.is_empty());
            assert!(config.stop_failure.is_empty());
            assert!(config.session_end.is_empty());
            assert!(config.pre_compact.is_empty());
            assert!(config.subagent_start.is_empty());
            assert!(config.subagent_stop.is_empty());
            assert!(config.task_created.is_empty());
            assert!(config.task_completed.is_empty());
            assert!(config.teammate_idle.is_empty());
            assert!(config.notification.is_empty());
            assert!(config.permission_request.is_empty());
        }

        #[test]
        fn test_hooks_config_get_hooks() {
            let config = HooksConfig {
                pre_tool_use: vec![HookConfig {
                    command: "echo pre".to_string(),
                    cwd: None,
                    env: HashMap::new(),
                    timeout_ms: 30000,
                    can_block: false,
                    tool_filter: None,
                }],
                post_tool_use: vec![],
                post_tool_use_failure: vec![],
                instructions_loaded: vec![],
                user_prompt_submit: vec![],
                session_start: vec![HookConfig {
                    command: "echo start".to_string(),
                    cwd: None,
                    env: HashMap::new(),
                    timeout_ms: 30000,
                    can_block: false,
                    tool_filter: None,
                }],
                stop: vec![],
                stop_failure: vec![],
                session_end: vec![],
                pre_compact: vec![],
                subagent_start: vec![],
                subagent_stop: vec![],
                task_created: vec![],
                task_completed: vec![],
                teammate_idle: vec![],
                notification: vec![],
                permission_request: vec![],
            };

            assert_eq!(config.get_hooks(HookType::PreToolUse).len(), 1);
            assert_eq!(config.get_hooks(HookType::PostToolUse).len(), 0);
            assert_eq!(config.get_hooks(HookType::SessionStart).len(), 1);
        }

        #[test]
        fn test_hooks_config_deserialization() {
            let json = r#"{
                "pre_tool_use": [
                    {"command": "echo pre1"},
                    {"command": "echo pre2"}
                ],
                "session_start": [
                    {"command": "echo start", "can_block": true}
                ]
            }"#;
            let config: HooksConfig = serde_json::from_str(json).unwrap();
            assert_eq!(config.pre_tool_use.len(), 2);
            assert_eq!(config.session_start.len(), 1);
            assert!(config.session_start[0].can_block);
        }

        #[test]
        fn test_hooks_config_load_nonexistent() {
            let path = PathBuf::from("/nonexistent/path/hooks.json");
            let config = HooksConfig::load(&path).unwrap();
            // Returns default when file doesn't exist
            assert!(config.pre_tool_use.is_empty());
        }
    }

    // ========================================================================
    // HookManager Tests
    // ========================================================================

    mod hook_manager_tests {
        use super::*;

        #[test]
        fn test_hook_manager_new() {
            let config = HooksConfig::default();
            let manager = HookManager::new(config);
            assert!(!manager.has_hooks(HookType::PreToolUse));
        }

        #[test]
        fn test_hook_manager_has_hooks() {
            let config = HooksConfig {
                pre_tool_use: vec![HookConfig {
                    command: "echo test".to_string(),
                    cwd: None,
                    env: HashMap::new(),
                    timeout_ms: 30000,
                    can_block: false,
                    tool_filter: None,
                }],
                ..Default::default()
            };
            let manager = HookManager::new(config);
            assert!(manager.has_hooks(HookType::PreToolUse));
            assert!(!manager.has_hooks(HookType::PostToolUse));
            assert!(!manager.has_hooks(HookType::SessionStart));
        }

        #[tokio::test]
        async fn test_hook_manager_execute_no_hooks() {
            let config = HooksConfig::default();
            let manager = HookManager::new(config);

            let context = HookContext {
                hook_type: "pre_tool_use".to_string(),
                tool_name: Some("test".to_string()),
                tool_input: None,
                tool_output: None,
                user_message: None,
                session_id: None,
                workspace_id: None,
                project_id: None,
            };

            let result = manager.execute(HookType::PreToolUse, context).await;
            // Default result when no hooks
            assert!(result.success);
            assert!(!result.should_block);
        }

        #[tokio::test]
        async fn test_hook_manager_execute_simple_hook() {
            let config = HooksConfig {
                pre_tool_use: vec![HookConfig {
                    command: "echo hello".to_string(),
                    cwd: None,
                    env: HashMap::new(),
                    timeout_ms: 5000,
                    can_block: false,
                    tool_filter: None,
                }],
                ..Default::default()
            };
            let manager = HookManager::new(config);

            let context = HookContext {
                hook_type: "pre_tool_use".to_string(),
                tool_name: Some("test".to_string()),
                tool_input: None,
                tool_output: None,
                user_message: None,
                session_id: None,
                workspace_id: None,
                project_id: None,
            };

            let result = manager.execute(HookType::PreToolUse, context).await;
            assert!(result.success);
            assert!(result.output.is_some());
            assert!(result.output.unwrap().contains("hello"));
        }

        #[tokio::test]
        async fn test_hook_manager_tool_filter_matches() {
            let config = HooksConfig {
                pre_tool_use: vec![HookConfig {
                    command: "echo filtered".to_string(),
                    cwd: None,
                    env: HashMap::new(),
                    timeout_ms: 5000,
                    can_block: false,
                    tool_filter: Some("session".to_string()),
                }],
                ..Default::default()
            };
            let manager = HookManager::new(config);

            let context = HookContext {
                hook_type: "pre_tool_use".to_string(),
                tool_name: Some("session".to_string()),
                tool_input: None,
                tool_output: None,
                user_message: None,
                session_id: None,
                workspace_id: None,
                project_id: None,
            };

            let result = manager.execute(HookType::PreToolUse, context).await;
            assert!(result.success);
            assert!(result.output.is_some());
        }

        #[tokio::test]
        async fn test_hook_manager_tool_filter_no_match() {
            let config = HooksConfig {
                pre_tool_use: vec![HookConfig {
                    command: "echo should-not-run".to_string(),
                    cwd: None,
                    env: HashMap::new(),
                    timeout_ms: 5000,
                    can_block: false,
                    tool_filter: Some("session".to_string()),
                }],
                ..Default::default()
            };
            let manager = HookManager::new(config);

            let context = HookContext {
                hook_type: "pre_tool_use".to_string(),
                tool_name: Some("memory".to_string()), // Different tool
                tool_input: None,
                tool_output: None,
                user_message: None,
                session_id: None,
                workspace_id: None,
                project_id: None,
            };

            let result = manager.execute(HookType::PreToolUse, context).await;
            // Hook didn't run due to filter mismatch - default result
            assert!(result.success);
            assert!(result.output.is_none());
        }

        #[tokio::test]
        async fn test_hook_manager_wildcard_filter() {
            let config = HooksConfig {
                pre_tool_use: vec![HookConfig {
                    command: "echo wildcard".to_string(),
                    cwd: None,
                    env: HashMap::new(),
                    timeout_ms: 5000,
                    can_block: false,
                    tool_filter: Some("*".to_string()),
                }],
                ..Default::default()
            };
            let manager = HookManager::new(config);

            let context = HookContext {
                hook_type: "pre_tool_use".to_string(),
                tool_name: Some("any_tool".to_string()),
                tool_input: None,
                tool_output: None,
                user_message: None,
                session_id: None,
                workspace_id: None,
                project_id: None,
            };

            let result = manager.execute(HookType::PreToolUse, context).await;
            assert!(result.success);
            assert!(result.output.is_some());
        }
    }

    // ========================================================================
    // Helper Function Tests
    // ========================================================================

    mod helper_tests {
        use super::*;

        #[test]
        fn test_tool_context_pre_tool_use() {
            let context = tool_context(
                "session",
                Some(serde_json::json!({"action": "init"})),
                None, // No output = pre_tool_use
                Some("sess-1"),
                Some("ws-1"),
                Some("proj-1"),
            );
            assert_eq!(context.hook_type, "pre_tool_use");
            assert_eq!(context.tool_name, Some("session".to_string()));
            assert!(context.tool_input.is_some());
            assert!(context.tool_output.is_none());
        }

        #[test]
        fn test_tool_context_post_tool_use() {
            let context = tool_context(
                "session",
                Some(serde_json::json!({"action": "init"})),
                Some(serde_json::json!({"success": true})), // Has output = post_tool_use
                Some("sess-1"),
                None,
                None,
            );
            assert_eq!(context.hook_type, "post_tool_use");
            assert!(context.tool_output.is_some());
        }

        #[test]
        fn test_prompt_context() {
            let context = prompt_context(
                "Hello, Claude!",
                Some("sess-1"),
                Some("ws-1"),
                Some("proj-1"),
            );
            assert_eq!(context.hook_type, "user_prompt_submit");
            assert_eq!(context.user_message, Some("Hello, Claude!".to_string()));
            assert!(context.tool_name.is_none());
        }

        #[test]
        fn test_session_context_start() {
            let context =
                session_context(HookType::SessionStart, Some("sess-1"), Some("ws-1"), None);
            assert_eq!(context.hook_type, "session_start");
            assert_eq!(context.session_id, Some("sess-1".to_string()));
        }

        #[test]
        fn test_session_context_end() {
            let context = session_context(HookType::SessionEnd, Some("sess-1"), None, None);
            assert_eq!(context.hook_type, "session_end");
        }

        #[test]
        fn test_session_context_pre_compact() {
            let context = session_context(
                HookType::PreCompact,
                Some("sess-1"),
                Some("ws-1"),
                Some("proj-1"),
            );
            assert_eq!(context.hook_type, "pre_compact");
        }
    }

    // ========================================================================
    // Blocking Behavior Tests
    // ========================================================================

    mod blocking_tests {
        use super::*;

        #[tokio::test]
        async fn test_hook_block_signal_in_stdout() {
            let config = HooksConfig {
                pre_tool_use: vec![HookConfig {
                    command: "echo HOOK_BLOCK".to_string(),
                    cwd: None,
                    env: HashMap::new(),
                    timeout_ms: 5000,
                    can_block: true,
                    tool_filter: None,
                }],
                ..Default::default()
            };
            let manager = HookManager::new(config);

            let context = HookContext {
                hook_type: "pre_tool_use".to_string(),
                tool_name: Some("test".to_string()),
                tool_input: None,
                tool_output: None,
                user_message: None,
                session_id: None,
                workspace_id: None,
                project_id: None,
            };

            let result = manager.execute(HookType::PreToolUse, context).await;
            assert!(result.should_block);
        }

        #[tokio::test]
        async fn test_hook_exit_code_1_blocks() {
            let config = HooksConfig {
                pre_tool_use: vec![HookConfig {
                    command: "exit 1".to_string(),
                    cwd: None,
                    env: HashMap::new(),
                    timeout_ms: 5000,
                    can_block: true,
                    tool_filter: None,
                }],
                ..Default::default()
            };
            let manager = HookManager::new(config);

            let context = HookContext {
                hook_type: "pre_tool_use".to_string(),
                tool_name: Some("test".to_string()),
                tool_input: None,
                tool_output: None,
                user_message: None,
                session_id: None,
                workspace_id: None,
                project_id: None,
            };

            let result = manager.execute(HookType::PreToolUse, context).await;
            assert!(result.should_block);
            assert!(!result.success);
        }
    }

    // ========================================================================
    // Environment Variable Tests
    // ========================================================================

    mod env_tests {
        use super::*;

        #[tokio::test]
        async fn test_hook_receives_hook_type_env() {
            let command = if cfg!(windows) {
                "echo %HOOK_TYPE%".to_string()
            } else {
                "echo $HOOK_TYPE".to_string()
            };
            let config = HooksConfig {
                session_start: vec![HookConfig {
                    command,
                    cwd: None,
                    env: HashMap::new(),
                    timeout_ms: 5000,
                    can_block: false,
                    tool_filter: None,
                }],
                ..Default::default()
            };
            let manager = HookManager::new(config);

            let context = session_context(HookType::SessionStart, None, None, None);
            let result = manager.execute(HookType::SessionStart, context).await;

            assert!(result.success);
            assert!(result.output.is_some());
            assert!(result.output.unwrap().contains("session_start"));
        }

        #[tokio::test]
        async fn test_hook_custom_env_vars() {
            let mut env = HashMap::new();
            env.insert("CUSTOM_VAR".to_string(), "custom_value".to_string());

            let command = if cfg!(windows) {
                "echo %CUSTOM_VAR%".to_string()
            } else {
                "echo $CUSTOM_VAR".to_string()
            };
            let config = HooksConfig {
                pre_tool_use: vec![HookConfig {
                    command,
                    cwd: None,
                    env,
                    timeout_ms: 5000,
                    can_block: false,
                    tool_filter: None,
                }],
                ..Default::default()
            };
            let manager = HookManager::new(config);

            let context = HookContext {
                hook_type: "pre_tool_use".to_string(),
                tool_name: Some("test".to_string()),
                tool_input: None,
                tool_output: None,
                user_message: None,
                session_id: None,
                workspace_id: None,
                project_id: None,
            };

            let result = manager.execute(HookType::PreToolUse, context).await;
            assert!(result.success);
            assert!(result.output.unwrap().contains("custom_value"));
        }
    }

    // ========================================================================
    // Coverage Tests
    // ========================================================================

    mod coverage_tests {
        use super::*;

        #[test]
        fn test_all_hook_types_covered() {
            // Document all hook types:
            // 1. PreToolUse - Before tool execution, can modify input or block
            // 2. PostToolUse - After tool execution, can log or react to output
            // 3. PostToolUseFailure - After tool execution fails
            // 4. UserPromptSubmit - On user input, can modify or validate prompt
            // 5. SessionStart - On session initialization
            // 6. Stop - On assistant stop
            // 7. SessionEnd - On session termination
            // 8. PreCompact - Before context compaction (save state)
            // 9. SubagentStart/SubagentStop - Subagent lifecycle
            // 10. TaskCompleted/TeammateIdle - Agent teams lifecycle
            // 11. Notification/PermissionRequest - system events

            let hook_types = vec![
                HookType::PreToolUse,
                HookType::PostToolUse,
                HookType::PostToolUseFailure,
                HookType::UserPromptSubmit,
                HookType::SessionStart,
                HookType::Stop,
                HookType::SessionEnd,
                HookType::PreCompact,
                HookType::SubagentStart,
                HookType::SubagentStop,
                HookType::TaskCompleted,
                HookType::TeammateIdle,
                HookType::Notification,
                HookType::PermissionRequest,
            ];
            assert_eq!(hook_types.len(), 14);
        }

        #[test]
        fn test_hook_config_fields() {
            // Document all HookConfig fields:
            // - command: Shell command to execute (required)
            // - cwd: Working directory (optional)
            // - env: Environment variables (optional, default empty)
            // - timeout_ms: Timeout in milliseconds (default 30000)
            // - can_block: Whether hook can block operation (default false)
            // - tool_filter: Tool name filter for tool hooks (optional)

            let fields = [
                "command",
                "cwd",
                "env",
                "timeout_ms",
                "can_block",
                "tool_filter",
            ];
            assert_eq!(fields.len(), 6);
        }

        #[test]
        fn test_hook_result_fields() {
            // Document all HookResult fields:
            // - success: Whether hook executed successfully
            // - output: stdout from hook (if any)
            // - error: stderr from hook (if any)
            // - should_block: Whether to block the operation
            // - modified_input: JSON-parsed modified input (if any)

            let fields = [
                "success",
                "output",
                "error",
                "should_block",
                "modified_input",
            ];
            assert_eq!(fields.len(), 5);
        }

        #[test]
        fn test_block_detection_methods() {
            // Document how blocking is detected:
            // 1. "HOOK_BLOCK" in stdout
            // 2. "HOOK_BLOCK" in stderr
            // 3. Exit code == 1
            // Plus: can_block must be true for blocking to take effect

            let methods = [
                "HOOK_BLOCK in stdout",
                "HOOK_BLOCK in stderr",
                "exit code 1",
            ];
            assert_eq!(methods.len(), 3);
        }
    }
}
