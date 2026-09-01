//! Configuration loading for the MCP server.

use crate::setup::local_dev_api_url_override;
use anyhow::{anyhow, Result};
use mcp_types::{
    config::{LogLevel, OutputFormat, ToolSurfaceProfile, Toolset, DEFAULT_API_URL, VERSION},
    AccountModePreference, Config,
};
use std::path::PathBuf;
use uuid::Uuid;

/// Load configuration from environment variables and config files.
pub fn load_config() -> Result<Config> {
    let saved_credentials = if std::env::var("CONTEXTSTREAM_API_KEY").is_err()
        && std::env::var("CONTEXTSTREAM_JWT").is_err()
    {
        load_saved_credentials()
    } else {
        None
    };

    // Try to load saved credentials if env vars not set.
    if let Some(saved) = saved_credentials.as_ref() {
        std::env::set_var("CONTEXTSTREAM_API_KEY", &saved.api_key);
    }

    // Prefer an explicit env override, then a source-built local dev binary,
    // then saved credentials, and finally the production default.
    let api_url = resolve_api_url(saved_credentials.as_ref());
    if std::env::var("CONTEXTSTREAM_API_URL").is_err() {
        std::env::set_var("CONTEXTSTREAM_API_URL", &api_url);
    }

    let api_key = std::env::var("CONTEXTSTREAM_API_KEY").ok();
    let jwt = std::env::var("CONTEXTSTREAM_JWT").ok();
    let allow_header_auth = parse_bool_env("CONTEXTSTREAM_ALLOW_HEADER_AUTH");

    // Check for credentials
    if api_key.is_none() && jwt.is_none() && !allow_header_auth {
        return Err(anyhow!(
            "Missing credentials: Set CONTEXTSTREAM_API_KEY or CONTEXTSTREAM_JWT for authentication"
        ));
    }

    let default_workspace_id = std::env::var("CONTEXTSTREAM_WORKSPACE_ID")
        .ok()
        .and_then(|s| Uuid::parse_str(&s).ok());

    let default_project_id = std::env::var("CONTEXTSTREAM_PROJECT_ID")
        .ok()
        .and_then(|s| Uuid::parse_str(&s).ok());

    let user_agent = std::env::var("CONTEXTSTREAM_USER_AGENT")
        .unwrap_or_else(|_| format!("contextstream-mcp-rust/{}", VERSION));

    let toolset = std::env::var("CONTEXTSTREAM_TOOLSET")
        .ok()
        .and_then(|s| s.parse::<Toolset>().ok())
        .unwrap_or_default();

    let log_level = std::env::var("CONTEXTSTREAM_LOG_LEVEL")
        .ok()
        .and_then(|s| s.parse::<LogLevel>().ok())
        .unwrap_or_default();

    let output_format = std::env::var("CONTEXTSTREAM_OUTPUT_FORMAT")
        .ok()
        .and_then(|s| s.parse::<OutputFormat>().ok())
        .unwrap_or_default();

    let context_pack_enabled = parse_bool_env_default("CONTEXTSTREAM_CONTEXT_PACK", true);
    let show_timing = parse_bool_env("CONTEXTSTREAM_SHOW_TIMING");
    let progressive_mode = parse_bool_env("CONTEXTSTREAM_PROGRESSIVE_MODE");
    let router_mode = parse_bool_env("CONTEXTSTREAM_ROUTER_MODE");
    let consolidated_mode = parse_bool_env_default("CONTEXTSTREAM_CONSOLIDATED", true);
    let auto_hide_integrations =
        parse_bool_env_default("CONTEXTSTREAM_AUTO_HIDE_INTEGRATIONS", true);

    let search_limit = std::env::var("CONTEXTSTREAM_SEARCH_LIMIT")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(10);

    let search_max_chars = std::env::var("CONTEXTSTREAM_SEARCH_MAX_CHARS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(800);

    // Transcript capture defaults to ON. Past-session context is central to
    // the "smart context & memory" promise — the only reasons to disable are
    // privacy edge-cases (regulated data) or explicit user opt-out. If this
    // returns false the assistant can still READ existing transcripts, it
    // just stops writing new ones.
    let transcripts_enabled = parse_bool_env_default("CONTEXTSTREAM_TRANSCRIPTS_ENABLED", true);
    let hook_transcripts_enabled =
        parse_bool_env_default("CONTEXTSTREAM_HOOK_TRANSCRIPTS_ENABLED", true);
    let capsule_enabled = parse_bool_env_default("CONTEXTSTREAM_CAPSULE_ENABLED", true);

    let tool_surface_profile = std::env::var("CONTEXTSTREAM_TOOL_SURFACE_PROFILE")
        .ok()
        .and_then(|s| s.parse::<ToolSurfaceProfile>().ok())
        .unwrap_or_else(|| infer_tool_surface_profile(&user_agent));

    let account_mode_preference = std::env::var("CONTEXTSTREAM_ACCOUNT_MODE")
        .ok()
        .and_then(|s| s.parse::<AccountModePreference>().ok())
        .unwrap_or_default();

    Ok(Config {
        api_url,
        api_key,
        jwt,
        default_workspace_id,
        default_project_id,
        user_agent,
        allow_header_auth,
        context_pack_enabled,
        show_timing,
        toolset,
        log_level,
        output_format,
        progressive_mode,
        router_mode,
        consolidated_mode,
        auto_hide_integrations,
        capsule_enabled,
        search_limit,
        search_max_chars,
        transcripts_enabled,
        hook_transcripts_enabled,
        tool_surface_profile,
        is_http_transport: false,
        account_mode_preference,
    })
}

fn resolve_api_url(saved: Option<&mcp_types::config::SavedCredentials>) -> String {
    if let Ok(api_url) = std::env::var("CONTEXTSTREAM_API_URL") {
        return api_url;
    }

    if let Some(local_api_url) = local_dev_api_url_override() {
        return local_api_url;
    }

    if let Some(saved) = saved {
        return saved.api_url.clone();
    }

    DEFAULT_API_URL.to_string()
}

fn infer_tool_surface_profile(user_agent: &str) -> ToolSurfaceProfile {
    let lowered = user_agent.to_ascii_lowercase();
    if lowered.contains("openai")
        || lowered.contains("chatgpt")
        || lowered.contains("responses-api")
        || lowered.contains("gpt-5")
    {
        ToolSurfaceProfile::OpenaiAgentic
    } else {
        ToolSurfaceProfile::Default
    }
}

/// Load saved credentials from ~/.contextstream/credentials.json
fn load_saved_credentials() -> Option<mcp_types::config::SavedCredentials> {
    let home = dirs::home_dir()?;
    let credentials_path = home.join(".contextstream").join("credentials.json");

    if !credentials_path.exists() {
        return None;
    }

    let content = std::fs::read_to_string(&credentials_path).ok()?;
    serde_json::from_str(&content).ok()
}

fn parse_bool_env(name: &str) -> bool {
    std::env::var(name)
        .map(|v| {
            let v = v.to_lowercase();
            v == "1" || v == "true" || v == "yes" || v == "on"
        })
        .unwrap_or(false)
}

fn parse_bool_env_default(name: &str, default: bool) -> bool {
    std::env::var(name)
        .map(|v| {
            let v = v.to_lowercase();
            v == "1" || v == "true" || v == "yes" || v == "on"
        })
        .unwrap_or(default)
}

/// Global default for local git capture when no per-repo policy applies.
///
/// Capture is ON by default; an org can flip the default to opt-in by setting
/// `CONTEXTSTREAM_GIT_CAPTURE_DEFAULT=off`. Both the `CONTEXTSTREAM_GIT_CAPTURE`
/// env kill-switch and a per-repo `.contextstream/config.json` `git_capture.enabled`
/// flag take precedence over this default (resolved in
/// [`crate::hook_handlers::git_common::capture_disabled`]).
pub fn git_capture_default_enabled() -> bool {
    parse_bool_env_default("CONTEXTSTREAM_GIT_CAPTURE_DEFAULT", true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::env_test_mutex;
    use mcp_types::config::SavedCredentials;

    #[test]
    fn resolve_api_url_prefers_saved_when_no_override_exists() {
        let _guard = env_test_mutex().lock().unwrap_or_else(|e| e.into_inner());
        let saved = SavedCredentials {
            api_url: "https://saved.example.com".to_string(),
            api_key: "test-key".to_string(),
        };

        std::env::remove_var("CONTEXTSTREAM_API_URL");

        // Test binaries run from target/debug/deps, so local_dev_api_url_override() is false here.
        assert_eq!(
            resolve_api_url(Some(&saved)),
            "https://saved.example.com".to_string()
        );
    }

    #[test]
    fn resolve_api_url_prefers_env_over_saved() {
        let _guard = env_test_mutex().lock().unwrap_or_else(|e| e.into_inner());
        let saved = SavedCredentials {
            api_url: "https://saved.example.com".to_string(),
            api_key: "test-key".to_string(),
        };

        std::env::set_var("CONTEXTSTREAM_API_URL", "http://localhost:9999");
        assert_eq!(
            resolve_api_url(Some(&saved)),
            "http://localhost:9999".to_string()
        );
        std::env::remove_var("CONTEXTSTREAM_API_URL");
    }
}

/// Get the config directory path.
pub fn config_dir() -> PathBuf {
    dirs::home_dir()
        .map(|h| h.join(".contextstream"))
        .unwrap_or_else(|| PathBuf::from(".contextstream"))
}

mod dirs {
    use std::path::PathBuf;

    pub fn home_dir() -> Option<PathBuf> {
        #[cfg(target_os = "windows")]
        {
            std::env::var_os("USERPROFILE").map(PathBuf::from)
        }

        #[cfg(not(target_os = "windows"))]
        {
            std::env::var_os("HOME").map(PathBuf::from)
        }
    }
}
