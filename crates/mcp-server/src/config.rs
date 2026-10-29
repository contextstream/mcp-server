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
    load_config_with_credentials(true)
}

/// Configuration for limited mode: identical resolution (API URL, toolset,
/// logging) but no credentials are required. Used when the stdio server
/// starts without an API key so the `init`/`account` tools can connect one.
pub fn load_limited_config() -> Result<Config> {
    let mut config = load_config_with_credentials(false)?;
    config.api_key = None;
    config.jwt = None;
    config.transcripts_enabled = false;
    config.hook_transcripts_enabled = false;
    Ok(config)
}

/// A credential variable's value, or `None` when it is unset, empty or blank.
/// An empty one is what an editor config sends when the variable it names
/// (`"${CONTEXTSTREAM_API_KEY}"`) is unset; it is no credential (#311).
fn credential_env(name: &str) -> Option<String> {
    std::env::var(name)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

/// An enum-valued setting, parsed: `None` when it is unset or blank, or when
/// its value is not one `T` knows. The second value is what to tell the
/// person about an unknown value, which used to be dropped without a word
/// (#319: `CONTEXTSTREAM_TOOLSET=bogus` served the default toolset).
fn parse_enum_setting<T: std::str::FromStr>(
    name: &str,
    raw: Option<&str>,
    known: &str,
) -> (Option<T>, Option<String>) {
    let Some(value) = raw.map(str::trim).filter(|value| !value.is_empty()) else {
        return (None, None);
    };
    match value.parse::<T>() {
        Ok(parsed) => (Some(parsed), None),
        Err(_) => (
            None,
            Some(format!(
                "contextstream-mcp: {name}=\"{value}\" is not one of {known}; using the default."
            )),
        ),
    }
}

/// An enum-valued environment variable; an unknown value is reported on
/// stderr, which an MCP client shows in its server log.
fn enum_env<T: std::str::FromStr>(name: &str, known: &str) -> Option<T> {
    let raw = std::env::var(name).ok();
    let (parsed, warning) = parse_enum_setting(name, raw.as_deref(), known);
    if let Some(warning) = warning {
        eprintln!("{warning}");
    }
    parsed
}

fn load_config_with_credentials(require_credentials: bool) -> Result<Config> {
    let has_env_auth = credential_env("CONTEXTSTREAM_API_KEY").is_some()
        || credential_env("CONTEXTSTREAM_JWT").is_some();
    let saved_credentials = if !has_env_auth || std::env::var("CONTEXTSTREAM_API_URL").is_err() {
        load_saved_credentials()
    } else {
        None
    };

    // Try to load saved credentials if env vars not set.
    if !has_env_auth {
        if let Some(saved) = saved_credentials.as_ref() {
            if !saved.api_key.trim().is_empty() {
                std::env::set_var("CONTEXTSTREAM_API_KEY", &saved.api_key);
            }
        }
    }

    // Prefer an explicit env override, then a source-built local dev binary,
    // then saved credentials, and finally the production default.
    let api_url = resolve_api_url(saved_credentials.as_ref());
    if std::env::var("CONTEXTSTREAM_API_URL").is_err() {
        std::env::set_var("CONTEXTSTREAM_API_URL", &api_url);
    }

    let api_key = credential_env("CONTEXTSTREAM_API_KEY");
    let jwt = credential_env("CONTEXTSTREAM_JWT");
    let allow_header_auth = parse_bool_env("CONTEXTSTREAM_ALLOW_HEADER_AUTH");

    // Check for credentials
    if require_credentials && api_key.is_none() && jwt.is_none() && !allow_header_auth {
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

    let toolset = enum_env::<Toolset>("CONTEXTSTREAM_TOOLSET", "light, standard or complete")
        .unwrap_or_default();

    let log_level = enum_env::<LogLevel>("CONTEXTSTREAM_LOG_LEVEL", "quiet, normal or verbose")
        .unwrap_or_default();

    let output_format =
        enum_env::<OutputFormat>("CONTEXTSTREAM_OUTPUT_FORMAT", "compact or pretty")
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

    let account_mode_preference =
        enum_env::<AccountModePreference>("CONTEXTSTREAM_ACCOUNT_MODE", "auto, team or personal")
            .unwrap_or_default();

    // Opt-in that re-enables caller-supplied local file paths on the HTTP
    // gateway. Only `allow` enables it; anything else (including unset) keeps
    // the hosted gateway from touching its own disk for caller-supplied paths.
    let http_local_files = std::env::var("CONTEXTSTREAM_HTTP_LOCAL_FILES")
        .map(|value| value.trim().eq_ignore_ascii_case("allow"))
        .unwrap_or(false);

    // Agent runs priced at most this many credits start without a quote. Unset, empty or not
    // a whole number of credits: 0, which always quotes. The variable's first name is read
    // when the current one is not set.
    let slipstream_auto_accept_max_credits =
        std::env::var(mcp_tools::domains::slipstream::AUTO_ACCEPT_ENV)
            .or_else(|_| std::env::var(mcp_tools::domains::slipstream::LEGACY_AUTO_ACCEPT_ENV))
            .ok()
            .and_then(|value| value.trim().parse::<u32>().ok())
            .unwrap_or(0);

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
        http_local_files,
        account_mode_preference,
        slipstream_auto_accept_max_credits,
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
    fn an_unknown_setting_value_is_reported_not_dropped() {
        // #319: CONTEXTSTREAM_TOOLSET=bogus served the default toolset silently.
        let (toolset, warning) = parse_enum_setting::<Toolset>(
            "CONTEXTSTREAM_TOOLSET",
            Some("bogus"),
            "light, standard or complete",
        );
        assert_eq!(toolset, None);
        assert_eq!(
            warning.as_deref(),
            Some("contextstream-mcp: CONTEXTSTREAM_TOOLSET=\"bogus\" is not one of light, standard or complete; using the default.")
        );
        let (toolset, warning) =
            parse_enum_setting::<Toolset>("CONTEXTSTREAM_TOOLSET", Some(" Light "), "");
        assert_eq!(toolset, Some(Toolset::Light));
        assert_eq!(warning, None);
        // Unset or blank is no value, and nothing to report.
        for raw in [None, Some(""), Some("  ")] {
            let (toolset, warning) =
                parse_enum_setting::<Toolset>("CONTEXTSTREAM_TOOLSET", raw, "");
            assert_eq!((toolset, warning), (None, None));
        }
    }

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
