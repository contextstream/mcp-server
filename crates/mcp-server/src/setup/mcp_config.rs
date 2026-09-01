//! MCP server configuration generation for editors.
//!
//! Generates the appropriate MCP config format for each editor type.

use super::credentials::{get_api_url, local_dev_api_url_override, normalize_api_url};
use super::safe_edit;
use anyhow::{Context, Result};
use mcp_client::json::parse_value_without_duplicate_keys;
use mcp_types::config::{DEFAULT_API_URL, VERSION};
use mcp_types::HARNESS_TEACHING_VERSION;
use serde_json::{json, Value};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use toml_edit::{DocumentMut, Item, Table};

use super::editors::Editor;

const DEFAULT_TOOLSET: &str = "complete";
const DEFAULT_OUTPUT_FORMAT: &str = "compact";
const DEFAULT_SEARCH_LIMIT: &str = "15";
const DEFAULT_SEARCH_MAX_CHARS: &str = "2400";
const DEFAULT_REMOTE_MCP_URL: &str = "https://mcp.contextstream.io/mcp";
const DEFAULT_REMOTE_CONTEXT_MODE: &str = "fast";
const DEFAULT_TRANSCRIPTS_ENABLED: bool = true;
const DEFAULT_HOOK_TRANSCRIPTS_ENABLED: bool = true;
const ENV_CONTEXT_PACK_ENABLED: &str = "CONTEXTSTREAM_CONTEXT_PACK";
const ENV_TRANSCRIPTS_ENABLED: &str = "CONTEXTSTREAM_TRANSCRIPTS_ENABLED";
const ENV_HOOK_TRANSCRIPTS_ENABLED: &str = "CONTEXTSTREAM_HOOK_TRANSCRIPTS_ENABLED";
const ENV_PROGRESSIVE_MODE: &str = "CONTEXTSTREAM_PROGRESSIVE_MODE";
const ENV_ROUTER_MODE: &str = "CONTEXTSTREAM_ROUTER_MODE";
const ENV_CONSOLIDATED_MODE: &str = "CONTEXTSTREAM_CONSOLIDATED";
const ENV_AUTO_HIDE_INTEGRATIONS: &str = "CONTEXTSTREAM_AUTO_HIDE_INTEGRATIONS";
const ENV_TOOLSET: &str = "CONTEXTSTREAM_TOOLSET";
const ENV_OUTPUT_FORMAT: &str = "CONTEXTSTREAM_OUTPUT_FORMAT";
const ENV_SEARCH_LIMIT: &str = "CONTEXTSTREAM_SEARCH_LIMIT";
const ENV_SEARCH_MAX_CHARS: &str = "CONTEXTSTREAM_SEARCH_MAX_CHARS";
const ENV_WORKSPACE_ID: &str = "CONTEXTSTREAM_WORKSPACE_ID";
const ENV_PROJECT_ID: &str = "CONTEXTSTREAM_PROJECT_ID";
const ENV_TOOL_SURFACE_PROFILE: &str = "CONTEXTSTREAM_TOOL_SURFACE_PROFILE";
const ENV_CLIENT_NAME: &str = "CONTEXTSTREAM_CLIENT";
const ENV_VSCODE_MCP_MODE: &str = "CONTEXTSTREAM_VSCODE_MCP_MODE";
const ENV_ACCOUNT_MODE: &str = "CONTEXTSTREAM_ACCOUNT_MODE";
const ENV_MANAGED_CONFIG_VERSION: &str = "CONTEXTSTREAM_MANAGED_CONFIG_VERSION";
const ENV_INSTALLATION_ID: &str = "CONTEXTSTREAM_INSTALLATION_ID";
const ENV_TEACHING_VERSION: &str = "CONTEXTSTREAM_TEACHING_VERSION";
const OPENCODE_CONFIG_SCHEMA_URL: &str = "https://opencode.ai/config.json";
const HEADER_CONTEXT_PACK_ENABLED: &str = "X-ContextStream-Context-Pack-Enabled";
const HEADER_TOOLSET: &str = "X-ContextStream-Toolset";
const HEADER_OUTPUT_FORMAT: &str = "X-ContextStream-Output-Format";
const HEADER_PROGRESSIVE_MODE: &str = "X-ContextStream-Progressive-Mode";
const HEADER_ROUTER_MODE: &str = "X-ContextStream-Router-Mode";
const HEADER_SEARCH_LIMIT: &str = "X-ContextStream-Search-Limit";
const HEADER_SEARCH_MAX_CHARS: &str = "X-ContextStream-Search-Max-Chars";
const HEADER_TRANSCRIPTS_ENABLED: &str = "X-ContextStream-Transcripts-Enabled";
const HEADER_HOOK_TRANSCRIPTS_ENABLED: &str = "X-ContextStream-Hook-Transcripts-Enabled";
const HEADER_WORKSPACE_ID: &str = "X-ContextStream-Workspace-Id";
const HEADER_PROJECT_ID: &str = "X-ContextStream-Project-Id";
const HEADER_CONSOLIDATED: &str = "X-ContextStream-Consolidated";
const HEADER_AUTO_HIDE_INTEGRATIONS: &str = "X-ContextStream-Auto-Hide-Integrations";
const HEADER_TOOL_SURFACE_PROFILE: &str = "X-ContextStream-Tool-Surface-Profile";
const HEADER_CLIENT_NAME: &str = "X-ContextStream-Client";
const HEADER_API_KEY: &str = "X-ContextStream-API-Key";
const HEADER_MANAGED_CONFIG_VERSION: &str = "X-ContextStream-Managed-Config-Version";
const HEADER_INSTALLATION_ID: &str = "X-ContextStream-Installation-Id";
const HEADER_TEACHING_VERSION: &str = "X-ContextStream-Teaching-Version";
pub const MANAGED_CONFIG_VERSION: &str = "2";
const RECOGNIZED_MANAGED_CONFIG_VERSIONS: &[&str] = &["1", MANAGED_CONFIG_VERSION];
const COPILOT_TOOL_SURFACE_PROFILE: &str = "openai_agentic";
const CODEX_MANAGED_COMMENT: &str = "# ContextStream MCP Server Configuration";
const CODEX_MANAGED_TRUST_COMMENT: &str = "# ContextStream managed project trust v1";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum VsCodeMcpMode {
    Auto,
    Remote,
    Local,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TransportMode {
    PreserveExisting,
    ForceRemote,
    ForceLocal,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct ManagedConfigIdentity {
    installation_id: Option<String>,
}

#[cfg(test)]
const TEST_MANAGED_INSTALLATION_ID: &str = "00000000-0000-4000-8000-000000000062";

#[cfg(test)]
thread_local! {
    // Editor-config unit tests exercise writer internals in parallel. They must
    // never create or inspect the developer's real installation state merely
    // because the production boundary attaches a durable managed identity.
    // The small number of persistence integration tests opt in explicitly
    // under the crate-wide environment mutex and an isolated HOME.
    static TEST_MANAGED_IDENTITY_PERSISTENCE: std::cell::Cell<bool> =
        const { std::cell::Cell::new(false) };
}

#[cfg(test)]
fn set_test_managed_identity_persistence(enabled: bool) {
    TEST_MANAGED_IDENTITY_PERSISTENCE.set(enabled);
}

impl ManagedConfigIdentity {
    fn for_preview() -> Result<Self> {
        let installation_id = mcp_client::activation::existing_installation_id()
            .context("Could not safely read ContextStream installation identity")?
            .map(|value| value.to_string());
        Ok(Self { installation_id })
    }

    fn for_write() -> Result<Self> {
        #[cfg(test)]
        if !safe_edit::is_dry_run() && !TEST_MANAGED_IDENTITY_PERSISTENCE.get() {
            return Ok(Self {
                installation_id: Some(TEST_MANAGED_INSTALLATION_ID.to_string()),
            });
        }

        let installation_id = if safe_edit::is_dry_run() {
            mcp_client::activation::existing_installation_id()
                .context("Could not safely read ContextStream installation identity")?
        } else {
            Some(
                mcp_client::activation::ensure_installation_id()
                    .context("Could not safely persist ContextStream installation identity")?,
            )
        };
        Ok(Self {
            installation_id: installation_id.map(|value| value.to_string()),
        })
    }
}

fn is_recognized_managed_config_version(value: &str) -> bool {
    RECOGNIZED_MANAGED_CONFIG_VERSIONS.contains(&value)
}

fn default_env_pairs(
    api_key: Option<&str>,
    workspace_id: Option<&str>,
    project_id: Option<&str>,
    default_transcripts_enabled: bool,
    default_hook_transcripts_enabled: bool,
) -> Vec<(&'static str, String)> {
    let mut pairs = vec![
        ("CONTEXTSTREAM_API_URL", get_api_url()),
        ("CONTEXTSTREAM_ALLOW_HEADER_AUTH", "false".to_string()),
        (
            "CONTEXTSTREAM_WORKSPACE_ID",
            workspace_id.unwrap_or_default().to_string(),
        ),
        (
            "CONTEXTSTREAM_PROJECT_ID",
            project_id.unwrap_or_default().to_string(),
        ),
        (
            "CONTEXTSTREAM_USER_AGENT",
            format!("contextstream-mcp-rust/{}", VERSION),
        ),
        (
            ENV_MANAGED_CONFIG_VERSION,
            MANAGED_CONFIG_VERSION.to_string(),
        ),
        ("CONTEXTSTREAM_TOOLSET", DEFAULT_TOOLSET.to_string()),
        ("CONTEXTSTREAM_LOG_LEVEL", "quiet".to_string()),
        (
            "CONTEXTSTREAM_OUTPUT_FORMAT",
            DEFAULT_OUTPUT_FORMAT.to_string(),
        ),
        ("CONTEXTSTREAM_CONTEXT_PACK", "true".to_string()),
        (ENV_ACCOUNT_MODE, "auto".to_string()),
        // Transcript capture defaults on for setup-generated configs.
        (
            ENV_TRANSCRIPTS_ENABLED,
            if default_transcripts_enabled {
                "true".to_string()
            } else {
                "false".to_string()
            },
        ),
        // Hook-based transcript persistence follows the same setup default.
        (
            ENV_HOOK_TRANSCRIPTS_ENABLED,
            if default_hook_transcripts_enabled {
                "true".to_string()
            } else {
                "false".to_string()
            },
        ),
        ("CONTEXTSTREAM_SHOW_TIMING", "false".to_string()),
        ("CONTEXTSTREAM_PROGRESSIVE_MODE", "false".to_string()),
        ("CONTEXTSTREAM_ROUTER_MODE", "false".to_string()),
        ("CONTEXTSTREAM_CONSOLIDATED", "true".to_string()),
        ("CONTEXTSTREAM_AUTO_HIDE_INTEGRATIONS", "true".to_string()),
        (
            "CONTEXTSTREAM_SEARCH_LIMIT",
            DEFAULT_SEARCH_LIMIT.to_string(),
        ),
        (
            "CONTEXTSTREAM_SEARCH_MAX_CHARS",
            DEFAULT_SEARCH_MAX_CHARS.to_string(),
        ),
        (
            "CONTEXTSTREAM_INCLUDE_STRUCTURED_CONTENT",
            "true".to_string(),
        ),
    ];

    if let Some(api_key) = api_key {
        pairs.insert(1, ("CONTEXTSTREAM_API_KEY", api_key.to_string()));
    }

    pairs
}

fn existing_env_string(existing_server: Option<&Value>, key: &str) -> Option<String> {
    existing_server
        .and_then(|server| server.get("env"))
        .and_then(|env| env.get(key))
        .and_then(Value::as_str)
        .map(String::from)
}

fn existing_server_string(existing_server: Option<&Value>, key: &str) -> Option<String> {
    existing_server
        .and_then(|server| server.get(key))
        .and_then(Value::as_str)
        .map(String::from)
}

fn existing_header_string(existing_server: Option<&Value>, key: &str) -> Option<String> {
    existing_server
        .and_then(|server| server.get("headers"))
        .and_then(|headers| headers.get(key))
        .and_then(Value::as_str)
        .map(String::from)
}

fn require_object_if_present(value: Option<&Value>, description: &str) -> Result<()> {
    if value.is_some_and(|value| !value.is_object()) {
        anyhow::bail!(
            "Refusing to modify {} because its existing value is not a JSON object.",
            description
        );
    }
    Ok(())
}

fn validate_existing_json_like_server(
    existing_server: Option<&Value>,
    description: &str,
) -> Result<()> {
    require_object_if_present(existing_server, description)?;
    let Some(server) = existing_server.and_then(Value::as_object) else {
        return Ok(());
    };
    for field in ["env", "environment", "headers"] {
        require_object_if_present(server.get(field), &format!("{}.{}", description, field))?;
    }
    if !json_like_server_is_contextstream_managed(existing_server.expect("server exists")) {
        anyhow::bail!(
            "Refusing to replace {} because the existing 'contextstream' entry is not \
             recognizably managed by ContextStream. Rename or remove that entry explicitly \
             before installing.",
            description
        );
    }
    Ok(())
}

fn path_token_basename(token: &str) -> &str {
    token
        .trim()
        .trim_end_matches(" (deleted)")
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or("")
}

fn is_contextstream_binary_token(token: &str) -> bool {
    matches!(
        path_token_basename(token),
        "contextstream-mcp" | "contextstream-mcp.exe"
    )
}

fn normalized_command_path(token: &str) -> String {
    token
        .trim()
        .trim_end_matches(" (deleted)")
        .replace('\\', "/")
}

/// Recognize legacy binary paths that the installer itself controlled.
///
/// A basename match is deliberately insufficient: a user may have their own
/// `/opt/tools/contextstream-mcp`. New configs carry an explicit ownership
/// marker; this path list exists only to migrate older generated configs.
fn is_known_legacy_contextstream_binary(token: &str) -> bool {
    if !is_contextstream_binary_token(token) {
        return false;
    }
    let normalized = normalized_command_path(token);
    let normalized_lower = normalized.to_ascii_lowercase();
    let managed = normalized_command_path(
        super::hooks::managed_binary_path()
            .to_string_lossy()
            .as_ref(),
    );
    if if cfg!(windows) {
        normalized.eq_ignore_ascii_case(&managed)
    } else {
        normalized == managed
    } {
        return true;
    }
    if std::env::current_exe().ok().is_some_and(|current| {
        let current = normalized_command_path(current.to_string_lossy().as_ref());
        if cfg!(windows) {
            normalized.eq_ignore_ascii_case(&current)
        } else {
            normalized == current
        }
    }) {
        return true;
    }

    normalized_lower.ends_with("/.contextstream/bin/contextstream-mcp")
        || normalized_lower.ends_with("/.contextstream/bin/contextstream-mcp.exe")
        || matches!(
            normalized_lower.as_str(),
            "/usr/local/bin/contextstream-mcp"
                | "/usr/bin/contextstream-mcp"
                | "/opt/homebrew/bin/contextstream-mcp"
        )
}

fn is_legacy_contextstream_package_token(token: &str) -> bool {
    matches!(
        token.trim(),
        "contextstream-mcp" | "@contextstream/mcp-server"
    )
}

fn command_and_args_are_contextstream_managed(command: &str, args: &[&str]) -> bool {
    if is_known_legacy_contextstream_binary(command) {
        return true;
    }
    match path_token_basename(command) {
        "cmd" | "cmd.exe" => matches!(
            args,
            [flag, binary, ..]
                if flag.eq_ignore_ascii_case("/c")
                    && is_known_legacy_contextstream_binary(binary)
        ),
        "npx" | "npx.cmd" => args
            .iter()
            .any(|argument| is_legacy_contextstream_package_token(argument)),
        _ => false,
    }
}

fn json_command_matches(server: &Value, predicate: impl Fn(&str, &[&str]) -> bool) -> bool {
    let args = server
        .get("args")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or(&[]);
    match server.get("command") {
        Some(Value::String(command)) => {
            let args = args.iter().map(Value::as_str).collect::<Option<Vec<_>>>();
            args.is_some_and(|args| predicate(command, args.as_slice()))
        }
        Some(Value::Array(command)) => {
            let strings = command
                .iter()
                .map(Value::as_str)
                .collect::<Option<Vec<_>>>();
            let Some(command) = strings else {
                return false;
            };
            command
                .split_first()
                .is_some_and(|(executable, args)| predicate(executable, args))
        }
        _ => false,
    }
}

fn json_command_is_contextstream_managed(server: &Value) -> bool {
    json_command_matches(server, command_and_args_are_contextstream_managed)
}

fn command_and_args_use_contextstream_binary(command: &str, args: &[&str]) -> bool {
    if is_contextstream_binary_token(command) {
        return true;
    }
    matches!(
        (path_token_basename(command), args),
        ("cmd" | "cmd.exe", [flag, binary, ..])
            if flag.eq_ignore_ascii_case("/c") && is_contextstream_binary_token(binary)
    )
}

fn json_command_uses_contextstream_binary(server: &Value) -> bool {
    json_command_matches(server, command_and_args_use_contextstream_binary)
}

fn json_server_has_legacy_contextstream_signature(server: &Value) -> bool {
    const LEGACY_ENV_KEYS: &[&str] = &[
        "CONTEXTSTREAM_API_URL",
        "CONTEXTSTREAM_USER_AGENT",
        "CONTEXTSTREAM_TOOLSET",
        "CONTEXTSTREAM_CLIENT",
        "CONTEXTSTREAM_API_KEY",
    ];
    ["env", "environment"].into_iter().any(|field| {
        server
            .get(field)
            .and_then(Value::as_object)
            .is_some_and(|values| LEGACY_ENV_KEYS.iter().any(|key| values.contains_key(*key)))
    })
}

fn is_default_contextstream_remote_url(url: &str) -> bool {
    url.trim()
        .split(['?', '#'])
        .next()
        .is_some_and(|base| base.trim_end_matches('/') == DEFAULT_REMOTE_MCP_URL)
}

fn object_has_recognized_managed_version(object: Option<&Value>, key: &str) -> bool {
    object
        .and_then(Value::as_object)
        .and_then(|values| values.get(key))
        .and_then(Value::as_str)
        .is_some_and(is_recognized_managed_config_version)
}

fn json_like_server_is_contextstream_managed(server: &Value) -> bool {
    if !server.is_object() {
        return false;
    }

    let explicitly_managed = ["env", "environment"].into_iter().any(|field| {
        object_has_recognized_managed_version(server.get(field), ENV_MANAGED_CONFIG_VERSION)
    }) || object_has_recognized_managed_version(
        server.get("headers"),
        HEADER_MANAGED_CONFIG_VERSION,
    );
    if explicitly_managed {
        return true;
    }

    if json_command_is_contextstream_managed(server) {
        return true;
    }
    if json_command_uses_contextstream_binary(server)
        && json_server_has_legacy_contextstream_signature(server)
    {
        return true;
    }

    server
        .get("url")
        .or_else(|| server.get("serverUrl"))
        .and_then(Value::as_str)
        .is_some_and(is_default_contextstream_remote_url)
}

fn editor_supports_hosted_remote(editor: &Editor) -> bool {
    !matches!(editor, Editor::Aider)
}

fn editor_defaults_to_hosted_remote(editor: &Editor) -> bool {
    editor_supports_hosted_remote(editor)
}

fn resolved_vscode_mcp_mode() -> VsCodeMcpMode {
    match std::env::var(ENV_VSCODE_MCP_MODE)
        .ok()
        .map(|value| value.trim().to_ascii_lowercase())
        .as_deref()
    {
        Some("remote") => VsCodeMcpMode::Remote,
        Some("local") => VsCodeMcpMode::Local,
        _ => VsCodeMcpMode::Auto,
    }
}

fn resolved_api_url(existing_server: Option<&Value>) -> String {
    std::env::var("CONTEXTSTREAM_API_URL")
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .map(|value| normalize_api_url(&value))
        .or_else(local_dev_api_url_override)
        .or_else(|| {
            existing_env_string(existing_server, "CONTEXTSTREAM_API_URL")
                .map(|value| normalize_api_url(&value))
        })
        .unwrap_or_else(get_api_url)
}

fn existing_json_like_server_is_remote(existing_server: Option<&Value>) -> bool {
    matches!(
        existing_server_string(existing_server, "type")
            .map(|value| value.to_ascii_lowercase()),
        Some(kind) if kind == "http" || kind == "remote"
    ) || existing_server
        .and_then(|server| server.get("url"))
        .is_some()
}

fn existing_json_like_server_is_local(existing_server: Option<&Value>) -> bool {
    matches!(
        existing_server_string(existing_server, "type")
            .map(|value| value.to_ascii_lowercase()),
        Some(kind) if kind == "stdio" || kind == "local"
    ) || existing_server
        .and_then(|server| server.get("command"))
        .is_some()
}

fn should_use_remote_http(
    editor: &Editor,
    existing_server: Option<&Value>,
    transport_mode: TransportMode,
) -> bool {
    if !editor_supports_hosted_remote(editor) {
        return false;
    }

    match transport_mode {
        TransportMode::ForceRemote => return true,
        TransportMode::ForceLocal => return false,
        TransportMode::PreserveExisting => {}
    }

    if existing_json_like_server_is_remote(existing_server) {
        return true;
    }
    if existing_json_like_server_is_local(existing_server) {
        return false;
    }

    if matches!(editor, Editor::Copilot | Editor::Cline | Editor::RooCode) {
        match resolved_vscode_mcp_mode() {
            VsCodeMcpMode::Remote => return true,
            VsCodeMcpMode::Local => return false,
            VsCodeMcpMode::Auto => {}
        }
    }

    editor_defaults_to_hosted_remote(editor)
}

fn build_merged_env_json(
    existing_server: Option<&Value>,
    api_key: Option<&str>,
    workspace_id: Option<&str>,
    project_id: Option<&str>,
    transcripts_enabled: Option<bool>,
    hook_transcripts_enabled: Option<bool>,
) -> Value {
    let resolved_api_url = resolved_api_url(existing_server);
    let resolved_workspace_id = workspace_id
        .filter(|value| !value.is_empty())
        .map(String::from)
        .or_else(|| existing_env_string(existing_server, "CONTEXTSTREAM_WORKSPACE_ID"))
        .unwrap_or_default();

    let resolved_project_id = project_id
        .filter(|value| !value.is_empty())
        .map(String::from)
        .or_else(|| existing_env_string(existing_server, "CONTEXTSTREAM_PROJECT_ID"))
        .unwrap_or_default();

    let mut env = match existing_server
        .and_then(|server| server.get("env"))
        .and_then(Value::as_object)
    {
        Some(existing_env) => existing_env.clone(),
        None => serde_json::Map::new(),
    };

    // When migrating an editor from hosted-remote back to local stdio, preserve
    // any existing remote header overrides by translating them into equivalent
    // local env values before defaults are applied.
    for (env_key, header_key) in [
        (ENV_CONTEXT_PACK_ENABLED, HEADER_CONTEXT_PACK_ENABLED),
        (ENV_TOOLSET, HEADER_TOOLSET),
        (ENV_OUTPUT_FORMAT, HEADER_OUTPUT_FORMAT),
        (ENV_PROGRESSIVE_MODE, HEADER_PROGRESSIVE_MODE),
        (ENV_ROUTER_MODE, HEADER_ROUTER_MODE),
        (ENV_SEARCH_LIMIT, HEADER_SEARCH_LIMIT),
        (ENV_SEARCH_MAX_CHARS, HEADER_SEARCH_MAX_CHARS),
        (ENV_TRANSCRIPTS_ENABLED, HEADER_TRANSCRIPTS_ENABLED),
        (
            ENV_HOOK_TRANSCRIPTS_ENABLED,
            HEADER_HOOK_TRANSCRIPTS_ENABLED,
        ),
        (ENV_WORKSPACE_ID, HEADER_WORKSPACE_ID),
        (ENV_PROJECT_ID, HEADER_PROJECT_ID),
        (ENV_CONSOLIDATED_MODE, HEADER_CONSOLIDATED),
        (ENV_AUTO_HIDE_INTEGRATIONS, HEADER_AUTO_HIDE_INTEGRATIONS),
        (ENV_TOOL_SURFACE_PROFILE, HEADER_TOOL_SURFACE_PROFILE),
    ] {
        if !env.contains_key(env_key) {
            if let Some(value) = existing_header_string(existing_server, header_key) {
                env.insert(env_key.to_string(), Value::String(value));
            }
        }
    }

    for (key, value) in default_env_pairs(
        api_key,
        Some(&resolved_workspace_id),
        Some(&resolved_project_id),
        transcripts_enabled.unwrap_or(DEFAULT_TRANSCRIPTS_ENABLED),
        hook_transcripts_enabled.unwrap_or(DEFAULT_HOOK_TRANSCRIPTS_ENABLED),
    ) {
        env.entry(key.to_string())
            .or_insert_with(|| Value::String(value));
    }

    // Always refresh these values on setup/update.
    env.insert(
        "CONTEXTSTREAM_API_URL".to_string(),
        Value::String(resolved_api_url),
    );
    if let Some(api_key) = api_key {
        env.insert(
            "CONTEXTSTREAM_API_KEY".to_string(),
            Value::String(api_key.to_string()),
        );
    } else {
        env.remove("CONTEXTSTREAM_API_KEY");
    }
    env.insert(
        "CONTEXTSTREAM_WORKSPACE_ID".to_string(),
        Value::String(resolved_workspace_id),
    );
    env.insert(
        "CONTEXTSTREAM_PROJECT_ID".to_string(),
        Value::String(resolved_project_id),
    );
    if let Some(enabled) = transcripts_enabled {
        env.insert(
            ENV_TRANSCRIPTS_ENABLED.to_string(),
            Value::String(enabled.to_string()),
        );
    }
    if let Some(enabled) = hook_transcripts_enabled {
        env.insert(
            ENV_HOOK_TRANSCRIPTS_ENABLED.to_string(),
            Value::String(enabled.to_string()),
        );
    }

    Value::Object(env)
}

fn build_contextstream_server_json(
    existing_server: Option<&Value>,
    api_key: Option<&str>,
    workspace_id: Option<&str>,
    project_id: Option<&str>,
    transcripts_enabled: Option<bool>,
    hook_transcripts_enabled: Option<bool>,
) -> Value {
    let (command, args) = resolved_command_and_args();
    let env = build_merged_env_json(
        existing_server,
        api_key,
        workspace_id,
        project_id,
        transcripts_enabled,
        hook_transcripts_enabled,
    );

    let mut server = existing_server.cloned().unwrap_or_else(|| json!({}));
    server["command"] = json!(command);
    server["args"] = json!(args);
    server["env"] = env;
    if let Some(obj) = server.as_object_mut() {
        obj.remove("type");
        obj.remove("url");
        obj.remove("headers");
    }
    server
}

fn resolved_remote_mcp_url(existing_server: Option<&Value>) -> String {
    std::env::var("CONTEXTSTREAM_MCP_HTTP_URL")
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .or_else(|| existing_server_string(existing_server, "url"))
        .unwrap_or_else(default_remote_mcp_url)
}

fn default_remote_mcp_url() -> String {
    format!("{DEFAULT_REMOTE_MCP_URL}?default_context_mode={DEFAULT_REMOTE_CONTEXT_MODE}")
}

fn build_remote_http_headers(
    existing_server: Option<&Value>,
    workspace_id: Option<&str>,
    project_id: Option<&str>,
    transcripts_enabled: Option<bool>,
    hook_transcripts_enabled: Option<bool>,
    remote_auth_api_key: Option<&str>,
) -> Value {
    let mut headers = serde_json::Map::new();
    headers.insert(
        HEADER_MANAGED_CONFIG_VERSION.to_string(),
        json!(MANAGED_CONFIG_VERSION),
    );
    headers.insert(HEADER_CONTEXT_PACK_ENABLED.to_string(), json!("true"));
    headers.insert(HEADER_TOOLSET.to_string(), json!(DEFAULT_TOOLSET));
    headers.insert(
        HEADER_OUTPUT_FORMAT.to_string(),
        json!(DEFAULT_OUTPUT_FORMAT),
    );
    headers.insert(HEADER_PROGRESSIVE_MODE.to_string(), json!("false"));
    headers.insert(HEADER_ROUTER_MODE.to_string(), json!("false"));
    headers.insert(HEADER_SEARCH_LIMIT.to_string(), json!(DEFAULT_SEARCH_LIMIT));
    headers.insert(
        HEADER_SEARCH_MAX_CHARS.to_string(),
        json!(DEFAULT_SEARCH_MAX_CHARS),
    );
    headers.insert(
        HEADER_TRANSCRIPTS_ENABLED.to_string(),
        json!(DEFAULT_TRANSCRIPTS_ENABLED.to_string()),
    );
    headers.insert(
        HEADER_HOOK_TRANSCRIPTS_ENABLED.to_string(),
        json!(DEFAULT_HOOK_TRANSCRIPTS_ENABLED.to_string()),
    );
    headers.insert(HEADER_CONSOLIDATED.to_string(), json!("true"));
    headers.insert(HEADER_AUTO_HIDE_INTEGRATIONS.to_string(), json!("true"));

    if let Some(existing) = existing_server
        .and_then(|server| server.get("headers"))
        .and_then(Value::as_object)
    {
        for (key, value) in existing {
            headers.insert(key.clone(), value.clone());
        }
    }

    for (env_key, header_key) in [
        (ENV_CONTEXT_PACK_ENABLED, HEADER_CONTEXT_PACK_ENABLED),
        (ENV_TOOLSET, HEADER_TOOLSET),
        (ENV_OUTPUT_FORMAT, HEADER_OUTPUT_FORMAT),
        (ENV_PROGRESSIVE_MODE, HEADER_PROGRESSIVE_MODE),
        (ENV_ROUTER_MODE, HEADER_ROUTER_MODE),
        (ENV_SEARCH_LIMIT, HEADER_SEARCH_LIMIT),
        (ENV_SEARCH_MAX_CHARS, HEADER_SEARCH_MAX_CHARS),
        (ENV_TRANSCRIPTS_ENABLED, HEADER_TRANSCRIPTS_ENABLED),
        (
            ENV_HOOK_TRANSCRIPTS_ENABLED,
            HEADER_HOOK_TRANSCRIPTS_ENABLED,
        ),
        (ENV_CONSOLIDATED_MODE, HEADER_CONSOLIDATED),
        (ENV_AUTO_HIDE_INTEGRATIONS, HEADER_AUTO_HIDE_INTEGRATIONS),
        (ENV_TOOL_SURFACE_PROFILE, HEADER_TOOL_SURFACE_PROFILE),
    ] {
        if existing_header_string(existing_server, header_key).is_none() {
            if let Some(value) = existing_env_string(existing_server, env_key) {
                headers.insert(header_key.to_string(), json!(value));
            }
        }
    }

    if let Some(value) = workspace_id
        .filter(|value| !value.is_empty())
        .map(String::from)
        .or_else(|| existing_header_string(existing_server, HEADER_WORKSPACE_ID))
        .or_else(|| existing_env_string(existing_server, ENV_WORKSPACE_ID))
    {
        headers.insert(HEADER_WORKSPACE_ID.to_string(), json!(value));
    }

    if let Some(value) = project_id
        .filter(|value| !value.is_empty())
        .map(String::from)
        .or_else(|| existing_header_string(existing_server, HEADER_PROJECT_ID))
        .or_else(|| existing_env_string(existing_server, ENV_PROJECT_ID))
    {
        headers.insert(HEADER_PROJECT_ID.to_string(), json!(value));
    }

    if let Some(enabled) = transcripts_enabled {
        headers.insert(
            HEADER_TRANSCRIPTS_ENABLED.to_string(),
            json!(enabled.to_string()),
        );
    }

    if let Some(enabled) = hook_transcripts_enabled {
        headers.insert(
            HEADER_HOOK_TRANSCRIPTS_ENABLED.to_string(),
            json!(enabled.to_string()),
        );
    }

    if existing_header_string(existing_server, HEADER_API_KEY).is_none() {
        if let Some(api_key) = remote_auth_api_key.filter(|value| !value.is_empty()) {
            headers.insert(HEADER_API_KEY.to_string(), json!(api_key));
        }
    }

    Value::Object(headers)
}

fn build_remote_http_server_json(
    existing_server: Option<&Value>,
    workspace_id: Option<&str>,
    project_id: Option<&str>,
    transcripts_enabled: Option<bool>,
    hook_transcripts_enabled: Option<bool>,
    remote_auth_api_key: Option<&str>,
) -> Value {
    let mut server = existing_server.cloned().unwrap_or_else(|| json!({}));
    server["type"] = json!("http");
    server["url"] = json!(resolved_remote_mcp_url(existing_server));
    server["headers"] = build_remote_http_headers(
        existing_server,
        workspace_id,
        project_id,
        transcripts_enabled,
        hook_transcripts_enabled,
        remote_auth_api_key,
    );

    if let Some(obj) = server.as_object_mut() {
        obj.remove("command");
        obj.remove("args");
        obj.remove("env");
    }

    server
}

fn default_tool_surface_profile_for_editor(editor: &Editor) -> Option<&'static str> {
    if matches!(editor, Editor::Copilot) {
        Some(COPILOT_TOOL_SURFACE_PROFILE)
    } else {
        None
    }
}

fn apply_managed_config_metadata(
    editor: &Editor,
    server: &mut Value,
    identity: &ManagedConfigIdentity,
) {
    if matches!(
        server.get("type").and_then(Value::as_str),
        Some("http" | "remote")
    ) && server.get("headers").is_none()
    {
        server["headers"] = json!({});
    }
    if let Some(headers) = server.get_mut("headers").and_then(Value::as_object_mut) {
        headers.insert(
            HEADER_MANAGED_CONFIG_VERSION.to_string(),
            json!(MANAGED_CONFIG_VERSION),
        );
        headers.insert(HEADER_CLIENT_NAME.to_string(), json!(editor.id()));
        headers.insert(
            HEADER_TEACHING_VERSION.to_string(),
            json!(HARNESS_TEACHING_VERSION),
        );
        if let Some(installation_id) = identity.installation_id.as_deref() {
            headers.insert(HEADER_INSTALLATION_ID.to_string(), json!(installation_id));
        }
        if let Some(profile) = default_tool_surface_profile_for_editor(editor) {
            headers
                .entry(HEADER_TOOL_SURFACE_PROFILE.to_string())
                .or_insert_with(|| json!(profile));
        }
    }

    if let Some(env) = server.get_mut("env").and_then(Value::as_object_mut) {
        env.insert(
            ENV_MANAGED_CONFIG_VERSION.to_string(),
            json!(MANAGED_CONFIG_VERSION),
        );
        env.insert(ENV_CLIENT_NAME.to_string(), json!(editor.id()));
        env.insert(
            ENV_TEACHING_VERSION.to_string(),
            json!(HARNESS_TEACHING_VERSION),
        );
        if let Some(installation_id) = identity.installation_id.as_deref() {
            env.insert(ENV_INSTALLATION_ID.to_string(), json!(installation_id));
        }
        if let Some(profile) = default_tool_surface_profile_for_editor(editor) {
            env.entry(ENV_TOOL_SURFACE_PROFILE.to_string())
                .or_insert_with(|| json!(profile));
        }
    }

    if let Some(environment) = server.get_mut("environment").and_then(Value::as_object_mut) {
        environment.insert(
            ENV_MANAGED_CONFIG_VERSION.to_string(),
            json!(MANAGED_CONFIG_VERSION),
        );
        environment.insert(ENV_CLIENT_NAME.to_string(), json!(editor.id()));
        environment.insert(
            ENV_TEACHING_VERSION.to_string(),
            json!(HARNESS_TEACHING_VERSION),
        );
        if let Some(installation_id) = identity.installation_id.as_deref() {
            environment.insert(ENV_INSTALLATION_ID.to_string(), json!(installation_id));
        }
        if let Some(profile) = default_tool_surface_profile_for_editor(editor) {
            environment
                .entry(ENV_TOOL_SURFACE_PROFILE.to_string())
                .or_insert_with(|| json!(profile));
        }
    }
}

fn resolved_command_and_args() -> (String, Vec<String>) {
    let managed = super::hooks::managed_binary_path();
    // A dry run does not materialize the helper binary; nevertheless its
    // config preview must match the command path a real run would write.
    if safe_edit::is_dry_run() || managed.exists() {
        if let Some(path) = sanitize_command_path(&managed) {
            return (path, vec![]);
        }
    }

    if cfg!(windows) {
        (
            "cmd".to_string(),
            vec!["/c".to_string(), "contextstream-mcp".to_string()],
        )
    } else {
        (
            std::env::current_exe()
                .ok()
                .and_then(|path| sanitize_command_path(&path))
                .unwrap_or_else(|| "contextstream-mcp".to_string()),
            vec![],
        )
    }
}

fn sanitize_command_path(path: &Path) -> Option<String> {
    let raw = path.to_string_lossy();
    if raw.is_empty() {
        return None;
    }

    if let Some(stripped) = raw.strip_suffix(" (deleted)") {
        if Path::new(stripped).exists() {
            return Some(stripped.to_string());
        }
        return None;
    }

    Some(raw.to_string())
}

fn toml_escape(value: &str) -> String {
    value.replace('\\', "\\\\").replace('"', "\\\"")
}

/// Build the standard MCP server JSON configuration.
#[cfg(test)]
fn build_mcp_server_json(api_key: &str) -> Value {
    let server = build_contextstream_server_json(None, Some(api_key), None, None, None, None);

    json!({
        "mcpServers": {
            "contextstream": server
        }
    })
}

/// Build VS Code extension MCP server configuration.
pub fn _build_vscode_server_json(api_key: &str, extension_key: &str) -> Value {
    let (command, args) = resolved_command_and_args();
    let mut env = serde_json::Map::new();
    for (key, value) in default_env_pairs(
        Some(api_key),
        None,
        None,
        DEFAULT_TRANSCRIPTS_ENABLED,
        DEFAULT_HOOK_TRANSCRIPTS_ENABLED,
    ) {
        env.insert(key.to_string(), Value::String(value));
    }

    json!({
        extension_key: {
            "mcpServers": {
                "contextstream": {
                    "command": command,
                    "args": args,
                    "env": Value::Object(env)
                }
            }
        }
    })
}

/// Build Codex TOML configuration.
#[cfg(test)]
fn build_codex_toml_config(api_key: &str) -> String {
    build_codex_toml_config_with_context(api_key, None, None, None, None)
}

#[cfg(test)]
fn build_codex_toml_config_with_context(
    api_key: &str,
    workspace_id: Option<&str>,
    project_id: Option<&str>,
    transcripts_enabled: Option<bool>,
    hook_transcripts_enabled: Option<bool>,
) -> String {
    let (command, args) = resolved_command_and_args();
    let command = toml_escape(&command);
    let args = args
        .into_iter()
        .map(|arg| format!(r#""{}""#, toml_escape(&arg)))
        .collect::<Vec<_>>()
        .join(", ");

    let mut content = format!(
        "# ContextStream MCP Server Configuration\n[mcp_servers.contextstream]\ncommand = \"{}\"\nargs = [{}]\n\n[mcp_servers.contextstream.env]\n",
        command, args
    );

    for (key, value) in default_env_pairs(
        Some(api_key),
        workspace_id,
        project_id,
        transcripts_enabled.unwrap_or(DEFAULT_TRANSCRIPTS_ENABLED),
        hook_transcripts_enabled.unwrap_or(DEFAULT_HOOK_TRANSCRIPTS_ENABLED),
    ) {
        content.push_str(&format!(r#"{} = "{}""#, key, toml_escape(&value)));
        content.push('\n');
    }

    content
}

fn build_codex_local_toml_config_preserving_existing(
    existing_content: Option<&str>,
    api_key: &str,
    workspace_id: Option<&str>,
    project_id: Option<&str>,
    transcripts_enabled: Option<bool>,
    hook_transcripts_enabled: Option<bool>,
    identity: &ManagedConfigIdentity,
) -> String {
    let (command, args) = resolved_command_and_args();
    let command = toml_escape(&command);
    let args = args
        .into_iter()
        .map(|arg| format!(r#""{}""#, toml_escape(&arg)))
        .collect::<Vec<_>>()
        .join(", ");

    let mut env_map = existing_content
        .map(extract_codex_env_map)
        .unwrap_or_default();

    for (key, value) in default_env_pairs(
        Some(api_key),
        workspace_id,
        project_id,
        transcripts_enabled.unwrap_or(DEFAULT_TRANSCRIPTS_ENABLED),
        hook_transcripts_enabled.unwrap_or(DEFAULT_HOOK_TRANSCRIPTS_ENABLED),
    ) {
        env_map.entry(key.to_string()).or_insert(value);
    }

    let resolved_api_url = std::env::var("CONTEXTSTREAM_API_URL")
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .map(|value| normalize_api_url(&value))
        .or_else(local_dev_api_url_override)
        .or_else(|| {
            existing_content
                .and_then(|content| extract_codex_env_value(content, "CONTEXTSTREAM_API_URL"))
                .map(|value| normalize_api_url(&value))
        })
        .unwrap_or_else(get_api_url);

    env_map.insert("CONTEXTSTREAM_API_URL".to_string(), resolved_api_url);
    env_map.insert("CONTEXTSTREAM_API_KEY".to_string(), api_key.to_string());
    env_map.insert(
        ENV_MANAGED_CONFIG_VERSION.to_string(),
        MANAGED_CONFIG_VERSION.to_string(),
    );
    env_map.insert(ENV_CLIENT_NAME.to_string(), Editor::Codex.id().to_string());
    env_map.insert(
        ENV_TEACHING_VERSION.to_string(),
        HARNESS_TEACHING_VERSION.to_string(),
    );
    if let Some(installation_id) = identity.installation_id.as_deref() {
        env_map.insert(ENV_INSTALLATION_ID.to_string(), installation_id.to_string());
    }

    if let Some(value) = workspace_id.filter(|value| !value.is_empty()) {
        env_map.insert("CONTEXTSTREAM_WORKSPACE_ID".to_string(), value.to_string());
    }
    if let Some(value) = project_id.filter(|value| !value.is_empty()) {
        env_map.insert("CONTEXTSTREAM_PROJECT_ID".to_string(), value.to_string());
    }
    if let Some(enabled) = transcripts_enabled {
        env_map.insert(ENV_TRANSCRIPTS_ENABLED.to_string(), enabled.to_string());
    }
    if let Some(enabled) = hook_transcripts_enabled {
        env_map.insert(
            ENV_HOOK_TRANSCRIPTS_ENABLED.to_string(),
            enabled.to_string(),
        );
    }

    let mut content = format!(
        "# ContextStream MCP Server Configuration\n[mcp_servers.contextstream]\ncommand = \"{}\"\nargs = [{}]\n\n[mcp_servers.contextstream.env]\n",
        command, args
    );

    for (key, value) in env_map {
        content.push_str(&format!(r#"{} = "{}""#, key, toml_escape(&value)));
        content.push('\n');
    }

    content
}

fn build_codex_remote_http_headers(
    existing_content: Option<&str>,
    workspace_id: Option<&str>,
    project_id: Option<&str>,
    transcripts_enabled: Option<bool>,
    hook_transcripts_enabled: Option<bool>,
    remote_auth_api_key: Option<&str>,
    identity: &ManagedConfigIdentity,
) -> std::collections::BTreeMap<String, String> {
    let mut headers = std::collections::BTreeMap::new();
    headers.insert(
        HEADER_MANAGED_CONFIG_VERSION.to_string(),
        MANAGED_CONFIG_VERSION.to_string(),
    );
    headers.insert(HEADER_CONTEXT_PACK_ENABLED.to_string(), "true".to_string());
    headers.insert(HEADER_TOOLSET.to_string(), DEFAULT_TOOLSET.to_string());
    headers.insert(
        HEADER_OUTPUT_FORMAT.to_string(),
        DEFAULT_OUTPUT_FORMAT.to_string(),
    );
    headers.insert(HEADER_PROGRESSIVE_MODE.to_string(), "false".to_string());
    headers.insert(HEADER_ROUTER_MODE.to_string(), "false".to_string());
    headers.insert(
        HEADER_SEARCH_LIMIT.to_string(),
        DEFAULT_SEARCH_LIMIT.to_string(),
    );
    headers.insert(
        HEADER_SEARCH_MAX_CHARS.to_string(),
        DEFAULT_SEARCH_MAX_CHARS.to_string(),
    );
    headers.insert(
        HEADER_TRANSCRIPTS_ENABLED.to_string(),
        DEFAULT_TRANSCRIPTS_ENABLED.to_string(),
    );
    headers.insert(
        HEADER_HOOK_TRANSCRIPTS_ENABLED.to_string(),
        DEFAULT_HOOK_TRANSCRIPTS_ENABLED.to_string(),
    );
    headers.insert(HEADER_CONSOLIDATED.to_string(), "true".to_string());
    headers.insert(
        HEADER_AUTO_HIDE_INTEGRATIONS.to_string(),
        "true".to_string(),
    );
    headers.insert(
        HEADER_CLIENT_NAME.to_string(),
        Editor::Codex.id().to_string(),
    );

    let existing_http_headers = existing_content
        .map(|existing| extract_codex_contextstream_inline_table(existing, "http_headers"))
        .unwrap_or_default();

    for (key, value) in &existing_http_headers {
        headers.insert(key.clone(), value.clone());
    }

    for (env_key, header_key) in [
        (ENV_CONTEXT_PACK_ENABLED, HEADER_CONTEXT_PACK_ENABLED),
        (ENV_TOOLSET, HEADER_TOOLSET),
        (ENV_OUTPUT_FORMAT, HEADER_OUTPUT_FORMAT),
        (ENV_PROGRESSIVE_MODE, HEADER_PROGRESSIVE_MODE),
        (ENV_ROUTER_MODE, HEADER_ROUTER_MODE),
        (ENV_SEARCH_LIMIT, HEADER_SEARCH_LIMIT),
        (ENV_SEARCH_MAX_CHARS, HEADER_SEARCH_MAX_CHARS),
        (ENV_TRANSCRIPTS_ENABLED, HEADER_TRANSCRIPTS_ENABLED),
        (
            ENV_HOOK_TRANSCRIPTS_ENABLED,
            HEADER_HOOK_TRANSCRIPTS_ENABLED,
        ),
        (ENV_CONSOLIDATED_MODE, HEADER_CONSOLIDATED),
        (ENV_AUTO_HIDE_INTEGRATIONS, HEADER_AUTO_HIDE_INTEGRATIONS),
        (ENV_TOOL_SURFACE_PROFILE, HEADER_TOOL_SURFACE_PROFILE),
    ] {
        if !existing_http_headers.contains_key(header_key) {
            if let Some(value) =
                existing_content.and_then(|content| extract_codex_env_value(content, env_key))
            {
                headers.insert(header_key.to_string(), value);
            }
        }
    }

    if let Some(value) = workspace_id
        .filter(|value| !value.is_empty())
        .map(String::from)
        .or_else(|| {
            existing_content.and_then(|content| {
                extract_codex_contextstream_http_header(content, HEADER_WORKSPACE_ID)
            })
        })
        .or_else(|| {
            existing_content.and_then(|content| extract_codex_env_value(content, ENV_WORKSPACE_ID))
        })
    {
        headers.insert(HEADER_WORKSPACE_ID.to_string(), value);
    }

    if let Some(value) = project_id
        .filter(|value| !value.is_empty())
        .map(String::from)
        .or_else(|| {
            existing_content.and_then(|content| {
                extract_codex_contextstream_http_header(content, HEADER_PROJECT_ID)
            })
        })
        .or_else(|| {
            existing_content.and_then(|content| extract_codex_env_value(content, ENV_PROJECT_ID))
        })
    {
        headers.insert(HEADER_PROJECT_ID.to_string(), value);
    }

    if let Some(enabled) = transcripts_enabled {
        headers.insert(HEADER_TRANSCRIPTS_ENABLED.to_string(), enabled.to_string());
    }

    if let Some(enabled) = hook_transcripts_enabled {
        headers.insert(
            HEADER_HOOK_TRANSCRIPTS_ENABLED.to_string(),
            enabled.to_string(),
        );
    }

    if let Some(api_key) = remote_auth_api_key.filter(|value| !value.is_empty()) {
        headers.insert("Authorization".to_string(), format!("Bearer {}", api_key));
    }
    headers.insert(
        HEADER_MANAGED_CONFIG_VERSION.to_string(),
        MANAGED_CONFIG_VERSION.to_string(),
    );
    headers.insert(
        HEADER_CLIENT_NAME.to_string(),
        Editor::Codex.id().to_string(),
    );
    headers.insert(
        HEADER_TEACHING_VERSION.to_string(),
        HARNESS_TEACHING_VERSION.to_string(),
    );
    if let Some(installation_id) = identity.installation_id.as_deref() {
        headers.insert(
            HEADER_INSTALLATION_ID.to_string(),
            installation_id.to_string(),
        );
    }

    headers
}

fn toml_inline_table(entries: &std::collections::BTreeMap<String, String>) -> String {
    let items = entries
        .iter()
        .map(|(key, value)| format!(r#""{}" = "{}""#, toml_escape(key), toml_escape(value)))
        .collect::<Vec<_>>()
        .join(", ");
    format!("{{ {} }}", items)
}

fn build_codex_remote_toml_config(
    existing_content: Option<&str>,
    url: &str,
    workspace_id: Option<&str>,
    project_id: Option<&str>,
    transcripts_enabled: Option<bool>,
    hook_transcripts_enabled: Option<bool>,
    remote_auth_api_key: Option<&str>,
    identity: &ManagedConfigIdentity,
) -> String {
    let headers = build_codex_remote_http_headers(
        existing_content,
        workspace_id,
        project_id,
        transcripts_enabled,
        hook_transcripts_enabled,
        remote_auth_api_key,
        identity,
    );

    let mut content = format!(
        "# ContextStream MCP Server Configuration\n[mcp_servers.contextstream]\nurl = \"{}\"\n",
        toml_escape(url)
    );

    if let Some(value) = existing_content
        .and_then(|raw| extract_codex_contextstream_value(raw, "bearer_token_env_var"))
        .filter(|_| remote_auth_api_key.is_none())
    {
        content.push_str(&format!(
            r#"bearer_token_env_var = "{}""#,
            toml_escape(&value)
        ));
        content.push('\n');
    }

    if !headers.is_empty() {
        content.push_str(&format!("http_headers = {}\n", toml_inline_table(&headers)));
    }

    let env_http_headers = existing_content
        .map(|raw| extract_codex_contextstream_inline_table(raw, "env_http_headers"))
        .unwrap_or_default();
    if !env_http_headers.is_empty() && remote_auth_api_key.is_none() {
        content.push_str(&format!(
            "env_http_headers = {}\n",
            toml_inline_table(&env_http_headers)
        ));
    }

    content
}

pub fn editor_supports_remote_mcp(editor: &Editor) -> bool {
    editor_supports_hosted_remote(editor)
}

/// Generate MCP config JSON for an editor without writing any files.
///
/// Returns a JSON object with:
/// - `editor`: editor ID
/// - `editor_name`: display name
/// - `config_path`: target config file path
/// - `config_format`: "json" | "toml" | "yaml" | "vscode_settings"
/// - `transport`: "remote" | "local"
/// - `server_config`: the server configuration object
/// - `supports_remote`: whether this editor supports hosted remote
/// - `supports_project_config`: whether project-level config is supported
/// - `project_config_path`: project config path (if applicable)
/// - `rules_path`: global rules file path
///
/// Used by the remote-first install script as a fallback when
/// server-side config generation is unavailable.
pub fn generate_config_json(
    editor: &Editor,
    api_key: &str,
    workspace_id: Option<&str>,
    project_id: Option<&str>,
    transport: &str,
    remote_auth_api_key: Option<&str>,
) -> Result<Value> {
    let identity = ManagedConfigIdentity::for_preview()?;
    Ok(generate_config_json_with_identity(
        editor,
        api_key,
        workspace_id,
        project_id,
        transport,
        remote_auth_api_key,
        &identity,
    ))
}

fn generate_config_json_with_identity(
    editor: &Editor,
    api_key: &str,
    workspace_id: Option<&str>,
    project_id: Option<&str>,
    transport: &str,
    remote_auth_api_key: Option<&str>,
    identity: &ManagedConfigIdentity,
) -> Value {
    let supports_remote = editor_supports_hosted_remote(editor);
    let requested_local = matches!(
        transport.trim().to_ascii_lowercase().as_str(),
        "local" | "binary" | "local-binary"
    );
    let use_remote = supports_remote && (!requested_local || !super::local_mcp_allowed());

    let mut server_config = if use_remote {
        build_remote_http_server_json(
            None,
            workspace_id,
            project_id,
            None,
            None,
            remote_auth_api_key,
        )
    } else {
        build_contextstream_server_json(None, Some(api_key), workspace_id, project_id, None, None)
    };
    apply_managed_config_metadata(editor, &mut server_config, identity);

    let config_path = editor
        .mcp_config_path()
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_default();

    let config_format = match editor {
        Editor::Codex => "toml",
        Editor::Aider => "yaml",
        Editor::Cline | Editor::RooCode => "vscode_settings",
        _ if editor.uses_json_config() => "json",
        _ => "json",
    };

    let vscode_settings_key = match editor {
        Editor::Cline => Some("cline.mcpServers"),
        Editor::RooCode => Some("roo-cline.mcpServers"),
        _ => None,
    };

    let rules_path = editor
        .rules_path(None)
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_default();

    let project_config_path = std::env::current_dir()
        .ok()
        .and_then(|cwd| editor.project_mcp_config_path(&cwd))
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_default();

    let mut result = json!({
        "editor": editor.id(),
        "editor_name": editor.display_name(),
        "config_path": config_path,
        "config_format": config_format,
        "transport": if use_remote { "remote" } else { "local" },
        "server_config": server_config,
        "supports_remote": supports_remote,
        "supports_project_config": editor.supports_project_mcp_config(),
        "project_config_path": project_config_path,
        "rules_path": rules_path,
    });

    if let Some(key) = vscode_settings_key {
        result["vscode_settings_key"] = json!(key);
    }

    result
}

/// Generate configs for all specified editors as a single JSON payload.
///
/// Returns `{ "editors": [ ... ], "credentials_path": "...", "transport_marker_path": "..." }`
pub fn generate_all_configs_json(
    editors: &[Editor],
    api_key: &str,
    workspace_id: Option<&str>,
    project_id: Option<&str>,
    transport: &str,
    remote_auth_api_key: Option<&str>,
) -> Result<Value> {
    let identity = ManagedConfigIdentity::for_preview()?;
    let configs: Vec<Value> = editors
        .iter()
        .map(|editor| {
            generate_config_json_with_identity(
                editor,
                api_key,
                workspace_id,
                project_id,
                transport,
                remote_auth_api_key,
                &identity,
            )
        })
        .collect();

    Ok(json!({
        "editors": configs,
        "credentials_path": super::credentials::credentials_file_path().to_string_lossy().to_string(),
        "transport_marker_path": super::credentials::contextstream_config_dir().join("setup-transport-mode").to_string_lossy().to_string(),
        "config_dir": super::credentials::contextstream_config_dir().to_string_lossy().to_string(),
        "version": VERSION,
        "remote_mcp_url": DEFAULT_REMOTE_MCP_URL,
    }))
}

pub fn repair_deleted_binary_path_configs(
    detected: &[Editor],
    project_path: Option<&Path>,
) -> Result<usize> {
    let mut repaired = 0usize;
    let mut seen_paths = HashSet::new();

    for editor in detected {
        // Rules-only integrations may expose a path used for another config
        // format (Aider uses YAML), but they have no MCP command to repair.
        // Never feed those user-owned files into the JSON/TOML repair path.
        if !editor.has_mcp_transport() {
            continue;
        }
        if let Some(path) = editor.mcp_config_path().filter(|path| path.exists()) {
            if seen_paths.insert(path.clone())
                && repair_deleted_binary_path_in_file(&path, Some(editor))?
            {
                repaired += 1;
            }
        }

        if let Some(project_path) = project_path {
            if let Some(path) = editor
                .project_mcp_config_path(project_path)
                .filter(|path| path.exists())
            {
                if seen_paths.insert(path.clone())
                    && repair_deleted_binary_path_in_file(&path, Some(editor))?
                {
                    repaired += 1;
                }
            }
        }
    }

    Ok(repaired)
}

fn repair_deleted_binary_path_in_file(path: &Path, editor: Option<&Editor>) -> Result<bool> {
    let path_exists = path
        .try_exists()
        .with_context(|| format!("Could not inspect Codex config {}", path.display()))?;
    if !path_exists {
        return Ok(false);
    }

    if matches!(editor, Some(Editor::Codex)) {
        return repair_deleted_binary_path_in_codex_toml(path);
    }

    let dialect = if path.extension().and_then(|extension| extension.to_str()) == Some("jsonc") {
        safe_edit::JsonDialect::Jsonc
    } else {
        safe_edit::JsonDialect::Strict
    };
    let loaded = safe_edit::read_for_edit(path, dialect)?;
    let mut updated = loaded.value.clone();
    let root_keys: &[&str] = match editor {
        Some(Editor::Copilot) => &["servers"],
        Some(Editor::OpenCode | Editor::KiloCode) => &["mcp"],
        Some(Editor::Cline) => &["cline.mcpServers"],
        Some(Editor::RooCode) => &["roo-cline.mcpServers"],
        _ => &["mcpServers", "servers", "mcp"],
    };

    let mut changed = false;
    for root_key in root_keys {
        if let Some(server) = updated
            .get_mut(*root_key)
            .and_then(Value::as_object_mut)
            .and_then(|servers| servers.get_mut("contextstream"))
        {
            if json_like_server_is_contextstream_managed(server) {
                changed |= repair_deleted_binary_strings(server);
            }
        }
    }
    if !changed {
        return Ok(false);
    }

    safe_edit::commit(path, &loaded, &updated)?;
    Ok(true)
}

fn repair_deleted_binary_strings(value: &mut Value) -> bool {
    let Some(command) = value.get_mut("command") else {
        return false;
    };
    repair_deleted_command_value(command)
}

fn repair_deleted_command_value(value: &mut Value) -> bool {
    match value {
        Value::String(string) => {
            let Some(without_suffix) = string.strip_suffix(" (deleted)") else {
                return false;
            };
            let executable = Path::new(without_suffix)
                .file_name()
                .and_then(|name| name.to_str());
            if !matches!(
                executable,
                Some("contextstream-mcp" | "contextstream-mcp.exe")
            ) {
                return false;
            }
            string.truncate(without_suffix.len());
            true
        }
        Value::Array(items) => {
            let mut changed = false;
            for item in items {
                // Visit every candidate. `Iterator::any` would short-circuit
                // after the first repair and leave later command paths stale.
                changed = repair_deleted_command_value(item) || changed;
            }
            changed
        }
        _ => false,
    }
}

fn repair_deleted_binary_path_in_codex_toml(path: &Path) -> Result<bool> {
    let content = std::fs::read_to_string(path)
        .with_context(|| format!("Could not read Codex config {}", path.display()))?;
    let mut document = parse_codex_toml(&content, path)?;
    let Some(server) = document
        .get("mcp_servers")
        .and_then(Item::as_table_like)
        .and_then(|servers| servers.get("contextstream"))
    else {
        return Ok(false);
    };
    if !toml_item_is_contextstream_managed(server) {
        return Ok(false);
    }
    let Some(command_item) = document
        .get_mut("mcp_servers")
        .and_then(Item::as_table_like_mut)
        .and_then(|servers| servers.get_mut("contextstream"))
        .and_then(Item::as_table_like_mut)
        .and_then(|server| server.get_mut("command"))
    else {
        return Ok(false);
    };
    let Some(command) = command_item.as_value().and_then(toml_edit::Value::as_str) else {
        return Ok(false);
    };
    let Some(repaired) = command.strip_suffix(" (deleted)") else {
        return Ok(false);
    };
    let executable = Path::new(repaired)
        .file_name()
        .and_then(|name| name.to_str());
    if !matches!(
        executable,
        Some("contextstream-mcp" | "contextstream-mcp.exe")
    ) {
        return Ok(false);
    }

    let existing_decor = command_item
        .as_value()
        .expect("command was just read as a TOML value")
        .decor()
        .clone();
    let mut replacement = toml_edit::value(repaired);
    *replacement
        .as_value_mut()
        .expect("toml_edit::value produces a value")
        .decor_mut() = existing_decor;
    *command_item = replacement;

    let output = render_codex_toml(&document, &content)?;
    safe_edit::write_if_unchanged(path, &output, Some(&content))?;
    Ok(true)
}

#[cfg(test)]
fn build_json_like_server_for_editor(
    editor: &Editor,
    existing_server: Option<&Value>,
    api_key: Option<&str>,
    workspace_id: Option<&str>,
    project_id: Option<&str>,
    transcripts_enabled: Option<bool>,
    hook_transcripts_enabled: Option<bool>,
    transport_mode: TransportMode,
    remote_auth_api_key: Option<&str>,
) -> Result<Value> {
    build_json_like_server_for_editor_with_identity(
        editor,
        existing_server,
        api_key,
        workspace_id,
        project_id,
        transcripts_enabled,
        hook_transcripts_enabled,
        transport_mode,
        remote_auth_api_key,
        &ManagedConfigIdentity::default(),
    )
}

#[allow(clippy::too_many_arguments)]
fn build_json_like_server_for_editor_with_identity(
    editor: &Editor,
    existing_server: Option<&Value>,
    api_key: Option<&str>,
    workspace_id: Option<&str>,
    project_id: Option<&str>,
    transcripts_enabled: Option<bool>,
    hook_transcripts_enabled: Option<bool>,
    transport_mode: TransportMode,
    remote_auth_api_key: Option<&str>,
    identity: &ManagedConfigIdentity,
) -> Result<Value> {
    validate_existing_json_like_server(existing_server, "ContextStream MCP server entry")?;
    let mut server = if should_use_remote_http(editor, existing_server, transport_mode) {
        build_remote_http_server_json(
            existing_server,
            workspace_id,
            project_id,
            transcripts_enabled,
            hook_transcripts_enabled,
            remote_auth_api_key,
        )
    } else {
        build_contextstream_server_json(
            existing_server,
            api_key,
            workspace_id,
            project_id,
            transcripts_enabled,
            hook_transcripts_enabled,
        )
    };
    apply_managed_config_metadata(editor, &mut server, identity);
    Ok(server)
}

/// Write MCP configuration for an editor.
#[allow(dead_code)]
pub fn write_mcp_config(
    editor: &Editor,
    api_key: &str,
    workspace_id: Option<&str>,
    project_id: Option<&str>,
) -> Result<()> {
    write_mcp_config_with_remote_auth(editor, api_key, workspace_id, project_id, None, None, None)
}

/// Write MCP configuration for an editor, overriding transcript defaults when provided.
pub fn write_mcp_config_with_overrides(
    editor: &Editor,
    api_key: &str,
    workspace_id: Option<&str>,
    project_id: Option<&str>,
    transcripts_enabled: Option<bool>,
    hook_transcripts_enabled: Option<bool>,
) -> Result<()> {
    write_mcp_config_with_remote_auth(
        editor,
        api_key,
        workspace_id,
        project_id,
        transcripts_enabled,
        hook_transcripts_enabled,
        None,
    )
}

/// Write MCP configuration for setup flows, optionally embedding an API key into
/// generated remote configs so the editor can authenticate after it reloads the
/// generated configuration.
pub fn write_mcp_config_with_remote_auth(
    editor: &Editor,
    api_key: &str,
    workspace_id: Option<&str>,
    project_id: Option<&str>,
    transcripts_enabled: Option<bool>,
    hook_transcripts_enabled: Option<bool>,
    remote_auth_api_key: Option<&str>,
) -> Result<()> {
    write_mcp_config_with_transport_mode(
        editor,
        api_key,
        workspace_id,
        project_id,
        transcripts_enabled,
        hook_transcripts_enabled,
        TransportMode::PreserveExisting,
        remote_auth_api_key,
    )
}

pub fn write_mcp_config_force_local(
    editor: &Editor,
    api_key: &str,
    workspace_id: Option<&str>,
    project_id: Option<&str>,
    transcripts_enabled: Option<bool>,
    hook_transcripts_enabled: Option<bool>,
) -> Result<()> {
    write_mcp_config_with_transport_mode(
        editor,
        api_key,
        workspace_id,
        project_id,
        transcripts_enabled,
        hook_transcripts_enabled,
        TransportMode::ForceLocal,
        None,
    )
}

pub fn write_mcp_config_force_remote_with_auth(
    editor: &Editor,
    api_key: &str,
    workspace_id: Option<&str>,
    project_id: Option<&str>,
    transcripts_enabled: Option<bool>,
    hook_transcripts_enabled: Option<bool>,
    remote_auth_api_key: Option<&str>,
) -> Result<()> {
    write_mcp_config_with_transport_mode(
        editor,
        api_key,
        workspace_id,
        project_id,
        transcripts_enabled,
        hook_transcripts_enabled,
        TransportMode::ForceRemote,
        remote_auth_api_key,
    )
}

pub fn migrate_mcp_config(
    editor: &Editor,
    api_key: &str,
    workspace_id: Option<&str>,
    project_id: Option<&str>,
    transcripts_enabled: Option<bool>,
    hook_transcripts_enabled: Option<bool>,
) -> Result<()> {
    let transport_mode = TransportMode::ForceRemote;
    write_mcp_config_with_transport_mode(
        editor,
        api_key,
        workspace_id,
        project_id,
        transcripts_enabled,
        hook_transcripts_enabled,
        transport_mode,
        matches!(transport_mode, TransportMode::ForceRemote)
            .then_some(api_key)
            .filter(|value| !value.is_empty()),
    )
}

fn write_mcp_config_with_transport_mode(
    editor: &Editor,
    api_key: &str,
    workspace_id: Option<&str>,
    project_id: Option<&str>,
    transcripts_enabled: Option<bool>,
    hook_transcripts_enabled: Option<bool>,
    transport_mode: TransportMode,
    remote_auth_api_key: Option<&str>,
) -> Result<()> {
    if matches!(editor, Editor::Aider) {
        // Aider has no MCP config surface; do not create installation state for
        // a write that cannot occur.
        return Ok(());
    }
    let identity = ManagedConfigIdentity::for_write()?;
    let result = match editor {
        Editor::ClaudeCode
        | Editor::Cursor
        | Editor::Windsurf
        | Editor::Copilot
        | Editor::Antigravity => write_json_mcp_config(
            editor,
            api_key,
            workspace_id,
            project_id,
            transcripts_enabled,
            hook_transcripts_enabled,
            transport_mode,
            remote_auth_api_key,
            &identity,
        ),
        Editor::Cline => write_vscode_mcp_config(
            editor,
            api_key,
            "cline.mcpServers",
            workspace_id,
            project_id,
            transcripts_enabled,
            hook_transcripts_enabled,
            transport_mode,
            remote_auth_api_key,
            &identity,
        ),
        Editor::KiloCode => write_kilo_mcp_config(
            api_key,
            workspace_id,
            project_id,
            transcripts_enabled,
            hook_transcripts_enabled,
            transport_mode,
            remote_auth_api_key,
            &identity,
        ),
        Editor::RooCode => write_vscode_mcp_config(
            editor,
            api_key,
            "roo-cline.mcpServers",
            workspace_id,
            project_id,
            transcripts_enabled,
            hook_transcripts_enabled,
            transport_mode,
            remote_auth_api_key,
            &identity,
        ),
        Editor::Codex => write_codex_config(
            api_key,
            workspace_id,
            project_id,
            transcripts_enabled,
            hook_transcripts_enabled,
            transport_mode,
            remote_auth_api_key,
            &identity,
        ),
        Editor::Aider => unreachable!("Aider returned before loading config identity"),
        Editor::OpenCode => write_opencode_mcp_config(
            editor,
            api_key,
            workspace_id,
            project_id,
            transcripts_enabled,
            hook_transcripts_enabled,
            transport_mode,
            &identity,
        ),
    };
    result?;
    record_configured_evidence(editor);
    Ok(())
}

fn record_configured_evidence(editor: &Editor) {
    if safe_edit::is_dry_run()
        || !crate::readiness_evidence_writes_enabled()
        || matches!(editor, Editor::Aider)
    {
        return;
    }
    if let Err(error) = mcp_client::harness_readiness::record_configured(
        editor.harness_id(),
        MANAGED_CONFIG_VERSION,
        HARNESS_TEACHING_VERSION,
    ) {
        tracing::warn!(
            %error,
            editor = editor.id(),
            "could not update local harness readiness after MCP config write"
        );
    }
}

// ============================================================================
// Kilo CLI Config (kilo.jsonc format)
// ============================================================================

/// Build the Kilo CLI MCP server entry using the new format:
/// `{ "type": "local", "command": [...], "environment": {...}, "enabled": true }`
fn build_kilo_server_json(
    existing_server: Option<&Value>,
    api_key: Option<&str>,
    workspace_id: Option<&str>,
    project_id: Option<&str>,
    transcripts_enabled: Option<bool>,
    hook_transcripts_enabled: Option<bool>,
) -> Value {
    let (command, args) = resolved_command_and_args();
    let mut cmd_array = vec![json!(command)];
    for arg in &args {
        cmd_array.push(json!(arg));
    }

    // Build environment from existing + defaults (using "environment" key, not "env")
    let existing_env = existing_server
        .and_then(|s| s.get("environment").or_else(|| s.get("env")))
        .and_then(Value::as_object);

    let mut environment = existing_env.cloned().unwrap_or_default();

    for (key, value) in default_env_pairs(
        api_key,
        workspace_id,
        project_id,
        transcripts_enabled.unwrap_or(DEFAULT_TRANSCRIPTS_ENABLED),
        hook_transcripts_enabled.unwrap_or(DEFAULT_HOOK_TRANSCRIPTS_ENABLED),
    ) {
        environment
            .entry(key.to_string())
            .or_insert_with(|| Value::String(value));
    }

    // Always refresh these values
    let resolved_api_url = resolved_api_url(existing_server);
    environment.insert(
        "CONTEXTSTREAM_API_URL".to_string(),
        Value::String(resolved_api_url),
    );
    if let Some(api_key) = api_key {
        environment.insert(
            "CONTEXTSTREAM_API_KEY".to_string(),
            Value::String(api_key.to_string()),
        );
    } else {
        environment.remove("CONTEXTSTREAM_API_KEY");
    }
    if let Some(ws_id) = workspace_id {
        environment.insert(
            "CONTEXTSTREAM_WORKSPACE_ID".to_string(),
            Value::String(ws_id.to_string()),
        );
    }
    if let Some(proj_id) = project_id {
        environment.insert(
            "CONTEXTSTREAM_PROJECT_ID".to_string(),
            Value::String(proj_id.to_string()),
        );
    }

    let mut server = existing_server.cloned().unwrap_or_else(|| json!({}));
    server["type"] = json!("local");
    server["command"] = Value::Array(cmd_array);
    server["environment"] = Value::Object(environment);
    server["enabled"] = json!(true);
    // Remove legacy keys and stale remote-transport keys if present
    if let Some(obj) = server.as_object_mut() {
        obj.remove("args");
        obj.remove("env");
        obj.remove("url");
        obj.remove("serverUrl");
        obj.remove("headers");
    }
    server
}

/// Kilo's config schema names the remote transport `"remote"` (allowed values
/// are `local` | `remote`; docs: kilo.ai/docs/automate/mcp/using-in-cli), not
/// the generic `"http"` the shared remote builder emits, and its local-side
/// env key is `environment` rather than `env`. Reshape the shared remote
/// entry accordingly.
fn kilo_remote_server_json(mut server: Value) -> Value {
    server["type"] = json!("remote");
    if let Some(obj) = server.as_object_mut() {
        obj.remove("environment");
    }
    server
}

/// Wildcard permission that auto-approves ContextStream MCP tools in Kilo's
/// permission dock (`{server}_{tool}` naming; values allow|ask|deny — docs:
/// kilo.ai/docs/getting-started/settings/auto-approving-actions).
const KILO_CONTEXTSTREAM_PERMISSION_KEY: &str = "contextstream_*";

/// Auto-approve ContextStream tools unless the user already expressed ANY
/// contextstream permission preference (their allow/ask/deny always wins).
fn ensure_kilo_contextstream_permission(config: &mut Value) -> Result<()> {
    require_object_if_present(config.get("permission"), "Kilo permission")?;
    let user_has_preference = config
        .get("permission")
        .and_then(Value::as_object)
        .is_some_and(|perms| perms.keys().any(|key| key.starts_with("contextstream")));
    if user_has_preference {
        return Ok(());
    }

    if let Some(perms) = config.get_mut("permission").and_then(Value::as_object_mut) {
        perms.insert(
            KILO_CONTEXTSTREAM_PERMISSION_KEY.to_string(),
            json!("allow"),
        );
    } else {
        config["permission"] = json!({ KILO_CONTEXTSTREAM_PERMISSION_KEY: "allow" });
    }
    Ok(())
}

/// Write Kilo CLI global MCP config to ~/.config/kilo/kilo.jsonc.
fn write_kilo_mcp_config(
    api_key: &str,
    workspace_id: Option<&str>,
    project_id: Option<&str>,
    transcripts_enabled: Option<bool>,
    hook_transcripts_enabled: Option<bool>,
    transport_mode: TransportMode,
    remote_auth_api_key: Option<&str>,
    identity: &ManagedConfigIdentity,
) -> Result<()> {
    let path = Editor::KiloCode.mcp_config_path().ok_or_else(|| {
        anyhow::anyhow!(
            "Could not determine config path for {}",
            Editor::KiloCode.display_name()
        )
    })?;

    // kilo.jsonc is JSONC by name and by convention, so read it tolerantly but
    // write it back surgically (see the write at the end of this function).
    let loaded = safe_edit::read_for_edit(&path, safe_edit::JsonDialect::Jsonc)?;
    let mut config: Value = loaded.value.clone();
    require_object_if_present(config.get("mcp"), "Kilo mcp")?;

    let existing_server = config
        .get("mcp")
        .and_then(|servers| servers.get("contextstream"));
    validate_existing_json_like_server(existing_server, "Kilo mcp.contextstream")?;

    let mut server = if should_use_remote_http(&Editor::KiloCode, existing_server, transport_mode) {
        kilo_remote_server_json(build_remote_http_server_json(
            existing_server,
            workspace_id,
            project_id,
            transcripts_enabled,
            hook_transcripts_enabled,
            remote_auth_api_key,
        ))
    } else {
        build_kilo_server_json(
            existing_server,
            Some(api_key),
            workspace_id,
            project_id,
            transcripts_enabled,
            hook_transcripts_enabled,
        )
    };
    apply_managed_config_metadata(&Editor::KiloCode, &mut server, identity);

    // Upsert into the "mcp" key, preserving other MCP servers and top-level settings
    if let Some(mcp) = config.get_mut("mcp").and_then(Value::as_object_mut) {
        mcp.insert("contextstream".to_string(), server);
    } else {
        config["mcp"] = json!({
            "contextstream": server
        });
    }

    // Auto-add instructions reference if not present
    if config.get("instructions").is_none() {
        config["instructions"] = json!([".kilo/rules/*.md"]);
    }

    // Auto-approve ContextStream tools in the permission dock (user
    // preferences win; see ensure_kilo_contextstream_permission).
    ensure_kilo_contextstream_permission(&mut config)?;

    safe_edit::commit(&path, &loaded, &config)?;

    Ok(())
}

fn build_opencode_environment(
    existing_server: Option<&Value>,
    api_key: Option<&str>,
    context_pack_enabled: Option<bool>,
) -> Value {
    let mut environment = match existing_server
        .and_then(|server| server.get("environment"))
        .and_then(Value::as_object)
    {
        Some(existing_env) => existing_env.clone(),
        None => serde_json::Map::new(),
    };

    // opencode does NOT expand `{env:VAR}` placeholders inside an `mcp.*.environment`
    // block (that interpolation only applies to provider `apiKey` fields). For local
    // transport we need a working credential: embed the actual key when we have one,
    // and only fall back to the placeholder for backwards compatibility when the
    // caller can't supply it.
    let api_key_value = api_key
        .map(|k| k.to_string())
        .unwrap_or_else(|| "{env:CONTEXTSTREAM_API_KEY}".to_string());

    for (key, value) in [
        ("CONTEXTSTREAM_API_KEY", api_key_value),
        (
            ENV_MANAGED_CONFIG_VERSION,
            MANAGED_CONFIG_VERSION.to_string(),
        ),
        ("CONTEXTSTREAM_TOOLSET", DEFAULT_TOOLSET.to_string()),
        ("CONTEXTSTREAM_LOG_LEVEL", "quiet".to_string()),
        (
            "CONTEXTSTREAM_OUTPUT_FORMAT",
            DEFAULT_OUTPUT_FORMAT.to_string(),
        ),
        (
            ENV_TRANSCRIPTS_ENABLED,
            DEFAULT_TRANSCRIPTS_ENABLED.to_string(),
        ),
        (
            ENV_HOOK_TRANSCRIPTS_ENABLED,
            DEFAULT_HOOK_TRANSCRIPTS_ENABLED.to_string(),
        ),
        ("CONTEXTSTREAM_CONSOLIDATED", "true".to_string()),
        ("CONTEXTSTREAM_AUTO_HIDE_INTEGRATIONS", "true".to_string()),
        (ENV_CLIENT_NAME, Editor::OpenCode.id().to_string()),
        (
            "CONTEXTSTREAM_SEARCH_LIMIT",
            DEFAULT_SEARCH_LIMIT.to_string(),
        ),
        (
            "CONTEXTSTREAM_SEARCH_MAX_CHARS",
            DEFAULT_SEARCH_MAX_CHARS.to_string(),
        ),
        (
            "CONTEXTSTREAM_INCLUDE_STRUCTURED_CONTENT",
            "true".to_string(),
        ),
    ] {
        environment
            .entry(key.to_string())
            .or_insert_with(|| Value::String(value));
    }

    let resolved_api_url = resolved_api_url(existing_server);
    if resolved_api_url != "https://api.contextstream.io" {
        environment.insert(
            "CONTEXTSTREAM_API_URL".to_string(),
            Value::String(resolved_api_url),
        );
    } else if existing_server
        .and_then(|server| server.get("environment"))
        .and_then(Value::as_object)
        .and_then(|env| env.get("CONTEXTSTREAM_API_URL"))
        .is_none()
    {
        environment.remove("CONTEXTSTREAM_API_URL");
    }

    match context_pack_enabled {
        Some(false) => {
            environment.insert(
                "CONTEXTSTREAM_CONTEXT_PACK".to_string(),
                Value::String("false".to_string()),
            );
        }
        Some(true) => {
            environment.insert(
                "CONTEXTSTREAM_CONTEXT_PACK".to_string(),
                Value::String("true".to_string()),
            );
        }
        None => {}
    }

    Value::Object(environment)
}

fn build_opencode_server_json(
    existing_server: Option<&Value>,
    api_key: Option<&str>,
    context_pack_enabled: Option<bool>,
) -> Value {
    let (cmd, args) = resolved_command_and_args();
    let mut command_array = Vec::with_capacity(1 + args.len());
    command_array.push(Value::String(cmd));
    command_array.extend(args.into_iter().map(Value::String));

    let mut server = existing_server.cloned().unwrap_or_else(|| json!({}));
    server["type"] = json!("local");
    server["command"] = Value::Array(command_array);
    server["environment"] =
        build_opencode_environment(existing_server, api_key, context_pack_enabled);
    server["enabled"] = json!(true);
    server
}

fn build_opencode_remote_server_json(existing_server: Option<&Value>) -> Value {
    let mut server = existing_server.cloned().unwrap_or_else(|| json!({}));
    server["type"] = json!("remote");
    server["url"] = json!(resolved_remote_mcp_url(existing_server));
    server["enabled"] = json!(true);
    if let Some(obj) = server.as_object_mut() {
        obj.remove("command");
        obj.remove("environment");
        obj.remove("env");
        obj.remove("args");
    }
    server
}

fn write_opencode_mcp_config(
    editor: &Editor,
    api_key: &str,
    _workspace_id: Option<&str>,
    _project_id: Option<&str>,
    _transcripts_enabled: Option<bool>,
    _hook_transcripts_enabled: Option<bool>,
    transport_mode: TransportMode,
    identity: &ManagedConfigIdentity,
) -> Result<()> {
    let path = Editor::OpenCode.mcp_config_path().ok_or_else(|| {
        anyhow::anyhow!(
            "Could not determine config path for {}",
            Editor::OpenCode.display_name()
        )
    })?;

    let loaded = safe_edit::read_for_edit(&path, safe_edit::JsonDialect::Strict)?;
    let mut config: Value = loaded.value.clone();
    require_object_if_present(config.get("mcp"), "OpenCode mcp")?;

    let existing_server = config
        .get("mcp")
        .and_then(|servers| servers.get("contextstream"));
    validate_existing_json_like_server(existing_server, "OpenCode mcp.contextstream")?;
    let mut server = if should_use_remote_http(editor, existing_server, transport_mode) {
        build_opencode_remote_server_json(existing_server)
    } else {
        build_opencode_server_json(existing_server, Some(api_key), None)
    };
    apply_managed_config_metadata(editor, &mut server, identity);

    if config.get("$schema").is_none() {
        config["$schema"] = json!(OPENCODE_CONFIG_SCHEMA_URL);
    }
    if let Some(mcp_servers) = config.get_mut("mcp").and_then(Value::as_object_mut) {
        mcp_servers.insert("contextstream".to_string(), server);
    } else {
        config["mcp"] = json!({
            "contextstream": server
        });
    }

    safe_edit::commit(&path, &loaded, &config)?;

    Ok(())
}

/// Write JSON MCP config file.
fn write_json_mcp_config(
    editor: &Editor,
    api_key: &str,
    workspace_id: Option<&str>,
    project_id: Option<&str>,
    transcripts_enabled: Option<bool>,
    hook_transcripts_enabled: Option<bool>,
    transport_mode: TransportMode,
    remote_auth_api_key: Option<&str>,
    identity: &ManagedConfigIdentity,
) -> Result<()> {
    let path = editor.mcp_config_path().ok_or_else(|| {
        anyhow::anyhow!(
            "Could not determine config path for {}",
            editor.display_name()
        )
    })?;

    // Read existing config or create new
    let loaded = safe_edit::read_for_edit(&path, safe_edit::JsonDialect::Strict)?;
    let mut config: Value = loaded.value.clone();

    // VS Code (Copilot) uses "servers" root; other editors use "mcpServers".
    let root_key = if matches!(editor, Editor::Copilot) {
        "servers"
    } else {
        "mcpServers"
    };
    require_object_if_present(
        config.get(root_key),
        &format!("{} {}", editor.display_name(), root_key),
    )?;

    let existing_server = config
        .get(root_key)
        .and_then(|servers| servers.get("contextstream"));
    let server = build_json_like_server_for_editor_with_identity(
        editor,
        existing_server,
        Some(api_key),
        workspace_id,
        project_id,
        transcripts_enabled,
        hook_transcripts_enabled,
        transport_mode,
        remote_auth_api_key,
        identity,
    )?;

    if let Some(mcp_servers) = config.get_mut(root_key).and_then(Value::as_object_mut) {
        mcp_servers.insert("contextstream".to_string(), server);
    } else {
        config[root_key] = json!({
            "contextstream": server
        });
    }

    safe_edit::commit(&path, &loaded, &config)?;

    Ok(())
}

/// Write VS Code settings MCP config.
fn write_vscode_mcp_config(
    editor: &Editor,
    api_key: &str,
    settings_key: &str,
    workspace_id: Option<&str>,
    project_id: Option<&str>,
    transcripts_enabled: Option<bool>,
    hook_transcripts_enabled: Option<bool>,
    transport_mode: TransportMode,
    remote_auth_api_key: Option<&str>,
    identity: &ManagedConfigIdentity,
) -> Result<()> {
    let path = get_vscode_settings_path()
        .ok_or_else(|| anyhow::anyhow!("Could not determine VS Code settings path"))?;

    // VS Code settings.json is officially JSONC and is almost always
    // hand-maintained, so this is a surgical edit of one top-level key rather
    // than a parse-and-rewrite: comments, key order, and formatting elsewhere
    // in the file survive byte-for-byte.
    let loaded = safe_edit::read_for_edit(&path, safe_edit::JsonDialect::Jsonc)?;
    require_object_if_present(
        loaded.value.get(settings_key),
        &format!("VS Code setting {}", settings_key),
    )?;

    let existing_server = loaded
        .value
        .get(settings_key)
        .and_then(|entry| entry.get("contextstream"));
    let contextstream = build_json_like_server_for_editor_with_identity(
        editor,
        existing_server,
        Some(api_key),
        workspace_id,
        project_id,
        transcripts_enabled,
        hook_transcripts_enabled,
        transport_mode,
        remote_auth_api_key,
        identity,
    )?;

    // Preserve any other MCP servers already registered under this key.
    let mut section = match loaded.value.get(settings_key) {
        Some(Value::Object(existing)) => Value::Object(existing.clone()),
        None => json!({}),
        Some(_) => unreachable!("settings section was validated as an object"),
    };
    section["contextstream"] = contextstream;

    let mut updated = loaded.value.clone();
    updated[settings_key] = section;
    safe_edit::commit(&path, &loaded, &updated)?;

    Ok(())
}

fn codex_contextstream_section_uses_remote(content: &str) -> Option<bool> {
    let marker = "[mcp_servers.contextstream]";
    let env_marker = "[mcp_servers.contextstream.env]";
    let mut in_contextstream_section = false;

    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('[') && trimmed.ends_with(']') {
            if trimmed == marker {
                in_contextstream_section = true;
                continue;
            }

            if in_contextstream_section && trimmed != env_marker {
                break;
            }

            continue;
        }

        if !in_contextstream_section || trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }

        let Some((name, _)) = trimmed.split_once('=') else {
            continue;
        };

        match name.trim() {
            "url" => return Some(true),
            "command" => return Some(false),
            _ => {}
        }
    }

    None
}

/// Write Codex TOML config using upsert logic.
///
/// Preserves existing config outside the `[mcp_servers.contextstream]` section.
/// Updates the contextstream section in-place if it already exists, or appends it.
pub(super) fn parse_codex_toml(content: &str, path: &Path) -> Result<DocumentMut> {
    content.parse::<DocumentMut>().with_context(|| {
        format!(
            "Refusing to modify {} because it is not valid TOML",
            path.display()
        )
    })
}

fn render_codex_toml(document: &DocumentMut, original: &str) -> Result<String> {
    let mut saw_crlf = false;
    let mut saw_lf = false;
    let bytes = original.as_bytes();
    for (index, byte) in bytes.iter().enumerate() {
        if *byte == b'\n' {
            if index > 0 && bytes[index - 1] == b'\r' {
                saw_crlf = true;
            } else {
                saw_lf = true;
            }
        }
    }
    if saw_crlf && saw_lf {
        anyhow::bail!("Refusing to modify Codex config with mixed LF and CRLF line endings");
    }

    let had_trailing_newline = original.ends_with('\n');
    let mut rendered = document.to_string();
    if !had_trailing_newline && rendered.ends_with('\n') {
        rendered.pop();
    }
    if saw_crlf {
        rendered = rendered.replace('\n', "\r\n");
    }
    Ok(rendered)
}

pub(super) fn contextstream_toml_item(document: &DocumentMut) -> Option<&Item> {
    document
        .get("mcp_servers")
        .and_then(Item::as_table_like)
        .and_then(|servers| servers.get("contextstream"))
}

pub(super) fn toml_item_string<'a>(item: &'a Item, key: &str) -> Option<&'a str> {
    item.as_table_like()?.get(key)?.as_value()?.as_str()
}

pub(super) fn toml_nested_string<'a>(
    item: &'a Item,
    table_key: &str,
    key: &str,
) -> Option<&'a str> {
    let nested = item.as_table_like()?.get(table_key)?;
    nested
        .as_table_like()
        .and_then(|table| table.get(key))
        .and_then(Item::as_value)
        .and_then(toml_edit::Value::as_str)
        .or_else(|| {
            nested
                .as_value()
                .and_then(toml_edit::Value::as_inline_table)
                .and_then(|table| table.get(key))
                .and_then(toml_edit::Value::as_str)
        })
}

fn toml_item_has_managed_heading(item: &Item) -> bool {
    item.as_table()
        .and_then(|table| table.decor().prefix())
        .and_then(|prefix| prefix.as_str())
        .is_some_and(|prefix| {
            prefix
                .lines()
                .any(|line| line.trim() == CODEX_MANAGED_COMMENT)
        })
}

fn toml_item_is_contextstream_managed(item: &Item) -> bool {
    if toml_item_has_managed_heading(item)
        || toml_nested_string(item, "env", ENV_MANAGED_CONFIG_VERSION)
            .is_some_and(is_recognized_managed_config_version)
        || ["http_headers", "headers"].into_iter().any(|table| {
            toml_nested_string(item, table, HEADER_MANAGED_CONFIG_VERSION)
                .is_some_and(is_recognized_managed_config_version)
        })
    {
        return true;
    }

    if let Some(command) = toml_item_string(item, "command") {
        let args = item
            .as_table_like()
            .and_then(|table| table.get("args"))
            .and_then(Item::as_value)
            .and_then(toml_edit::Value::as_array)
            .and_then(|array| {
                array
                    .iter()
                    .map(toml_edit::Value::as_str)
                    .collect::<Option<Vec<_>>>()
            })
            .unwrap_or_default();
        if command_and_args_are_contextstream_managed(command, &args) {
            return true;
        }
        if command_and_args_use_contextstream_binary(command, &args)
            && [
                "CONTEXTSTREAM_API_URL",
                "CONTEXTSTREAM_USER_AGENT",
                "CONTEXTSTREAM_TOOLSET",
                "CONTEXTSTREAM_CLIENT",
                "CONTEXTSTREAM_API_KEY",
            ]
            .into_iter()
            .any(|key| toml_nested_string(item, "env", key).is_some())
        {
            return true;
        }
    }

    toml_item_string(item, "url").is_some_and(is_default_contextstream_remote_url)
}

fn strip_one_trailing_managed_heading(prefix: &str) -> String {
    let mut offset = 0usize;
    let mut candidate = None;
    for line in prefix.split_inclusive('\n') {
        let start = offset;
        offset += line.len();
        if line.trim().is_empty() {
            continue;
        }
        candidate = (line.trim() == CODEX_MANAGED_COMMENT).then_some((start, offset));
    }
    if offset < prefix.len() {
        let line = &prefix[offset..];
        if !line.trim().is_empty() {
            candidate = (line.trim() == CODEX_MANAGED_COMMENT).then_some((offset, prefix.len()));
        }
    }

    let Some((start, end)) = candidate else {
        return prefix.to_string();
    };
    let mut preserved = String::with_capacity(prefix.len() - (end - start));
    preserved.push_str(&prefix[..start]);
    preserved.push_str(&prefix[end..]);
    preserved
}

fn preserve_user_prefix_when_replacing_codex_item(existing: &Item, replacement: &mut Item) {
    let (Some(existing), Some(replacement)) = (existing.as_table(), replacement.as_table_mut())
    else {
        return;
    };
    let Some(existing_prefix) = existing.decor().prefix().and_then(|prefix| prefix.as_str()) else {
        return;
    };
    let preserved = strip_one_trailing_managed_heading(existing_prefix);
    let replacement_prefix = replacement
        .decor()
        .prefix()
        .and_then(|prefix| prefix.as_str())
        .unwrap_or("")
        .to_string();
    replacement
        .decor_mut()
        .set_prefix(format!("{preserved}{replacement_prefix}"));
}

fn set_contextstream_toml_item(document: &mut DocumentMut, item: Item) -> Result<()> {
    if document.get("mcp_servers").is_none() {
        let mut servers = Table::new();
        servers.set_implicit(true);
        document
            .as_table_mut()
            .insert("mcp_servers", Item::Table(servers));
    }
    let servers = document
        .get_mut("mcp_servers")
        .and_then(Item::as_table_like_mut)
        .ok_or_else(|| {
            anyhow::anyhow!("Refusing to modify Codex config: 'mcp_servers' is not a TOML table")
        })?;
    servers.insert("contextstream", item);
    Ok(())
}

fn upsert_contextstream_toml(existing: &str, new_block: &str, path: &Path) -> Result<String> {
    let mut document = parse_codex_toml(existing, path)?;
    if contextstream_toml_item(&document)
        .is_some_and(|item| !toml_item_is_contextstream_managed(item))
    {
        anyhow::bail!(
            "Refusing to replace {} because mcp_servers.contextstream is not recognizably \
             managed by ContextStream. Rename or remove that table explicitly before installing.",
            path.display()
        );
    }
    let generated = parse_codex_toml(new_block, path)
        .context("Internal error: generated ContextStream configuration is not valid TOML")?;
    let mut item = contextstream_toml_item(&generated)
        .cloned()
        .ok_or_else(|| {
            anyhow::anyhow!(
                "Internal error: generated Codex config has no ContextStream server table"
            )
        })?;
    if let Some(existing_item) = contextstream_toml_item(&document) {
        preserve_user_prefix_when_replacing_codex_item(existing_item, &mut item);
    }
    set_contextstream_toml_item(&mut document, item)?;
    render_codex_toml(&document, existing)
}

fn remove_contextstream_toml(
    content: &str,
    path: &Path,
    backup: Option<&DocumentMut>,
) -> Result<(String, bool)> {
    let mut document = parse_codex_toml(content, path)?;
    let current_server = contextstream_toml_item(&document).cloned();
    let mut changed_server = false;
    if current_server
        .as_ref()
        .is_some_and(toml_item_is_contextstream_managed)
    {
        match backup
            .and_then(contextstream_toml_item)
            .filter(|item| !toml_item_is_contextstream_managed(item))
        {
            Some(original_server) => {
                set_contextstream_toml_item(&mut document, original_server.clone())?;
                changed_server = true;
            }
            None => {
                changed_server = document
                    .get_mut("mcp_servers")
                    .and_then(Item::as_table_like_mut)
                    .and_then(|servers| servers.remove("contextstream"))
                    .is_some();
            }
        }
    }
    let removed_trust = revert_managed_codex_project_trust(&mut document, backup)?;
    if !changed_server && !removed_trust {
        return Ok((content.to_string(), false));
    }

    let remove_empty_implicit_parent = document
        .get("mcp_servers")
        .and_then(Item::as_table)
        .is_some_and(|table| table.is_implicit() && table.is_empty());
    if remove_empty_implicit_parent {
        document.as_table_mut().remove("mcp_servers");
    }
    Ok((render_codex_toml(&document, content)?, true))
}

fn write_codex_config(
    api_key: &str,
    workspace_id: Option<&str>,
    project_id: Option<&str>,
    transcripts_enabled: Option<bool>,
    hook_transcripts_enabled: Option<bool>,
    transport_mode: TransportMode,
    remote_auth_api_key: Option<&str>,
    identity: &ManagedConfigIdentity,
) -> Result<()> {
    let path = dirs::home_dir()
        .map(|h| h.join(".codex").join("config.toml"))
        .ok_or_else(|| anyhow::anyhow!("Could not determine Codex config path"))?;

    let remote_supported = editor_supports_hosted_remote(&Editor::Codex);
    let default_remote = remote_supported && resolved_api_url(None) == DEFAULT_API_URL;

    if !path.exists() {
        let requested_remote = match transport_mode {
            TransportMode::ForceRemote => true,
            TransportMode::ForceLocal => false,
            TransportMode::PreserveExisting => default_remote,
        };
        let use_remote = remote_supported && requested_remote;
        let new_block = if use_remote {
            build_codex_remote_toml_config(
                None,
                &resolved_remote_mcp_url(None),
                workspace_id,
                project_id,
                transcripts_enabled,
                hook_transcripts_enabled,
                remote_auth_api_key,
                identity,
            )
        } else {
            build_codex_local_toml_config_preserving_existing(
                None,
                api_key,
                workspace_id,
                project_id,
                transcripts_enabled,
                hook_transcripts_enabled,
                identity,
            )
        };
        parse_codex_toml(&new_block, &path)
            .context("Internal error: generated Codex configuration is invalid")?;
        safe_edit::write_if_unchanged(&path, &new_block, None)?;
        return Ok(());
    }

    let raw_existing = std::fs::read_to_string(&path)
        .with_context(|| format!("Could not read Codex config {}", path.display()))?;
    parse_codex_toml(&raw_existing, &path)?;
    let existing = raw_existing.as_str();

    let resolved_workspace_id = workspace_id
        .filter(|value| !value.is_empty())
        .map(String::from)
        .or_else(|| extract_codex_env_value(existing, "CONTEXTSTREAM_WORKSPACE_ID"));
    let resolved_project_id = project_id
        .filter(|value| !value.is_empty())
        .map(String::from)
        .or_else(|| extract_codex_env_value(existing, "CONTEXTSTREAM_PROJECT_ID"));
    let requested_remote = match transport_mode {
        TransportMode::ForceRemote => true,
        TransportMode::ForceLocal => false,
        TransportMode::PreserveExisting => {
            codex_contextstream_section_uses_remote(existing).unwrap_or(default_remote)
        }
    };
    let use_remote = remote_supported && requested_remote;
    let new_block = if use_remote {
        build_codex_remote_toml_config(
            Some(existing),
            &resolved_remote_mcp_url(None),
            resolved_workspace_id.as_deref(),
            resolved_project_id.as_deref(),
            transcripts_enabled,
            hook_transcripts_enabled,
            remote_auth_api_key,
            identity,
        )
    } else {
        build_codex_local_toml_config_preserving_existing(
            Some(existing),
            api_key,
            resolved_workspace_id.as_deref(),
            resolved_project_id.as_deref(),
            transcripts_enabled,
            hook_transcripts_enabled,
            identity,
        )
    };

    let result = upsert_contextstream_toml(existing, &new_block, &path)?;
    safe_edit::write_if_unchanged(&path, &result, Some(&raw_existing))?;
    Ok(())
}

/// Ensure the given project directory is listed as a trusted project in `~/.codex/config.toml`.
///
/// Codex disables project-level `config.toml` files unless the project folder is explicitly
/// trusted in the global config.  This appends a `[projects."<path>"]` section with
/// `trust_level = "trusted"` when one does not already exist.
pub fn ensure_codex_project_trust(project_path: &std::path::Path) -> Result<()> {
    let config_path = dirs::home_dir()
        .map(|h| h.join(".codex").join("config.toml"))
        .ok_or_else(|| anyhow::anyhow!("Could not determine Codex config path"))?;

    if !config_path
        .try_exists()
        .with_context(|| format!("Could not inspect Codex config {}", config_path.display()))?
    {
        return Ok(());
    }

    let content = std::fs::read_to_string(&config_path)
        .with_context(|| format!("Could not read Codex config {}", config_path.display()))?;
    let document = parse_codex_toml(&content, &config_path)?;
    let wholly_managed = codex_document_is_wholly_managed(&content, &document);
    let updated = upsert_codex_project_trust_config(&content, project_path)?;

    if updated == content {
        return Ok(());
    }

    if wholly_managed {
        safe_edit::write_owned_file_if_unchanged(&config_path, &updated, Some(&content))?;
    } else {
        safe_edit::write_if_unchanged(&config_path, &updated, Some(&content))?;
    }
    Ok(())
}

fn project_has_managed_trust(project: &Table) -> bool {
    project
        .key("trust_level")
        .and_then(|key| key.leaf_decor().prefix())
        .and_then(|prefix| prefix.as_str())
        .is_some_and(|prefix| {
            prefix
                .lines()
                .any(|line| line.trim() == CODEX_MANAGED_TRUST_COMMENT)
        })
}

fn add_managed_trust_marker(project: &mut Table) -> Result<()> {
    let mut key = project
        .key_mut("trust_level")
        .ok_or_else(|| anyhow::anyhow!("Codex project table has no trust_level key"))?;
    let existing_prefix = key
        .leaf_decor()
        .prefix()
        .and_then(|prefix| prefix.as_str())
        .unwrap_or("");
    if existing_prefix
        .lines()
        .any(|line| line.trim() == CODEX_MANAGED_TRUST_COMMENT)
    {
        return Ok(());
    }

    let mut prefix = existing_prefix.to_string();
    if !prefix.ends_with('\n') {
        prefix.push('\n');
    }
    prefix.push_str(CODEX_MANAGED_TRUST_COMMENT);
    prefix.push('\n');
    key.leaf_decor_mut().set_prefix(prefix);
    Ok(())
}

fn set_managed_codex_project_trust(
    document: &mut DocumentMut,
    normalized_path: &str,
) -> Result<bool> {
    if document.get("projects").is_none() {
        let mut projects = Table::new();
        projects.set_implicit(true);
        document
            .as_table_mut()
            .insert("projects", Item::Table(projects));
    }
    let projects = document
        .get_mut("projects")
        .and_then(Item::as_table_like_mut)
        .ok_or_else(|| {
            anyhow::anyhow!("Refusing to modify Codex config: 'projects' is not a TOML table")
        })?;

    let desired_key = codex_project_compare_key(normalized_path);
    let matching_keys: Vec<String> = projects
        .iter()
        .filter(|(key, _)| codex_project_compare_key(key) == desired_key)
        .map(|(key, _)| key.to_string())
        .collect();
    if matching_keys.len() > 1 {
        anyhow::bail!(
            "Refusing to modify Codex config: multiple project tables normalize to '{}'",
            normalized_path
        );
    }

    if let Some(existing_key) = matching_keys.first() {
        let project = projects
            .get_mut(existing_key)
            .and_then(Item::as_table_mut)
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "Refusing to modify Codex config: project '{}' is not a standard TOML table",
                    existing_key
                )
            })?;
        if project
            .get("trust_level")
            .and_then(Item::as_value)
            .and_then(toml_edit::Value::as_str)
            == Some("trusted")
        {
            return Ok(false);
        }

        let mut trust = toml_edit::value("trusted");
        if let Some(existing) = project.get("trust_level").and_then(Item::as_value) {
            *trust
                .as_value_mut()
                .expect("toml_edit::value always produces a value")
                .decor_mut() = existing.decor().clone();
        }
        project.insert("trust_level", trust);
        add_managed_trust_marker(project)?;
        return Ok(true);
    }

    let mut project = Table::new();
    project.insert("trust_level", toml_edit::value("trusted"));
    add_managed_trust_marker(&mut project)?;
    projects.insert(normalized_path, Item::Table(project));
    Ok(true)
}

fn managed_codex_trust_paths(document: &DocumentMut) -> Vec<String> {
    document
        .get("projects")
        .and_then(Item::as_table_like)
        .map(|projects| {
            projects
                .iter()
                .filter_map(|(path, item)| {
                    item.as_table()
                        .filter(|project| project_has_managed_trust(project))
                        .map(|_| path.to_string())
                })
                .collect()
        })
        .unwrap_or_default()
}

fn matching_codex_project_key(
    projects: &dyn toml_edit::TableLike,
    normalized_path: &str,
) -> Result<Option<String>> {
    let desired_key = codex_project_compare_key(normalized_path);
    let matching_keys: Vec<String> = projects
        .iter()
        .filter(|(key, _)| codex_project_compare_key(key) == desired_key)
        .map(|(key, _)| key.to_string())
        .collect();
    if matching_keys.len() > 1 {
        anyhow::bail!(
            "Refusing to modify Codex config: multiple project tables normalize to '{}'",
            normalized_path
        );
    }
    Ok(matching_keys.into_iter().next())
}

fn revert_managed_codex_project_trust(
    document: &mut DocumentMut,
    backup: Option<&DocumentMut>,
) -> Result<bool> {
    let managed_paths = managed_codex_trust_paths(document);
    if managed_paths.is_empty() {
        return Ok(false);
    }

    let mut changed = false;
    for current_key in managed_paths {
        let backup_project = match backup
            .and_then(|backup| backup.get("projects"))
            .and_then(Item::as_table_like)
        {
            Some(projects) => matching_codex_project_key(projects, &current_key)?
                .and_then(|key| projects.get(&key))
                .and_then(Item::as_table)
                .cloned(),
            None => None,
        };

        let projects = document
            .get_mut("projects")
            .and_then(Item::as_table_like_mut)
            .ok_or_else(|| anyhow::anyhow!("Codex projects table changed unexpectedly"))?;
        let project = projects
            .get_mut(&current_key)
            .and_then(Item::as_table_mut)
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "Codex project '{}' changed unexpectedly during uninstall",
                    current_key
                )
            })?;

        match backup_project.as_ref().and_then(|table| {
            table
                .get("trust_level")
                .cloned()
                .zip(table.key("trust_level").map(|key| key.leaf_decor().clone()))
        }) {
            Some((backup_trust, backup_key_decor)) => {
                project.insert("trust_level", backup_trust);
                project
                    .key_mut("trust_level")
                    .expect("restored trust key exists")
                    .leaf_decor_mut()
                    .clone_from(&backup_key_decor);
            }
            None => {
                project.remove("trust_level");
            }
        }
        let remove_project = project.is_empty() && backup_project.is_none();
        if remove_project {
            projects.remove(&current_key);
        }
        changed = true;
    }

    let remove_empty_implicit_parent = document
        .get("projects")
        .and_then(Item::as_table)
        .is_some_and(|table| table.is_implicit() && table.is_empty());
    if remove_empty_implicit_parent {
        document.as_table_mut().remove("projects");
    }
    Ok(changed)
}

fn codex_document_is_wholly_managed(content: &str, document: &DocumentMut) -> bool {
    if content.lines().any(|line| {
        let trimmed = line.trim();
        line.contains('#')
            && trimmed != CODEX_MANAGED_COMMENT
            && trimmed != CODEX_MANAGED_TRUST_COMMENT
    }) {
        return false;
    }
    if document
        .iter()
        .any(|(key, _)| !matches!(key, "mcp_servers" | "projects"))
    {
        return false;
    }

    let Some(servers) = document.get("mcp_servers").and_then(Item::as_table_like) else {
        return false;
    };
    if servers.len() != 1
        || !servers
            .get("contextstream")
            .is_some_and(toml_item_is_contextstream_managed)
    {
        return false;
    }

    document
        .get("projects")
        .and_then(Item::as_table_like)
        .is_none_or(|projects| {
            projects.iter().all(|(_, item)| {
                let Some(project) = item.as_table() else {
                    return false;
                };
                project.len() == 1 && project_has_managed_trust(project)
            })
        })
}

fn upsert_codex_project_trust_config(content: &str, project_path: &Path) -> Result<String> {
    let normalized_path = normalize_codex_project_path(&project_path.to_string_lossy());
    let mut document = parse_codex_toml(content, Path::new("Codex config.toml"))?;
    if !set_managed_codex_project_trust(&mut document, &normalized_path)? {
        return Ok(content.to_string());
    }
    render_codex_toml(&document, content)
}

fn codex_project_compare_key(path: &str) -> String {
    let normalized = normalize_codex_project_path(path);
    if normalized.as_bytes().get(1) == Some(&b':') || normalized.starts_with("//") {
        normalized.to_ascii_lowercase()
    } else {
        normalized
    }
}

fn normalize_codex_project_path(path: &str) -> String {
    let decoded = decode_codex_project_key(path);
    // Strip Windows extended-length path prefix (\\?\ or //?/)
    let decoded = decoded
        .strip_prefix("\\\\?\\")
        .or_else(|| decoded.strip_prefix("//?/"))
        .unwrap_or(&decoded)
        .to_string();
    let preserve_unc_prefix = decoded.starts_with("\\\\") || decoded.starts_with("//");
    let mut normalized = String::with_capacity(decoded.len());
    let mut slash_count = 0usize;
    let mut previous_was_separator = false;

    for ch in decoded.chars() {
        if ch == '\\' || ch == '/' {
            if preserve_unc_prefix && slash_count < 2 {
                normalized.push('/');
                slash_count += 1;
                previous_was_separator = true;
                continue;
            }

            if !previous_was_separator {
                normalized.push('/');
            }
            previous_was_separator = true;
            continue;
        }

        normalized.push(ch);
        previous_was_separator = false;
    }

    if normalized.as_bytes().get(1) == Some(&b':') {
        let mut chars = normalized.chars();
        if let Some(first) = chars.next() {
            normalized = format!("{}{}", first.to_ascii_uppercase(), chars.as_str());
        }
    }

    normalized
}

fn decode_codex_project_key(raw: &str) -> String {
    let mut decoded = String::with_capacity(raw.len());
    let mut chars = raw.chars().peekable();

    while let Some(ch) = chars.next() {
        if ch != '\\' {
            decoded.push(ch);
            continue;
        }

        match chars.peek().copied() {
            Some('\\') => {
                decoded.push('\\');
                chars.next();
            }
            Some('"') => {
                decoded.push('"');
                chars.next();
            }
            Some(next) => {
                decoded.push('\\');
                decoded.push(next);
                chars.next();
            }
            None => decoded.push('\\'),
        }
    }

    decoded
}

fn extract_codex_env_value(content: &str, key: &str) -> Option<String> {
    let env_marker = "[mcp_servers.contextstream.env]";
    let mut in_env_section = false;

    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('[') && trimmed.ends_with(']') {
            in_env_section = trimmed == env_marker;
            continue;
        }

        if !in_env_section || trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }

        let (name, raw_value) = match trimmed.split_once('=') {
            Some(parts) => parts,
            None => continue,
        };

        if name.trim() != key {
            continue;
        }

        let value = raw_value.trim();
        if value.starts_with('"') && value.ends_with('"') && value.len() >= 2 {
            let unquoted = &value[1..value.len() - 1];
            return Some(unquoted.replace("\\\"", "\"").replace("\\\\", "\\"));
        }

        return Some(value.to_string());
    }

    None
}

fn extract_codex_contextstream_value(content: &str, key: &str) -> Option<String> {
    let marker = "[mcp_servers.contextstream]";
    let env_marker = "[mcp_servers.contextstream.env]";
    let mut in_contextstream_section = false;

    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('[') && trimmed.ends_with(']') {
            if trimmed == marker {
                in_contextstream_section = true;
                continue;
            }

            if in_contextstream_section && trimmed != env_marker {
                break;
            }

            continue;
        }

        if !in_contextstream_section || trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }

        let Some((name, raw_value)) = trimmed.split_once('=') else {
            continue;
        };

        if name.trim() != key {
            continue;
        }

        let value = raw_value.trim();
        if value.starts_with('"') && value.ends_with('"') && value.len() >= 2 {
            let unquoted = &value[1..value.len() - 1];
            return Some(unquoted.replace("\\\"", "\"").replace("\\\\", "\\"));
        }

        return Some(value.to_string());
    }

    None
}

fn parse_codex_inline_table(raw: &str) -> std::collections::BTreeMap<String, String> {
    let mut map = std::collections::BTreeMap::new();
    let raw = raw.trim();
    if !(raw.starts_with('{') && raw.ends_with('}')) {
        return map;
    }

    let inner = &raw[1..raw.len() - 1];
    let mut parts = Vec::new();
    let mut current = String::new();
    let mut in_quotes = false;
    let mut escaped = false;

    for ch in inner.chars() {
        if escaped {
            current.push(ch);
            escaped = false;
            continue;
        }
        match ch {
            '\\' if in_quotes => {
                current.push(ch);
                escaped = true;
            }
            '"' => {
                current.push(ch);
                in_quotes = !in_quotes;
            }
            ',' if !in_quotes => {
                if !current.trim().is_empty() {
                    parts.push(current.trim().to_string());
                }
                current.clear();
            }
            _ => current.push(ch),
        }
    }

    if !current.trim().is_empty() {
        parts.push(current.trim().to_string());
    }

    for part in parts {
        let Some((raw_key, raw_value)) = part.split_once('=') else {
            continue;
        };
        let key = raw_key
            .trim()
            .trim_matches('"')
            .replace("\\\"", "\"")
            .replace("\\\\", "\\");
        let value = raw_value
            .trim()
            .trim_matches('"')
            .replace("\\\"", "\"")
            .replace("\\\\", "\\");
        map.insert(key, value);
    }

    map
}

fn extract_codex_contextstream_inline_table(
    content: &str,
    key: &str,
) -> std::collections::BTreeMap<String, String> {
    extract_codex_contextstream_value(content, key)
        .map(|raw| parse_codex_inline_table(&raw))
        .unwrap_or_default()
}

fn extract_codex_contextstream_http_header(content: &str, key: &str) -> Option<String> {
    extract_codex_contextstream_inline_table(content, "http_headers").remove(key)
}

fn extract_codex_env_map(content: &str) -> std::collections::BTreeMap<String, String> {
    let env_marker = "[mcp_servers.contextstream.env]";
    let mut in_env_section = false;
    let mut env = std::collections::BTreeMap::new();

    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('[') && trimmed.ends_with(']') {
            in_env_section = trimmed == env_marker;
            continue;
        }

        if !in_env_section || trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }

        let Some((name, raw_value)) = trimmed.split_once('=') else {
            continue;
        };

        let key = name.trim();
        let value = raw_value.trim();
        let parsed = if value.starts_with('"') && value.ends_with('"') && value.len() >= 2 {
            value[1..value.len() - 1]
                .replace("\\\"", "\"")
                .replace("\\\\", "\\")
        } else {
            value.to_string()
        };
        env.insert(key.to_string(), parsed);
    }

    env
}

/// Get VS Code settings path.
fn get_vscode_settings_path() -> Option<std::path::PathBuf> {
    #[cfg(target_os = "macos")]
    {
        dirs::home_dir().map(|h| {
            h.join("Library")
                .join("Application Support")
                .join("Code")
                .join("User")
                .join("settings.json")
        })
    }

    #[cfg(target_os = "windows")]
    {
        dirs::data_dir().map(|d| d.join("Code").join("User").join("settings.json"))
    }

    #[cfg(target_os = "linux")]
    {
        dirs::config_dir().map(|c| c.join("Code").join("User").join("settings.json"))
    }
}

/// Try to parse JSON-like content (with comments and trailing commas).
pub fn try_parse_json_like(content: &str) -> Result<Value> {
    let content = content.strip_prefix('\u{feff}').unwrap_or(content);

    // First try standard JSON
    if let Ok(value) = parse_value_without_duplicate_keys(content) {
        return Ok(value);
    }

    // Try stripping comments and trailing commas
    let stripped = strip_json_comments_checked(content)?;
    let cleaned = remove_trailing_commas(&stripped);

    parse_value_without_duplicate_keys(&cleaned)
        .map_err(|e| anyhow::anyhow!("Failed to parse JSON: {}", e))
}

fn strip_json_comments_checked(content: &str) -> Result<String> {
    let mut result = String::new();
    let mut chars = content.chars().peekable();
    let mut in_string = false;
    let mut escape = false;

    while let Some(c) = chars.next() {
        if escape {
            result.push(c);
            escape = false;
            continue;
        }

        if c == '\\' && in_string {
            result.push(c);
            escape = true;
            continue;
        }

        if c == '"' {
            in_string = !in_string;
            result.push(c);
            continue;
        }

        if !in_string && c == '/' {
            if let Some(&next) = chars.peek() {
                if next == '/' {
                    // Single-line comment - skip to end of line
                    chars.next();
                    while let Some(&nc) = chars.peek() {
                        if nc == '\n' {
                            break;
                        }
                        chars.next();
                    }
                    continue;
                } else if next == '*' {
                    // Multi-line comment - skip to */
                    chars.next();
                    // A block comment is trivia, not an empty string. Keep a
                    // separator so invalid token fusion such as `1/*c*/2` or
                    // `tr/*c*/ue` cannot be misparsed as `12` or `true`.
                    result.push(' ');
                    let mut terminated = false;
                    while let Some(nc) = chars.next() {
                        if nc == '\n' || nc == '\r' {
                            result.push(nc);
                        }
                        if nc == '*' {
                            if let Some(&ncc) = chars.peek() {
                                if ncc == '/' {
                                    chars.next();
                                    terminated = true;
                                    break;
                                }
                            }
                        }
                    }
                    if !terminated {
                        anyhow::bail!("Unterminated block comment in JSON-like configuration");
                    }
                    continue;
                }
            }
        }

        result.push(c);
    }

    if in_string {
        anyhow::bail!("Unterminated string in JSON-like configuration");
    }

    Ok(result)
}

/// Remove trailing commas from JSON.
fn remove_trailing_commas(content: &str) -> String {
    let mut result = String::new();
    let chars: Vec<char> = content.chars().collect();
    let len = chars.len();
    let mut i = 0;
    let mut in_string = false;
    let mut escape = false;

    while i < len {
        let c = chars[i];

        if escape {
            result.push(c);
            escape = false;
            i += 1;
            continue;
        }
        if in_string && c == '\\' {
            result.push(c);
            escape = true;
            i += 1;
            continue;
        }
        if c == '"' {
            in_string = !in_string;
            result.push(c);
            i += 1;
            continue;
        }

        if !in_string && c == ',' {
            // Look ahead to see if this comma is followed only by whitespace and then } or ]
            let mut j = i + 1;
            while j < len
                && (chars[j] == ' ' || chars[j] == '\t' || chars[j] == '\n' || chars[j] == '\r')
            {
                j += 1;
            }

            if j < len && (chars[j] == '}' || chars[j] == ']') {
                // This is a trailing comma - skip it
                i += 1;
                continue;
            }
        }

        result.push(c);
        i += 1;
    }

    result
}

/// Write project-level MCP configuration for an editor.
#[allow(dead_code)]
pub fn write_project_mcp_config(
    editor: &Editor,
    project_path: &Path,
    api_key: &str,
    workspace_id: Option<&str>,
    project_id: Option<&str>,
) -> Result<()> {
    write_project_mcp_config_with_remote_auth(
        editor,
        project_path,
        api_key,
        workspace_id,
        project_id,
        None,
        None,
        None,
    )
}

/// Write project-level MCP configuration and optionally override transcript defaults.
pub fn write_project_mcp_config_with_overrides(
    editor: &Editor,
    project_path: &Path,
    api_key: &str,
    workspace_id: Option<&str>,
    project_id: Option<&str>,
    transcripts_enabled: Option<bool>,
    hook_transcripts_enabled: Option<bool>,
) -> Result<()> {
    write_project_mcp_config_with_remote_auth(
        editor,
        project_path,
        api_key,
        workspace_id,
        project_id,
        transcripts_enabled,
        hook_transcripts_enabled,
        None,
    )
}

pub fn write_project_mcp_config_with_remote_auth(
    editor: &Editor,
    project_path: &Path,
    api_key: &str,
    workspace_id: Option<&str>,
    project_id: Option<&str>,
    transcripts_enabled: Option<bool>,
    hook_transcripts_enabled: Option<bool>,
    remote_auth_api_key: Option<&str>,
) -> Result<()> {
    write_project_mcp_config_with_transport_mode(
        editor,
        project_path,
        api_key,
        workspace_id,
        project_id,
        transcripts_enabled,
        hook_transcripts_enabled,
        TransportMode::PreserveExisting,
        remote_auth_api_key,
    )
}

pub fn write_project_mcp_config_force_local(
    editor: &Editor,
    project_path: &Path,
    api_key: &str,
    workspace_id: Option<&str>,
    project_id: Option<&str>,
    transcripts_enabled: Option<bool>,
    hook_transcripts_enabled: Option<bool>,
) -> Result<()> {
    write_project_mcp_config_with_transport_mode(
        editor,
        project_path,
        api_key,
        workspace_id,
        project_id,
        transcripts_enabled,
        hook_transcripts_enabled,
        TransportMode::ForceLocal,
        None,
    )
}

pub fn write_project_mcp_config_force_remote_with_auth(
    editor: &Editor,
    project_path: &Path,
    api_key: &str,
    workspace_id: Option<&str>,
    project_id: Option<&str>,
    transcripts_enabled: Option<bool>,
    hook_transcripts_enabled: Option<bool>,
    remote_auth_api_key: Option<&str>,
) -> Result<()> {
    write_project_mcp_config_with_transport_mode(
        editor,
        project_path,
        api_key,
        workspace_id,
        project_id,
        transcripts_enabled,
        hook_transcripts_enabled,
        TransportMode::ForceRemote,
        remote_auth_api_key,
    )
}

pub fn migrate_project_mcp_config(
    editor: &Editor,
    project_path: &Path,
    api_key: &str,
    workspace_id: Option<&str>,
    project_id: Option<&str>,
    transcripts_enabled: Option<bool>,
    hook_transcripts_enabled: Option<bool>,
) -> Result<()> {
    let transport_mode = TransportMode::ForceRemote;
    write_project_mcp_config_with_transport_mode(
        editor,
        project_path,
        api_key,
        workspace_id,
        project_id,
        transcripts_enabled,
        hook_transcripts_enabled,
        transport_mode,
        matches!(transport_mode, TransportMode::ForceRemote)
            .then_some(api_key)
            .filter(|value| !value.is_empty()),
    )
}

fn write_project_mcp_config_with_transport_mode(
    editor: &Editor,
    project_path: &Path,
    api_key: &str,
    workspace_id: Option<&str>,
    project_id: Option<&str>,
    transcripts_enabled: Option<bool>,
    hook_transcripts_enabled: Option<bool>,
    transport_mode: TransportMode,
    remote_auth_api_key: Option<&str>,
) -> Result<()> {
    let mcp_path = match editor.project_mcp_config_path(project_path) {
        Some(p) => p,
        None => return Ok(()), // Editor doesn't support project MCP config
    };
    let identity = ManagedConfigIdentity::for_write()?;
    let root_key = if matches!(editor, Editor::Copilot) {
        "servers"
    } else if matches!(editor, Editor::OpenCode | Editor::KiloCode) {
        "mcp"
    } else {
        "mcpServers"
    };

    // Read existing config or create new
    let loaded = safe_edit::read_for_edit(&mcp_path, safe_edit::JsonDialect::Strict)?;
    let mut config: Value = loaded.value.clone();
    require_object_if_present(
        config.get(root_key),
        &format!("project MCP root {}", root_key),
    )?;

    let existing_server = config
        .get(root_key)
        .and_then(|servers| servers.get("contextstream"))
        .cloned();
    validate_existing_json_like_server(
        existing_server.as_ref(),
        "project ContextStream MCP server entry",
    )?;

    let mut server = if matches!(editor, Editor::OpenCode) {
        if should_use_remote_http(editor, existing_server.as_ref(), transport_mode) {
            build_opencode_remote_server_json(existing_server.as_ref())
        } else {
            // Embed the actual api_key in opencode's `environment` block —
            // opencode does NOT expand `{env:VAR}` placeholders there, so a
            // placeholder would leave the spawned MCP child without credentials.
            build_opencode_server_json(existing_server.as_ref(), Some(api_key), None)
        }
    } else if matches!(editor, Editor::KiloCode) {
        if should_use_remote_http(editor, existing_server.as_ref(), transport_mode) {
            kilo_remote_server_json(build_remote_http_server_json(
                existing_server.as_ref(),
                workspace_id,
                project_id,
                transcripts_enabled,
                hook_transcripts_enabled,
                remote_auth_api_key,
            ))
        } else {
            build_kilo_server_json(
                existing_server.as_ref(),
                None,
                workspace_id,
                project_id,
                transcripts_enabled,
                hook_transcripts_enabled,
            )
        }
    } else {
        build_json_like_server_for_editor_with_identity(
            editor,
            existing_server.as_ref(),
            None,
            workspace_id,
            project_id,
            transcripts_enabled,
            hook_transcripts_enabled,
            transport_mode,
            remote_auth_api_key,
            &identity,
        )?
    };
    apply_managed_config_metadata(editor, &mut server, &identity);

    if let Some(mcp_servers) = config.get_mut(root_key).and_then(Value::as_object_mut) {
        mcp_servers.insert("contextstream".to_string(), server);
    } else {
        config[root_key] = json!({
            "contextstream": server
        });
    }

    safe_edit::commit(&mcp_path, &loaded, &config)?;
    record_configured_evidence(editor);

    Ok(())
}

/// Write workspace config to .contextstream/config.json in the project.
pub fn write_workspace_config(
    project_path: &Path,
    workspace_id: &str,
    workspace_name: &str,
    project_name: Option<&str>,
    project_id: Option<&str>,
) -> Result<()> {
    let config_path = project_path.join(".contextstream").join("config.json");

    // Read existing config or create new
    let loaded = safe_edit::read_for_edit(&config_path, safe_edit::JsonDialect::Strict)?;
    let mut config: Value = loaded.value.clone();
    let checkout_root = super::wizard_config::canonical_checkout_root(project_path);
    let association_changed = config.get("workspace_id").and_then(Value::as_str)
        != Some(workspace_id)
        || config.get("workspace_name").and_then(Value::as_str) != Some(workspace_name)
        || config.get("project_name").and_then(Value::as_str) != project_name
        || config.get("project_id").and_then(Value::as_str) != project_id
        || config.get("checkout_root").and_then(Value::as_str) != Some(checkout_root.as_str());
    let has_valid_associated_at = config
        .get("associated_at")
        .and_then(Value::as_str)
        .is_some_and(|value| chrono::DateTime::parse_from_rfc3339(value).is_ok());

    // Update workspace info
    config["workspace_id"] = json!(workspace_id);
    config["workspace_name"] = json!(workspace_name);
    if let Some(name) = project_name {
        config["project_name"] = json!(name);
    } else if let Some(obj) = config.as_object_mut() {
        obj.remove("project_name");
    }
    if let Some(id) = project_id {
        config["project_id"] = json!(id);
    } else if let Some(obj) = config.as_object_mut() {
        obj.remove("project_id");
    }
    config["checkout_root"] = json!(checkout_root);
    if association_changed || !has_valid_associated_at {
        config["associated_at"] = json!(chrono::Utc::now().to_rfc3339());
    }
    config["version"] = json!(mcp_types::config::VERSION);

    safe_edit::commit_with_removals(
        &config_path,
        &loaded,
        &config,
        &["project_id", "project_name"],
    )?;

    Ok(())
}

/// Remove the `contextstream` entry from an editor's global MCP config.
///
/// Reads the config, removes only the `contextstream` server key, and writes
/// back. Other MCP servers are preserved.
fn try_restore_exact_codex_backup(path: &Path, current: &str) -> Result<bool> {
    let backup_path = safe_edit::backup_path(path)?;
    if !backup_path
        .try_exists()
        .with_context(|| format!("Could not inspect backup {}", backup_path.display()))?
    {
        return Ok(false);
    }
    let backup = safe_edit::read_recovery_file(&backup_path)?
        .ok_or_else(|| anyhow::anyhow!("Backup {} disappeared", backup_path.display()))?;
    let mut expected = parse_codex_toml(&backup, &backup_path)?;
    if contextstream_toml_item(&expected).is_some_and(toml_item_is_contextstream_managed) {
        // A refresh snapshot is not the pre-install user state.
        return Ok(false);
    }
    let current_document = parse_codex_toml(current, path)?;
    let Some(current_item) = contextstream_toml_item(&current_document).cloned() else {
        return Ok(false);
    };
    if !toml_item_is_contextstream_managed(&current_item) {
        return Ok(false);
    }
    set_contextstream_toml_item(&mut expected, current_item)?;
    for project_path in managed_codex_trust_paths(&current_document) {
        set_managed_codex_project_trust(&mut expected, &project_path)?;
    }
    if render_codex_toml(&expected, &backup)? != current {
        return Ok(false);
    }

    safe_edit::restore_text_first_backup(path, current, true, &backup)
}

fn remove_contextstream_from_codex_toml(path: &Path) -> Result<()> {
    let content = std::fs::read_to_string(path)
        .with_context(|| format!("Could not read Codex config {}", path.display()))?;
    let current_document = parse_codex_toml(&content, path)?;
    let has_managed_server =
        contextstream_toml_item(&current_document).is_some_and(toml_item_is_contextstream_managed);
    let has_managed_trust = !managed_codex_trust_paths(&current_document).is_empty();
    if !has_managed_server && !has_managed_trust {
        // An unrelated same-name server must not become dependent on the
        // validity of a stale ContextStream recovery sidecar.
        return Ok(());
    }
    if try_restore_exact_codex_backup(path, &content)? {
        return Ok(());
    }

    let backup_path = safe_edit::backup_path(path)?;
    let backup = match safe_edit::read_recovery_file(&backup_path)? {
        Some(content) => {
            let document = parse_codex_toml(&content, &backup_path)?;
            Some((content, document))
        }
        None => None,
    };
    let (output, removed) =
        remove_contextstream_toml(&content, path, backup.as_ref().map(|(_, doc)| doc))?;
    if !removed {
        return Ok(());
    }
    let backup_is_wholly_managed = backup
        .as_ref()
        .is_some_and(|(content, document)| codex_document_is_wholly_managed(content, document));
    if output.trim().is_empty() && (backup.is_none() || backup_is_wholly_managed) {
        safe_edit::remove_owned_file_if_unchanged(path, &content)?;
        if let Some((backup_content, _)) = backup.filter(|_| backup_is_wholly_managed) {
            safe_edit::remove_owned_file_if_unchanged(&backup_path, &backup_content)?;
        }
    } else {
        safe_edit::write_if_unchanged(path, &output, Some(&content))?;
    }

    Ok(())
}

fn mcp_root_key(editor: &Editor) -> Option<&'static str> {
    match editor {
        Editor::Copilot => Some("servers"),
        Editor::ClaudeCode | Editor::Cursor | Editor::Windsurf | Editor::Antigravity => {
            Some("mcpServers")
        }
        Editor::OpenCode | Editor::KiloCode => Some("mcp"),
        Editor::Cline => Some("cline.mcpServers"),
        Editor::RooCode => Some("roo-cline.mcpServers"),
        Editor::Codex | Editor::Aider => None,
    }
}

fn mcp_json_dialect(editor: &Editor, path: &Path) -> safe_edit::JsonDialect {
    if matches!(editor, Editor::Cline | Editor::RooCode | Editor::KiloCode)
        || path.extension().and_then(|extension| extension.to_str()) == Some("jsonc")
    {
        safe_edit::JsonDialect::Jsonc
    } else {
        safe_edit::JsonDialect::Strict
    }
}

fn try_restore_exact_json_mcp_backup(
    path: &Path,
    editor: &Editor,
    current: &safe_edit::LoadedConfig,
) -> Result<bool> {
    let Some(root_key) = mcp_root_key(editor) else {
        return Ok(false);
    };
    let Some(current_server) = current
        .value
        .get(root_key)
        .and_then(Value::as_object)
        .and_then(|servers| servers.get("contextstream"))
        .cloned()
    else {
        return Ok(false);
    };
    if !json_like_server_is_contextstream_managed(&current_server) {
        return Ok(false);
    }

    let backup_path = safe_edit::backup_path(path)?;
    let Some(backup) =
        safe_edit::read_recovery_for_edit(&backup_path, mcp_json_dialect(editor, path))?
    else {
        return Ok(false);
    };
    if backup
        .value
        .get(root_key)
        .and_then(Value::as_object)
        .and_then(|servers| servers.get("contextstream"))
        .is_some_and(json_like_server_is_contextstream_managed)
    {
        // A refresh snapshot is not the pre-install user state.
        return Ok(false);
    }
    let mut expected = backup.value.clone();
    if let Some(servers) = expected.get_mut(root_key).and_then(Value::as_object_mut) {
        servers.insert("contextstream".to_string(), current_server);
    } else {
        expected[root_key] = json!({ "contextstream": current_server });
    }

    // These writers intentionally add bounded companion settings. Reproduce
    // them so exact comparison still proves there were no user edits.
    if matches!(editor, Editor::OpenCode) && expected.get("$schema").is_none() {
        expected["$schema"] = json!(OPENCODE_CONFIG_SCHEMA_URL);
    }
    if matches!(editor, Editor::KiloCode) {
        if expected.get("instructions").is_none() {
            expected["instructions"] = json!([".kilo/rules/*.md"]);
        }
        ensure_kilo_contextstream_permission(&mut expected)?;
    }

    let expected_raw = safe_edit::render(&backup, &expected)?;
    if expected_raw != current.raw {
        return Ok(false);
    }

    safe_edit::restore_first_backup(path, current, &backup.raw)
}

fn remove_empty_json_object_key(config: &mut Value, key: &str) -> bool {
    let should_remove = config
        .get(key)
        .and_then(Value::as_object)
        .is_some_and(serde_json::Map::is_empty);
    if should_remove {
        if let Some(object) = config.as_object_mut() {
            object.remove(key);
        }
    }
    should_remove
}

fn is_empty_json_config(config: &Value) -> bool {
    config.as_object().is_some_and(serde_json::Map::is_empty)
}

fn json_root_contains_only_server(config: &Value, root_key: &str, server_key: &str) -> bool {
    config
        .get(root_key)
        .and_then(Value::as_object)
        .is_some_and(|servers| servers.len() == 1 && servers.contains_key(server_key))
}

fn json_mcp_config_is_wholly_managed(editor: &Editor, loaded: &safe_edit::LoadedConfig) -> bool {
    if strip_json_comments_checked(&loaded.raw)
        .map(|without_comments| without_comments != loaded.raw)
        .unwrap_or(true)
    {
        return false;
    }
    let Some(object) = loaded.value.as_object() else {
        return false;
    };
    let Some(root_key) = mcp_root_key(editor) else {
        return false;
    };
    if !json_root_contains_only_server(&loaded.value, root_key, "contextstream") {
        return false;
    }
    if !loaded
        .value
        .get(root_key)
        .and_then(Value::as_object)
        .and_then(|servers| servers.get("contextstream"))
        .is_some_and(json_like_server_is_contextstream_managed)
    {
        return false;
    }

    match editor {
        Editor::OpenCode => {
            object
                .keys()
                .all(|key| matches!(key.as_str(), "mcp" | "$schema"))
                && object
                    .get("$schema")
                    .is_none_or(|value| value == OPENCODE_CONFIG_SCHEMA_URL)
        }
        Editor::KiloCode => {
            object
                .keys()
                .all(|key| matches!(key.as_str(), "mcp" | "instructions" | "permission"))
                && object
                    .get("instructions")
                    .is_none_or(|value| value == &json!([".kilo/rules/*.md"]))
                && object.get("permission").is_none_or(|value| {
                    value
                        == &json!({
                            KILO_CONTEXTSTREAM_PERMISSION_KEY: "allow"
                        })
                })
        }
        _ => object.len() == 1,
    }
}

fn cleanup_generated_json_companions(editor: &Editor, config: &mut Value) {
    let Some(object) = config.as_object_mut() else {
        return;
    };
    match editor {
        Editor::OpenCode
            if object
                .keys()
                .all(|key| matches!(key.as_str(), "mcp" | "$schema"))
                && object
                    .get("mcp")
                    .and_then(Value::as_object)
                    .is_none_or(serde_json::Map::is_empty)
                && object
                    .get("$schema")
                    .is_some_and(|value| value == OPENCODE_CONFIG_SCHEMA_URL) =>
        {
            object.remove("mcp");
            object.remove("$schema");
        }
        Editor::KiloCode
            if object
                .keys()
                .all(|key| matches!(key.as_str(), "instructions" | "permission" | "mcp"))
                && object
                    .get("mcp")
                    .and_then(Value::as_object)
                    .is_none_or(serde_json::Map::is_empty)
                && object
                    .get("instructions")
                    .is_none_or(|value| value == &json!([".kilo/rules/*.md"]))
                && object.get("permission").is_none_or(|value| {
                    value
                        == &json!({
                            KILO_CONTEXTSTREAM_PERMISSION_KEY: "allow"
                        })
                }) =>
        {
            object.remove("mcp");
            object.remove("instructions");
            object.remove("permission");
        }
        _ => {}
    }
}

fn revert_generated_json_companions_from_backup(
    editor: &Editor,
    backup: &safe_edit::LoadedConfig,
    config: &mut Value,
) -> Vec<&'static str> {
    let mut removed = Vec::new();
    let Some(object) = config.as_object_mut() else {
        return removed;
    };

    if matches!(editor, Editor::OpenCode)
        && backup.value.get("$schema").is_none()
        && object
            .get("$schema")
            .is_some_and(|value| value == OPENCODE_CONFIG_SCHEMA_URL)
    {
        object.remove("$schema");
        removed.push("$schema");
    }

    if matches!(editor, Editor::KiloCode) {
        if backup.value.get("instructions").is_none()
            && object
                .get("instructions")
                .is_some_and(|value| value == &json!([".kilo/rules/*.md"]))
        {
            object.remove("instructions");
            removed.push("instructions");
        }

        let backup_had_permission = backup
            .value
            .get("permission")
            .and_then(Value::as_object)
            .is_some_and(|permissions| permissions.contains_key(KILO_CONTEXTSTREAM_PERMISSION_KEY));
        if !backup_had_permission {
            let removed_permission = object
                .get_mut("permission")
                .and_then(Value::as_object_mut)
                .is_some_and(|permissions| {
                    if permissions.get(KILO_CONTEXTSTREAM_PERMISSION_KEY) == Some(&json!("allow")) {
                        permissions.remove(KILO_CONTEXTSTREAM_PERMISSION_KEY);
                        true
                    } else {
                        false
                    }
                });
            if removed_permission
                && backup.value.get("permission").is_none()
                && object
                    .get("permission")
                    .and_then(Value::as_object)
                    .is_some_and(serde_json::Map::is_empty)
            {
                object.remove("permission");
                removed.push("permission");
            }
        }
    }

    removed
}

pub fn remove_contextstream_from_mcp_config(editor: &Editor) -> Result<()> {
    // Aider has no MCP configuration surface. Its similarly located YAML file
    // belongs entirely to the user and must never be parsed as JSON or touched
    // by MCP uninstall.
    if matches!(editor, Editor::Aider) {
        return Ok(());
    }

    let Some(path) = editor.mcp_config_path() else {
        return Ok(());
    };
    if !path
        .try_exists()
        .with_context(|| format!("Could not inspect config {}", path.display()))?
    {
        return Ok(());
    }

    // Codex stores MCP servers in TOML, so it never goes through the JSON
    // reader below.
    if matches!(editor, Editor::Codex) {
        return remove_contextstream_from_codex_toml(&path);
    }

    let loaded = safe_edit::read_for_edit(&path, mcp_json_dialect(editor, &path))?;
    let root_key = mcp_root_key(editor).expect("non-Codex MCP editor has a JSON root key");
    let Some(current_server) = loaded
        .value
        .get(root_key)
        .and_then(Value::as_object)
        .and_then(|servers| servers.get("contextstream"))
    else {
        return Ok(());
    };
    if !json_like_server_is_contextstream_managed(current_server) {
        // Ownership must be established before reading a recovery sidecar. An
        // unrelated same-name server must not become dependent on stale or
        // malformed ContextStream backup state.
        return Ok(());
    }
    if try_restore_exact_json_mcp_backup(&path, editor, &loaded)? {
        return Ok(());
    }
    let backup_path = safe_edit::backup_path(&path)?;
    let backup = safe_edit::read_recovery_for_edit(&backup_path, mcp_json_dialect(editor, &path))?;
    let backup_exists = backup.is_some();
    let backup_is_wholly_managed = backup
        .as_ref()
        .is_some_and(|backup| json_mcp_config_is_wholly_managed(editor, backup));
    let current_is_wholly_managed = json_mcp_config_is_wholly_managed(editor, &loaded);
    let mut config: Value = loaded.value.clone();

    if backup
        .as_ref()
        .and_then(|backup| backup.value.get(root_key))
        .is_some_and(|root| !root.is_object())
    {
        anyhow::bail!(
            "Refusing to uninstall from {} because recovery backup {} has a non-object '{}' \
             value that cannot be restored without overwriting newer user changes",
            path.display(),
            backup_path.display(),
            root_key
        );
    }
    let original_server = backup
        .as_ref()
        .and_then(|backup| backup.value.get(root_key))
        .and_then(Value::as_object)
        .and_then(|servers| servers.get("contextstream"))
        .filter(|server| !json_like_server_is_contextstream_managed(server))
        .cloned();
    let restored_original_server = original_server.is_some();
    let changed = if let Some(original_server) = original_server {
        config
            .get_mut(root_key)
            .and_then(Value::as_object_mut)
            .expect("current MCP root was already read as an object")
            .insert("contextstream".to_string(), original_server);
        true
    } else {
        config
            .get_mut(root_key)
            .and_then(Value::as_object_mut)
            .and_then(|servers| servers.remove("contextstream"))
            .is_some()
    };

    if changed {
        let may_remove_generated_companions = !restored_original_server
            && (backup_is_wholly_managed || (!backup_exists && current_is_wholly_managed));
        let mut removed_top_level_keys = Vec::new();
        if may_remove_generated_companions {
            let companion_candidates: &[&str] = match editor {
                Editor::OpenCode => &["mcp", "$schema"],
                Editor::KiloCode => &["mcp", "instructions", "permission"],
                _ => &[],
            };
            let present_before: Vec<&str> = companion_candidates
                .iter()
                .copied()
                .filter(|key| config.get(*key).is_some())
                .collect();
            cleanup_generated_json_companions(editor, &mut config);
            removed_top_level_keys.extend(
                present_before
                    .into_iter()
                    .filter(|key| config.get(*key).is_none()),
            );
            if let Some(root_key) = mcp_root_key(editor) {
                if remove_empty_json_object_key(&mut config, root_key) {
                    removed_top_level_keys.push(root_key);
                }
            }
        } else if let Some(backup) = backup.as_ref() {
            removed_top_level_keys.extend(revert_generated_json_companions_from_backup(
                editor,
                backup,
                &mut config,
            ));
            if !restored_original_server
                && backup.value.get(root_key).is_none()
                && remove_empty_json_object_key(&mut config, root_key)
            {
                removed_top_level_keys.push(root_key);
            }
        }
        if !loaded.nonstandard_syntax
            && may_remove_generated_companions
            && is_empty_json_config(&config)
        {
            safe_edit::remove_owned_file_if_unchanged(&path, &loaded.raw)?;
        } else {
            safe_edit::commit_with_removals(&path, &loaded, &config, &removed_top_level_keys)?;
        }
        if backup_is_wholly_managed {
            if let Some(backup) = backup {
                safe_edit::remove_owned_file_if_unchanged(&backup_path, &backup.raw)?;
            }
        }
    }

    Ok(())
}

fn remove_contextstream_from_json_path(
    path: &Path,
    root_key: &str,
    server_key: &str,
) -> Result<bool> {
    if !path
        .try_exists()
        .with_context(|| format!("Could not inspect config {}", path.display()))?
    {
        return Ok(false);
    }

    let loaded = safe_edit::read_for_edit(path, safe_edit::JsonDialect::Strict)?;
    let Some(current_server) = loaded
        .value
        .get(root_key)
        .and_then(Value::as_object)
        .and_then(|servers| servers.get(server_key))
    else {
        return Ok(false);
    };
    if !json_like_server_is_contextstream_managed(current_server) {
        // Do not parse, trust, or otherwise make this unrelated server depend
        // on a recovery file ContextStream has no reason to consume.
        return Ok(false);
    }
    if try_restore_exact_json_path_backup(path, root_key, server_key, &loaded)? {
        return Ok(true);
    }
    let backup_path = safe_edit::backup_path(path)?;
    let backup = safe_edit::read_recovery_for_edit(&backup_path, safe_edit::JsonDialect::Strict)?;
    let backup_exists = backup.is_some();
    let backup_is_wholly_managed = backup.as_ref().is_some_and(|backup| {
        strip_json_comments_checked(&backup.raw)
            .is_ok_and(|without_comments| without_comments == backup.raw)
            && backup.value.as_object().is_some_and(|object| {
                object.len() == 1
                    && json_root_contains_only_server(&backup.value, root_key, server_key)
            })
            && backup
                .value
                .get(root_key)
                .and_then(Value::as_object)
                .and_then(|servers| servers.get(server_key))
                .is_some_and(json_like_server_is_contextstream_managed)
    });
    let current_is_wholly_managed = strip_json_comments_checked(&loaded.raw)
        .is_ok_and(|without_comments| without_comments == loaded.raw)
        && loaded.value.as_object().is_some_and(|object| {
            object.len() == 1 && json_root_contains_only_server(&loaded.value, root_key, server_key)
        })
        && loaded
            .value
            .get(root_key)
            .and_then(Value::as_object)
            .and_then(|servers| servers.get(server_key))
            .is_some_and(json_like_server_is_contextstream_managed);
    let mut config: Value = loaded.value.clone();
    if backup
        .as_ref()
        .and_then(|backup| backup.value.get(root_key))
        .is_some_and(|root| !root.is_object())
    {
        anyhow::bail!(
            "Refusing to uninstall from {} because recovery backup {} has a non-object '{}' \
             value that cannot be restored without overwriting newer user changes",
            path.display(),
            backup_path.display(),
            root_key
        );
    }
    let original_server = backup
        .as_ref()
        .and_then(|backup| backup.value.get(root_key))
        .and_then(Value::as_object)
        .and_then(|servers| servers.get(server_key))
        .filter(|server| !json_like_server_is_contextstream_managed(server))
        .cloned();
    let restored_original_server = original_server.is_some();
    let changed = if let Some(original_server) = original_server {
        config
            .get_mut(root_key)
            .and_then(Value::as_object_mut)
            .expect("current MCP root was already read as an object")
            .insert(server_key.to_string(), original_server);
        true
    } else {
        config
            .get_mut(root_key)
            .and_then(Value::as_object_mut)
            .and_then(|servers| servers.remove(server_key))
            .is_some()
    };

    if changed {
        // Delete empty files after cleanup (only if no other content remains).
        let may_delete_generated = !restored_original_server
            && (backup_is_wholly_managed || (!backup_exists && current_is_wholly_managed));
        let backup_had_root = backup
            .as_ref()
            .is_some_and(|backup| backup.value.get(root_key).is_some());
        let removed_root = !restored_original_server
            && (may_delete_generated || (backup_exists && !backup_had_root))
            && remove_empty_json_object_key(&mut config, root_key);

        if is_empty_json_config(&config) && may_delete_generated && !loaded.nonstandard_syntax {
            safe_edit::remove_owned_file_if_unchanged(path, &loaded.raw)?;
        } else {
            let removed_keys = removed_root
                .then_some(root_key)
                .into_iter()
                .collect::<Vec<_>>();
            safe_edit::commit_with_removals(path, &loaded, &config, &removed_keys)?;
        }
        if backup_is_wholly_managed {
            if let Some(backup) = backup {
                safe_edit::remove_owned_file_if_unchanged(&backup_path, &backup.raw)?;
            }
        }
    }

    Ok(changed)
}

fn try_restore_exact_json_path_backup(
    path: &Path,
    root_key: &str,
    server_key: &str,
    current: &safe_edit::LoadedConfig,
) -> Result<bool> {
    let Some(current_server) = current
        .value
        .get(root_key)
        .and_then(Value::as_object)
        .and_then(|servers| servers.get(server_key))
        .cloned()
    else {
        return Ok(false);
    };
    if !json_like_server_is_contextstream_managed(&current_server) {
        return Ok(false);
    }
    let backup_path = safe_edit::backup_path(path)?;
    let Some(backup) =
        safe_edit::read_recovery_for_edit(&backup_path, safe_edit::JsonDialect::Strict)?
    else {
        return Ok(false);
    };
    if backup
        .value
        .get(root_key)
        .and_then(Value::as_object)
        .and_then(|servers| servers.get(server_key))
        .is_some_and(json_like_server_is_contextstream_managed)
    {
        // A refresh snapshot is not the pre-install user state.
        return Ok(false);
    }
    let mut expected = backup.value.clone();
    if let Some(servers) = expected.get_mut(root_key).and_then(Value::as_object_mut) {
        servers.insert(server_key.to_string(), current_server);
    } else {
        expected[root_key] = json!({ server_key: current_server });
    }
    let expected_raw = safe_edit::render(&backup, &expected)?;
    if expected_raw != current.raw {
        return Ok(false);
    }
    safe_edit::restore_first_backup(path, current, &backup.raw)
}

fn project_mcp_cleanup_paths(editor: &Editor, project_path: &Path) -> Vec<(PathBuf, &'static str)> {
    let mut paths = Vec::new();
    if let Some(primary) = editor.project_mcp_config_path(project_path) {
        // Project MCP files use "mcpServers" by default.
        // Copilot uses VS Code's `.vscode/mcp.json` with "servers".
        let primary_key = if matches!(editor, Editor::Copilot) {
            "servers"
        } else if matches!(editor, Editor::OpenCode | Editor::KiloCode) {
            "mcp"
        } else {
            "mcpServers"
        };
        paths.push((primary, primary_key));
    }

    paths
}

/// Remove ContextStream from project-level MCP config files for an editor.
///
/// Preserves non-ContextStream config entries and removes stale Cursor legacy
/// `.vscode/mcp.json` entries when present.
pub fn remove_contextstream_from_project_mcp_config(
    editor: &Editor,
    project_path: &Path,
) -> Result<bool> {
    let mut removed = false;
    for (path, root_key) in project_mcp_cleanup_paths(editor, project_path) {
        removed |= remove_contextstream_from_json_path(&path, root_key, "contextstream")?;
    }
    Ok(removed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::env_test_mutex;
    use mcp_types::config::DEFAULT_API_URL;
    use std::ffi::OsString;

    struct EnvVarGuard {
        key: &'static str,
        previous: Option<OsString>,
    }

    impl EnvVarGuard {
        fn set(key: &'static str, value: impl AsRef<std::ffi::OsStr>) -> Self {
            let previous = std::env::var_os(key);
            std::env::set_var(key, value);
            Self { key, previous }
        }
    }

    impl Drop for EnvVarGuard {
        fn drop(&mut self) {
            match self.previous.take() {
                Some(previous) => std::env::set_var(self.key, previous),
                None => std::env::remove_var(self.key),
            }
        }
    }

    struct HomeGuard(Option<OsString>);

    impl HomeGuard {
        fn isolate_under(home: &Path) -> Self {
            let previous = std::env::var_os("HOME");
            std::env::set_var("HOME", home);
            Self(previous)
        }
    }

    impl Drop for HomeGuard {
        fn drop(&mut self) {
            match self.0.take() {
                Some(previous) => std::env::set_var("HOME", previous),
                None => std::env::remove_var("HOME"),
            }
        }
    }

    struct XdgConfigGuard(Option<OsString>);

    impl XdgConfigGuard {
        fn isolate_under(home: &Path) -> Self {
            let previous = std::env::var_os("XDG_CONFIG_HOME");
            std::env::set_var("XDG_CONFIG_HOME", home.join(".config"));
            Self(previous)
        }
    }

    impl Drop for XdgConfigGuard {
        fn drop(&mut self) {
            match self.0.take() {
                Some(previous) => std::env::set_var("XDG_CONFIG_HOME", previous),
                None => std::env::remove_var("XDG_CONFIG_HOME"),
            }
        }
    }

    struct DryRunGuard;

    impl DryRunGuard {
        fn enabled() -> Self {
            safe_edit::set_dry_run(true);
            Self
        }
    }

    impl Drop for DryRunGuard {
        fn drop(&mut self) {
            safe_edit::set_dry_run(false);
            let _ = safe_edit::take_planned_changes();
        }
    }

    struct ManagedIdentityPersistenceGuard;

    impl ManagedIdentityPersistenceGuard {
        fn enabled() -> Self {
            set_test_managed_identity_persistence(true);
            Self
        }
    }

    impl Drop for ManagedIdentityPersistenceGuard {
        fn drop(&mut self) {
            set_test_managed_identity_persistence(false);
        }
    }

    #[test]
    fn project_mcp_write_refuses_wrong_typed_containers_without_changing_bytes() {
        let project = tempfile::tempdir().expect("project");
        let path = project.path().join(".mcp.json");

        for original in [
            "{\n  \"mcpServers\": \"user sentinel\"\n}\n",
            "{\n  \"mcpServers\": {\"contextstream\": \"user sentinel\"}\n}\n",
            "{\n  \"mcpServers\": {\"contextstream\": {\"headers\": \"user sentinel\"}}\n}\n",
        ] {
            std::fs::write(&path, original).expect("seed config");
            let result = write_project_mcp_config_force_remote_with_auth(
                &Editor::ClaudeCode,
                project.path(),
                "test-key",
                Some("workspace"),
                Some("project"),
                None,
                None,
                Some("test-key"),
            );
            assert!(result.is_err(), "wrong-typed config unexpectedly succeeded");
            assert_eq!(
                std::fs::read_to_string(&path).expect("read preserved config"),
                original
            );
            assert!(
                !safe_edit::backup_path(&path).unwrap().exists(),
                "a refused edit must not create a recovery sidecar"
            );
        }
    }

    #[test]
    fn kilo_permission_scalar_is_not_replaced_with_an_object() {
        let mut config = json!({"permission": "user sentinel"});
        let original = config.clone();
        assert!(ensure_kilo_contextstream_permission(&mut config).is_err());
        assert_eq!(config, original);
    }

    /// VS Code settings.json is officially JSONC and is nearly always
    /// hand-maintained. Writing an MCP entry into it must not disturb a single
    /// byte the user wrote — this is the highest-blast-radius file we touch.
    #[test]
    fn vscode_settings_keep_comments_and_unrelated_keys() {
        let dir = std::env::temp_dir().join(format!("cs-vscode-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("settings.json");

        let original = r#"// My VS Code settings
{
  // fonts
  "editor.fontSize": 14,
  "editor.rulers": [80, 120],
  /* formatting */
  "editor.formatOnSave": true,
  "terminal.integrated.env.linux": { "FOO": "bar" }
}"#;
        std::fs::write(&path, original).unwrap();

        // Mirror what write_vscode_mcp_config does to the loaded document.
        let loaded = safe_edit::read_for_edit(&path, safe_edit::JsonDialect::Jsonc).unwrap();
        let mut section = match loaded.value.get("mcp") {
            Some(Value::Object(existing)) => Value::Object(existing.clone()),
            _ => json!({}),
        };
        section["contextstream"] = json!({ "command": "contextstream-mcp" });
        let updated = safe_edit::set_top_level_key(&loaded.raw, "mcp", &section).unwrap();
        safe_edit::write_if_changed(&path, &updated).unwrap();

        let after = std::fs::read_to_string(&path).unwrap();
        assert!(
            after.starts_with("// My VS Code settings"),
            "header comment lost"
        );
        assert!(after.contains("// fonts"), "inline comment lost");
        assert!(after.contains("/* formatting */"), "block comment lost");
        assert!(
            after.contains("\"editor.rulers\": [80, 120]"),
            "array formatting was reflowed"
        );

        let parsed = try_parse_json_like(&after).unwrap();
        assert_eq!(parsed["editor.fontSize"], json!(14));
        assert_eq!(parsed["terminal.integrated.env.linux"]["FOO"], json!("bar"));
        assert_eq!(
            parsed["mcp"]["contextstream"]["command"],
            json!("contextstream-mcp")
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn unreadable_vscode_settings_are_never_overwritten() {
        let dir = std::env::temp_dir().join(format!("cs-vscode-bad-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("settings.json");
        let original = "{ this is not json at all";
        std::fs::write(&path, original).unwrap();

        assert!(safe_edit::read_for_edit(&path, safe_edit::JsonDialect::Jsonc).is_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), original);

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_build_mcp_server_json() {
        let _guard = env_test_mutex().lock().unwrap_or_else(|e| e.into_inner());
        // Pin to production URL so local_dev_api_url_override() doesn't interfere
        std::env::set_var("CONTEXTSTREAM_API_URL", DEFAULT_API_URL);
        let config = build_mcp_server_json("test-key");
        std::env::remove_var("CONTEXTSTREAM_API_URL");
        assert!(config.get("mcpServers").is_some());
        assert!(config["mcpServers"]["contextstream"].is_object());
        assert_eq!(
            config["mcpServers"]["contextstream"]["env"]["CONTEXTSTREAM_API_URL"]
                .as_str()
                .unwrap_or_default(),
            DEFAULT_API_URL
        );
        assert_eq!(
            config["mcpServers"]["contextstream"]["env"]["CONTEXTSTREAM_API_KEY"]
                .as_str()
                .unwrap_or_default(),
            "test-key"
        );
        assert_eq!(
            config["mcpServers"]["contextstream"]["env"]["CONTEXTSTREAM_CONSOLIDATED"]
                .as_str()
                .unwrap_or_default(),
            "true"
        );
        assert_eq!(
            config["mcpServers"]["contextstream"]["env"]["CONTEXTSTREAM_TOOLSET"]
                .as_str()
                .unwrap_or_default(),
            DEFAULT_TOOLSET
        );
        assert_eq!(
            config["mcpServers"]["contextstream"]["env"][ENV_TRANSCRIPTS_ENABLED]
                .as_str()
                .unwrap_or_default(),
            "true"
        );
        assert_eq!(
            config["mcpServers"]["contextstream"]["env"][ENV_HOOK_TRANSCRIPTS_ENABLED]
                .as_str()
                .unwrap_or_default(),
            "true"
        );
        assert_eq!(
            config["mcpServers"]["contextstream"]["env"]
                ["CONTEXTSTREAM_INCLUDE_STRUCTURED_CONTENT"]
                .as_str()
                .unwrap_or_default(),
            "true"
        );
        assert_eq!(
            config["mcpServers"]["contextstream"]["env"]["CONTEXTSTREAM_SEARCH_LIMIT"]
                .as_str()
                .unwrap_or_default(),
            DEFAULT_SEARCH_LIMIT
        );
        assert_eq!(
            config["mcpServers"]["contextstream"]["env"]["CONTEXTSTREAM_SEARCH_MAX_CHARS"]
                .as_str()
                .unwrap_or_default(),
            DEFAULT_SEARCH_MAX_CHARS
        );
    }

    #[test]
    fn test_build_codex_toml_config_includes_defaults() {
        let _guard = env_test_mutex().lock().unwrap_or_else(|e| e.into_inner());
        // Pin to production URL so local_dev_api_url_override() doesn't interfere
        std::env::set_var("CONTEXTSTREAM_API_URL", DEFAULT_API_URL);
        let config = build_codex_toml_config("test-key");
        std::env::remove_var("CONTEXTSTREAM_API_URL");
        assert!(config.contains(r#"CONTEXTSTREAM_API_URL = "https://api.contextstream.io""#));
        assert!(config.contains(r#"CONTEXTSTREAM_API_KEY = "test-key""#));
        assert!(config.contains(r#"CONTEXTSTREAM_TOOLSET = "complete""#));
        assert!(config.contains(r#"CONTEXTSTREAM_CONTEXT_PACK = "true""#));
        assert!(config.contains(r#"CONTEXTSTREAM_TRANSCRIPTS_ENABLED = "true""#));
        assert!(config.contains(r#"CONTEXTSTREAM_HOOK_TRANSCRIPTS_ENABLED = "true""#));
        assert!(config.contains(r#"CONTEXTSTREAM_INCLUDE_STRUCTURED_CONTENT = "true""#));
        assert!(config.contains(r#"CONTEXTSTREAM_SEARCH_LIMIT = "15""#));
        assert!(config.contains(r#"CONTEXTSTREAM_SEARCH_MAX_CHARS = "2400""#));
    }

    #[test]
    fn test_build_merged_env_json_preserves_workspace_and_updates_defaults() {
        let _guard = env_test_mutex().lock().unwrap_or_else(|e| e.into_inner());
        std::env::remove_var("CONTEXTSTREAM_API_URL");
        let existing = json!({
            "command": "contextstream-mcp",
            "args": [],
            "env": {
                "CONTEXTSTREAM_API_URL": "http://localhost:8080",
                "CONTEXTSTREAM_API_KEY": "old-key",
                "CONTEXTSTREAM_WORKSPACE_ID": "ws-123",
                "CONTEXTSTREAM_PROJECT_ID": "proj-123",
                "CONTEXTSTREAM_SEARCH_LIMIT": "3",
                "CONTEXTSTREAM_SEARCH_MAX_CHARS": "400",
                "CUSTOM_ENV": "keep-me"
            }
        });

        let merged =
            build_merged_env_json(Some(&existing), Some("new-key"), None, None, None, None);
        assert_eq!(
            merged["CONTEXTSTREAM_API_KEY"].as_str().unwrap_or_default(),
            "new-key"
        );
        assert_eq!(
            merged["CONTEXTSTREAM_WORKSPACE_ID"]
                .as_str()
                .unwrap_or_default(),
            "ws-123"
        );
        assert_eq!(
            merged["CONTEXTSTREAM_PROJECT_ID"]
                .as_str()
                .unwrap_or_default(),
            "proj-123"
        );
        assert_eq!(
            merged["CONTEXTSTREAM_API_URL"].as_str().unwrap_or_default(),
            "http://localhost:8080"
        );
        assert_eq!(
            merged["CONTEXTSTREAM_SEARCH_LIMIT"]
                .as_str()
                .unwrap_or_default(),
            "3"
        );
        assert_eq!(
            merged["CONTEXTSTREAM_SEARCH_MAX_CHARS"]
                .as_str()
                .unwrap_or_default(),
            "400"
        );
        assert_eq!(merged["CUSTOM_ENV"].as_str().unwrap_or_default(), "keep-me");
    }

    #[test]
    fn test_build_merged_env_json_prefers_env_api_url_override() {
        let _guard = env_test_mutex().lock().unwrap_or_else(|e| e.into_inner());
        let existing = json!({
            "command": "contextstream-mcp",
            "args": [],
            "env": {
                "CONTEXTSTREAM_API_URL": "https://api.contextstream.io"
            }
        });

        std::env::set_var("CONTEXTSTREAM_API_URL", "http://localhost:8080/");
        let merged =
            build_merged_env_json(Some(&existing), Some("new-key"), None, None, None, None);
        std::env::remove_var("CONTEXTSTREAM_API_URL");

        assert_eq!(
            merged["CONTEXTSTREAM_API_URL"].as_str().unwrap_or_default(),
            "http://localhost:8080"
        );
    }

    #[test]
    fn test_build_merged_env_json_prefers_local_dev_override_over_existing_url() {
        let _guard = env_test_mutex().lock().unwrap_or_else(|e| e.into_inner());
        let existing = json!({
            "command": "contextstream-mcp",
            "args": [],
            "env": {
                "CONTEXTSTREAM_API_URL": "https://api.contextstream.io"
            }
        });

        std::env::remove_var("CONTEXTSTREAM_API_URL");
        let merged =
            build_merged_env_json(Some(&existing), Some("new-key"), None, None, None, None);
        assert_eq!(
            merged["CONTEXTSTREAM_API_URL"].as_str().unwrap_or_default(),
            local_dev_api_url_override()
                .as_deref()
                .unwrap_or("https://api.contextstream.io")
        );
        std::env::remove_var("CONTEXTSTREAM_API_URL");
    }

    #[test]
    fn test_build_merged_env_json_overrides_transcript_defaults_when_provided() {
        let _guard = env_test_mutex().lock().unwrap_or_else(|e| e.into_inner());
        std::env::remove_var("CONTEXTSTREAM_API_URL");
        let existing = json!({
            "command": "contextstream-mcp",
            "args": [],
            "env": {
                "CONTEXTSTREAM_TRANSCRIPTS_ENABLED": "false",
                "CONTEXTSTREAM_HOOK_TRANSCRIPTS_ENABLED": "false"
            }
        });

        let merged = build_merged_env_json(
            Some(&existing),
            Some("new-key"),
            None,
            None,
            Some(true),
            Some(true),
        );

        assert_eq!(
            merged[ENV_TRANSCRIPTS_ENABLED].as_str().unwrap_or_default(),
            "true"
        );
        assert_eq!(
            merged[ENV_HOOK_TRANSCRIPTS_ENABLED]
                .as_str()
                .unwrap_or_default(),
            "true"
        );
    }

    #[test]
    fn test_build_merged_env_json_omits_plaintext_api_key_when_disabled() {
        let _guard = env_test_mutex().lock().unwrap_or_else(|e| e.into_inner());
        std::env::remove_var("CONTEXTSTREAM_API_URL");
        let existing = json!({
            "command": "contextstream-mcp",
            "args": [],
            "env": {
                "CONTEXTSTREAM_API_KEY": "old-key",
                "CONTEXTSTREAM_WORKSPACE_ID": "ws-123"
            }
        });

        let merged = build_merged_env_json(Some(&existing), None, None, None, None, None);

        assert!(merged.get("CONTEXTSTREAM_API_KEY").is_none());
        assert_eq!(
            merged["CONTEXTSTREAM_WORKSPACE_ID"]
                .as_str()
                .unwrap_or_default(),
            "ws-123"
        );
    }

    #[test]
    fn test_build_json_like_server_for_copilot_remote_sets_openai_agentic_surface() {
        let server = build_json_like_server_for_editor(
            &Editor::Copilot,
            None,
            Some("test-key"),
            Some("ws-id"),
            Some("project-id"),
            None,
            None,
            TransportMode::ForceRemote,
            Some("remote-key"),
        )
        .unwrap();

        assert_eq!(server["type"].as_str(), Some("http"));
        assert_eq!(
            server["headers"][HEADER_TOOL_SURFACE_PROFILE].as_str(),
            Some(COPILOT_TOOL_SURFACE_PROFILE)
        );
    }

    #[test]
    fn test_build_json_like_server_for_copilot_local_sets_openai_agentic_surface() {
        let server = build_json_like_server_for_editor(
            &Editor::Copilot,
            None,
            Some("test-key"),
            Some("ws-id"),
            Some("project-id"),
            None,
            None,
            TransportMode::ForceLocal,
            None,
        )
        .unwrap();

        assert!(server.get("command").is_some());
        assert_eq!(
            server["env"][ENV_TOOL_SURFACE_PROFILE].as_str(),
            Some(COPILOT_TOOL_SURFACE_PROFILE)
        );
    }

    #[test]
    fn test_build_json_like_server_for_copilot_preserves_explicit_surface_profile() {
        let mut existing = json!({
            "type": "http",
            "url": default_remote_mcp_url(),
            "headers": {}
        });
        existing["headers"][HEADER_TOOL_SURFACE_PROFILE] = json!("default");
        let server = build_json_like_server_for_editor(
            &Editor::Copilot,
            Some(&existing),
            Some("test-key"),
            None,
            None,
            None,
            None,
            TransportMode::ForceRemote,
            Some("remote-key"),
        )
        .unwrap();

        assert_eq!(
            server["headers"][HEADER_TOOL_SURFACE_PROFILE].as_str(),
            Some("default")
        );
    }

    #[test]
    fn test_extract_codex_env_value() {
        let config = r#"
[mcp_servers.contextstream]
command = "contextstream-mcp"
args = []

[mcp_servers.contextstream.env]
CONTEXTSTREAM_WORKSPACE_ID = "ws-abc"
CONTEXTSTREAM_PROJECT_ID = "proj-xyz"
"#;

        assert_eq!(
            extract_codex_env_value(config, "CONTEXTSTREAM_WORKSPACE_ID"),
            Some("ws-abc".to_string())
        );
        assert_eq!(
            extract_codex_env_value(config, "CONTEXTSTREAM_PROJECT_ID"),
            Some("proj-xyz".to_string())
        );
        assert_eq!(extract_codex_env_value(config, "MISSING"), None);
    }

    #[test]
    fn test_ensure_codex_project_trust_adds_entry() {
        let initial = r#"model = "gpt-5.3-codex"

[mcp_servers.contextstream]
command = "contextstream-mcp"
args = []
"#;
        let project_path = std::path::Path::new("/tmp/my-project");
        let section_header = r#"[projects."/tmp/my-project"]"#;
        let result =
            upsert_codex_project_trust_config(initial, project_path).expect("upsert trust");

        assert!(result.contains(section_header));
        assert!(
            result.contains("trust_level = \"trusted\""),
            "unexpected trust config:\n{result}"
        );
        assert!(result.contains(CODEX_MANAGED_TRUST_COMMENT));
        // Existing content preserved
        assert!(result.contains("model = \"gpt-5.3-codex\""));
        assert!(result.contains("[mcp_servers.contextstream]"));
    }

    #[test]
    fn test_ensure_codex_project_trust_keeps_single_canonical_entry() {
        let project_path = "/home/user/my-project";
        let content = r#"model = "gpt-5.3-codex"

[projects."/home/user/my-project"]
trust_level = "trusted"

[mcp_servers.contextstream]
command = "contextstream-mcp"
"#
        .to_string();

        let updated = upsert_codex_project_trust_config(&content, Path::new(project_path))
            .expect("upsert trust");
        assert_eq!(updated, content);
    }

    #[test]
    fn test_upsert_codex_project_trust_preserves_equivalent_windows_header() {
        let content = r#"model = "gpt-5.3-codex"

[projects."C:\\Users\\alice\\source\\repos"]
trust_level = "trusted"
"#;

        let updated =
            upsert_codex_project_trust_config(content, Path::new(r"C:\Users\alice\source\repos"))
                .expect("upsert trust");

        assert_eq!(updated, content);
    }

    #[test]
    fn test_upsert_codex_project_trust_rejects_malformed_toml() {
        let content = r#"model = "gpt-5.3-codex"

[projects."C:/Users/alice/source/repos"]
trust_level = "trusted"

[projects."C:\Users\alice\source\repos"]
trust_level = "trusted"
"#;

        let error =
            upsert_codex_project_trust_config(content, Path::new(r"C:\Users\alice\source\repos"))
                .expect_err("invalid TOML must fail closed");
        assert!(error.to_string().contains("not valid TOML"));
    }

    #[test]
    fn test_upsert_codex_project_trust_preserves_non_trust_settings() {
        let content = r#"model = "gpt-5.3-codex"

[projects."/tmp/my-project"]
custom = "value"
trust_level = "untrusted"
"#;

        let updated = upsert_codex_project_trust_config(content, Path::new("/tmp/my-project"))
            .expect("upsert trust");

        assert!(updated.contains("custom = \"value\""));
        assert!(
            updated.contains("trust_level = \"trusted\""),
            "unexpected trust config:\n{updated}"
        );
        assert!(updated.contains(CODEX_MANAGED_TRUST_COMMENT));
    }

    #[test]
    fn test_upsert_codex_project_trust_preserves_orphaned_contextstream_comments() {
        let content = r#"model = "gpt-5.4"

# ContextStream MCP Server Configuration
[projects."/srv/projects"]
trust_level = "trusted"

# ContextStream MCP Server Configuration

# ContextStream MCP Server Configuration
[projects."/srv/projects/super-productivity"]
trust_level = "trusted"

# ContextStream MCP Server Configuration
[mcp_servers.contextstream]
url = "https://mcp.contextstream.io/mcp?default_context_mode=fast"
"#;

        let updated = upsert_codex_project_trust_config(content, Path::new("/home/alice"))
            .expect("upsert trust");
        assert_eq!(
            updated
                .matches("# ContextStream MCP Server Configuration")
                .count(),
            4
        );
        assert!(updated.contains(CODEX_MANAGED_TRUST_COMMENT));
        assert!(updated.contains("[projects.\"/home/alice\"]"));
        assert!(updated.contains("[mcp_servers.contextstream]"));
    }

    #[test]
    fn test_try_parse_json_like() {
        let input = r#"{
            "key": "value",  // comment
            "array": [1, 2, 3,],  // trailing comma
        }"#;

        let result = try_parse_json_like(input);
        assert!(result.is_ok());
    }

    #[test]
    fn jsonc_trailing_comma_cleanup_never_changes_string_contents() {
        let input = r#"{
            // Force the JSONC path.
            "command": "printf ', }' && echo \", ]\"",
            "array": [1, 2,],
        }"#;

        let parsed = try_parse_json_like(input).expect("valid JSONC");
        assert_eq!(parsed["command"], json!("printf ', }' && echo \", ]\""));
        assert_eq!(parsed["array"], json!([1, 2]));
    }

    #[test]
    fn json_like_parser_rejects_unterminated_comments_and_trailing_garbage() {
        assert!(try_parse_json_like(r#"{"a": 1} /* never closed"#).is_err());
        assert!(try_parse_json_like(r#"{"a": 1} garbage"#).is_err());
        assert!(try_parse_json_like(r#"{"a": 1/* comment is trivia, not deletion */2}"#).is_err());
        assert!(try_parse_json_like(r#"{"a": tr/* split token */ue}"#).is_err());
    }

    #[test]
    fn json_like_parser_rejects_duplicate_keys_at_any_depth() {
        let top_level = try_parse_json_like(r#"{"a": 1, "a": 2}"#)
            .expect_err("duplicate top-level key must fail");
        assert!(top_level.to_string().contains("duplicate object key 'a'"));

        let nested = try_parse_json_like(
            r#"{
                // Force the JSONC path too.
                "hooks": {"BeforeTool": 1, "BeforeTool": 2},
            }"#,
        )
        .expect_err("duplicate nested key must fail");
        assert!(nested
            .to_string()
            .contains("duplicate object key 'BeforeTool'"));
    }

    #[test]
    fn test_remove_contextstream_from_project_cursor_paths() {
        let temp = tempfile::tempdir().expect("tempdir");
        let project = temp.path();
        let cursor_path = project.join(".cursor").join("mcp.json");
        std::fs::create_dir_all(cursor_path.parent().expect("cursor parent")).expect("mkdirs");

        std::fs::write(
            &cursor_path,
            r#"{
  "mcpServers": {
    "contextstream": {
      "command": "contextstream-mcp",
      "env": { "CONTEXTSTREAM_MANAGED_CONFIG_VERSION": "1" }
    },
    "other": { "command": "other-mcp" }
  }
}"#,
        )
        .expect("write cursor mcp");

        let removed = remove_contextstream_from_project_mcp_config(&Editor::Cursor, project)
            .expect("cleanup project mcp");
        assert!(removed, "expected to remove contextstream entries");

        let cursor = std::fs::read_to_string(&cursor_path).expect("read cursor mcp");
        assert!(!cursor.contains("\"contextstream\""));
        assert!(cursor.contains("\"other\""));
    }

    #[test]
    fn test_write_project_mcp_config_cursor_does_not_create_vscode_mcp() {
        let temp = tempfile::tempdir().expect("tempdir");
        let project = temp.path();
        let vscode_path = project.join(".vscode").join("mcp.json");

        write_project_mcp_config(
            &Editor::Cursor,
            project,
            "test-key",
            Some("ws-id"),
            Some("project-id"),
        )
        .expect("write project cursor mcp");

        assert!(
            !vscode_path.exists(),
            "Cursor project config should not create .vscode/mcp.json"
        );
    }

    #[test]
    fn test_write_project_mcp_config_omits_plaintext_api_key() {
        let temp = tempfile::tempdir().expect("tempdir");
        let project = temp.path();
        let cursor_path = project.join(".cursor").join("mcp.json");

        write_project_mcp_config(
            &Editor::Cursor,
            project,
            "test-key",
            Some("ws-id"),
            Some("project-id"),
        )
        .expect("write project cursor mcp");

        let content = std::fs::read_to_string(&cursor_path).expect("read cursor mcp");
        assert!(!content.contains("CONTEXTSTREAM_API_KEY"));
        // Cursor defaults to remote HTTP transport; workspace ID is carried in
        // the X-ContextStream-Workspace-Id header, not an environment variable.
        assert!(
            content.contains("CONTEXTSTREAM_WORKSPACE_ID") || content.contains(HEADER_WORKSPACE_ID),
            "expected workspace ID in either env var or HTTP header"
        );
    }

    #[test]
    fn test_write_project_mcp_config_copilot_uses_vscode_servers_shape() {
        let _guard = env_test_mutex().lock().unwrap_or_else(|e| e.into_inner());
        std::env::remove_var(ENV_VSCODE_MCP_MODE);
        let temp = tempfile::tempdir().expect("tempdir");
        let project = temp.path();
        let vscode_path = project.join(".vscode").join("mcp.json");

        write_project_mcp_config(
            &Editor::Copilot,
            project,
            "test-key",
            Some("ws-id"),
            Some("project-id"),
        )
        .expect("write project copilot mcp");

        let content = std::fs::read_to_string(&vscode_path).expect("read copilot vscode mcp");
        let value: serde_json::Value = serde_json::from_str(&content).expect("parse json");
        assert!(
            value.get("servers").is_some(),
            "expected VS Code servers root"
        );
        assert!(
            value
                .get("mcpServers")
                .and_then(serde_json::Value::as_object)
                .is_none(),
            "copilot project mcp should not use mcpServers root"
        );
        assert_eq!(
            value["servers"]["contextstream"]["type"].as_str(),
            Some("http")
        );
        assert_eq!(
            value["servers"]["contextstream"]["headers"][HEADER_TOOLSET].as_str(),
            Some(DEFAULT_TOOLSET)
        );
        assert_eq!(
            value["servers"]["contextstream"]["url"].as_str(),
            Some(default_remote_mcp_url().as_str())
        );
        assert_eq!(
            value["servers"]["contextstream"]["headers"][HEADER_OUTPUT_FORMAT].as_str(),
            Some(DEFAULT_OUTPUT_FORMAT)
        );
        assert_eq!(
            value["servers"]["contextstream"]["headers"][HEADER_SEARCH_LIMIT].as_str(),
            Some(DEFAULT_SEARCH_LIMIT)
        );
        assert_eq!(
            value["servers"]["contextstream"]["headers"][HEADER_SEARCH_MAX_CHARS].as_str(),
            Some(DEFAULT_SEARCH_MAX_CHARS)
        );
        assert_eq!(
            value["servers"]["contextstream"]["headers"][HEADER_TRANSCRIPTS_ENABLED].as_str(),
            Some("true")
        );
        assert_eq!(
            value["servers"]["contextstream"]["headers"][HEADER_CONSOLIDATED].as_str(),
            Some("true")
        );
        assert!(value["servers"]["contextstream"].get("command").is_none());
        assert!(!content.contains("CONTEXTSTREAM_API_KEY"));
    }

    #[test]
    fn test_build_remote_http_server_json_preserves_existing_header_overrides() {
        let existing = json!({
            "type": "http",
            "url": "https://mcp.contextstream.io/mcp",
            "headers": {
                HEADER_TRANSCRIPTS_ENABLED: "false"
            }
        });

        let server = build_remote_http_server_json(Some(&existing), None, None, None, None, None);

        assert_eq!(
            server["headers"][HEADER_TOOLSET].as_str(),
            Some(DEFAULT_TOOLSET)
        );
        assert_eq!(
            server["headers"][HEADER_TRANSCRIPTS_ENABLED].as_str(),
            Some("false")
        );
        assert_eq!(
            server["headers"][HEADER_HOOK_TRANSCRIPTS_ENABLED].as_str(),
            Some("true")
        );
        assert_eq!(
            server["headers"][HEADER_CONSOLIDATED].as_str(),
            Some("true")
        );
    }

    #[test]
    fn test_build_remote_http_server_json_applies_transcript_overrides() {
        let existing = json!({
            "type": "http",
            "url": "https://mcp.contextstream.io/mcp",
            "headers": {
                HEADER_TRANSCRIPTS_ENABLED: "true",
                HEADER_HOOK_TRANSCRIPTS_ENABLED: "true"
            }
        });

        let server = build_remote_http_server_json(
            Some(&existing),
            None,
            None,
            Some(false),
            Some(false),
            None,
        );

        assert_eq!(
            server["headers"][HEADER_TRANSCRIPTS_ENABLED].as_str(),
            Some("false")
        );
        assert_eq!(
            server["headers"][HEADER_HOOK_TRANSCRIPTS_ENABLED].as_str(),
            Some("false")
        );
    }

    #[test]
    fn test_write_project_mcp_config_copilot_can_force_local_stdio() {
        let _guard = env_test_mutex().lock().unwrap_or_else(|e| e.into_inner());
        std::env::set_var(ENV_VSCODE_MCP_MODE, "local");
        std::env::set_var("CONTEXTSTREAM_API_URL", DEFAULT_API_URL);

        let temp = tempfile::tempdir().expect("tempdir");
        let project = temp.path();
        let vscode_path = project.join(".vscode").join("mcp.json");

        write_project_mcp_config(
            &Editor::Copilot,
            project,
            "test-key",
            Some("ws-id"),
            Some("project-id"),
        )
        .expect("write local copilot mcp");

        let content = std::fs::read_to_string(&vscode_path).expect("read local copilot mcp");
        let value: serde_json::Value = serde_json::from_str(&content).expect("parse json");
        let server = &value["servers"]["contextstream"];
        assert!(server["command"].as_str().is_some());
        assert_eq!(
            server["env"]["CONTEXTSTREAM_TOOLSET"].as_str(),
            Some(DEFAULT_TOOLSET)
        );
        assert_eq!(
            server["env"][ENV_TRANSCRIPTS_ENABLED].as_str(),
            Some("true")
        );
        assert_eq!(
            server["env"][ENV_HOOK_TRANSCRIPTS_ENABLED].as_str(),
            Some("true")
        );
        assert_eq!(
            server["env"]["CONTEXTSTREAM_SEARCH_LIMIT"].as_str(),
            Some(DEFAULT_SEARCH_LIMIT)
        );
        assert_eq!(
            server["env"]["CONTEXTSTREAM_SEARCH_MAX_CHARS"].as_str(),
            Some(DEFAULT_SEARCH_MAX_CHARS)
        );
        assert!(server.get("url").is_none());
        assert!(!content.contains("CONTEXTSTREAM_API_KEY"));

        std::env::remove_var("CONTEXTSTREAM_API_URL");
        std::env::remove_var(ENV_VSCODE_MCP_MODE);
    }

    #[test]
    fn test_write_project_mcp_config_preserves_existing_remote_headers() {
        let _guard = env_test_mutex().lock().unwrap_or_else(|e| e.into_inner());
        std::env::set_var("CONTEXTSTREAM_API_URL", DEFAULT_API_URL);

        let temp = tempfile::tempdir().expect("tempdir");
        let project = temp.path();
        let vscode_dir = project.join(".vscode");
        std::fs::create_dir_all(&vscode_dir).expect("create vscode dir");
        let vscode_path = vscode_dir.join("mcp.json");
        std::fs::write(
            &vscode_path,
            serde_json::to_string_pretty(&json!({
                "servers": {
                    "contextstream": {
                        "type": "http",
                        "url": DEFAULT_REMOTE_MCP_URL,
                        "headers": {
                            HEADER_TRANSCRIPTS_ENABLED: "false",
                            HEADER_HOOK_TRANSCRIPTS_ENABLED: "false"
                        }
                    }
                }
            }))
            .expect("serialize existing config"),
        )
        .expect("write existing project config");

        write_project_mcp_config(
            &Editor::Copilot,
            project,
            "test-key",
            Some("ws-id"),
            Some("project-id"),
        )
        .expect("rewrite project copilot config");

        let content = std::fs::read_to_string(&vscode_path).expect("read project copilot mcp");
        let value: serde_json::Value = serde_json::from_str(&content).expect("parse json");
        let server = &value["servers"]["contextstream"];
        assert_eq!(server["type"].as_str(), Some("http"));
        assert_eq!(
            server["headers"][HEADER_TRANSCRIPTS_ENABLED].as_str(),
            Some("false")
        );
        assert_eq!(
            server["headers"][HEADER_HOOK_TRANSCRIPTS_ENABLED].as_str(),
            Some("false")
        );
        assert_eq!(server["url"].as_str(), Some(DEFAULT_REMOTE_MCP_URL));
        assert!(server.get("command").is_none());

        std::env::remove_var("CONTEXTSTREAM_API_URL");
    }

    #[test]
    fn test_write_project_mcp_config_preserves_existing_local_env_overrides() {
        let temp = tempfile::tempdir().expect("tempdir");
        let project = temp.path();
        let mcp_path = project.join(".mcp.json");
        std::fs::write(
            &mcp_path,
            serde_json::to_string_pretty(&json!({
                "mcpServers": {
                    "contextstream": {
                        "command": "/usr/local/bin/contextstream-mcp",
                        "args": [],
                        "env": {
                            ENV_TRANSCRIPTS_ENABLED: "false",
                            ENV_HOOK_TRANSCRIPTS_ENABLED: "false",
                            "CONTEXTSTREAM_SEARCH_LIMIT": DEFAULT_SEARCH_LIMIT
                        }
                    }
                }
            }))
            .expect("serialize existing config"),
        )
        .expect("write existing local project config");

        write_project_mcp_config(
            &Editor::ClaudeCode,
            project,
            "test-key",
            Some("ws-id"),
            Some("project-id"),
        )
        .expect("rewrite project claude config");

        let content = std::fs::read_to_string(&mcp_path).expect("read project claude mcp");
        let value: serde_json::Value = serde_json::from_str(&content).expect("parse json");
        let server = &value["mcpServers"]["contextstream"];
        assert_eq!(
            server["env"][ENV_TRANSCRIPTS_ENABLED].as_str(),
            Some("false")
        );
        assert_eq!(
            server["env"][ENV_HOOK_TRANSCRIPTS_ENABLED].as_str(),
            Some("false")
        );
    }

    #[test]
    fn test_write_mcp_config_claude_preserves_existing_local_env_overrides() {
        let _guard = env_test_mutex().lock().unwrap_or_else(|e| e.into_inner());
        let temp = tempfile::tempdir().expect("tempdir");
        let previous_home = std::env::var_os("HOME");
        std::env::set_var("HOME", temp.path());
        let _xdg_guard = XdgConfigGuard::isolate_under(temp.path());

        let claude_dir = temp.path().join(".claude");
        std::fs::create_dir_all(&claude_dir).expect("create claude dir");
        let config_path = claude_dir.join("mcp.json");
        std::fs::write(
            &config_path,
            serde_json::to_string_pretty(&json!({
                "mcpServers": {
                    "contextstream": {
                        "command": "/usr/local/bin/contextstream-mcp",
                        "args": [],
                        "env": {
                            ENV_TRANSCRIPTS_ENABLED: "false",
                            ENV_HOOK_TRANSCRIPTS_ENABLED: "false",
                            ENV_SEARCH_LIMIT: "21",
                            ENV_SEARCH_MAX_CHARS: "4100"
                        }
                    }
                }
            }))
            .expect("serialize claude config"),
        )
        .expect("write claude config");

        write_mcp_config(
            &Editor::ClaudeCode,
            "test-key",
            Some("ws-id"),
            Some("project-id"),
        )
        .expect("update claude config");

        let content = std::fs::read_to_string(&config_path).expect("read claude config");
        let value: serde_json::Value = serde_json::from_str(&content).expect("parse json");
        let server = &value["mcpServers"]["contextstream"];
        assert_eq!(
            server["env"][ENV_TRANSCRIPTS_ENABLED].as_str(),
            Some("false")
        );
        assert_eq!(
            server["env"][ENV_HOOK_TRANSCRIPTS_ENABLED].as_str(),
            Some("false")
        );
        assert_eq!(server["env"][ENV_SEARCH_LIMIT].as_str(), Some("21"));
        assert_eq!(server["env"][ENV_SEARCH_MAX_CHARS].as_str(), Some("4100"));

        if let Some(value) = previous_home {
            std::env::set_var("HOME", value);
        } else {
            std::env::remove_var("HOME");
        }
    }

    #[test]
    fn test_write_mcp_config_claude_defaults_to_remote_when_no_existing_config() {
        let _guard = env_test_mutex().lock().unwrap_or_else(|e| e.into_inner());
        let temp = tempfile::tempdir().expect("tempdir");
        let previous_home = std::env::var_os("HOME");
        let previous_api_url = std::env::var_os("CONTEXTSTREAM_API_URL");
        std::env::set_var("HOME", temp.path());
        let _xdg_guard = XdgConfigGuard::isolate_under(temp.path());
        std::env::set_var("CONTEXTSTREAM_API_URL", DEFAULT_API_URL);

        write_mcp_config(&Editor::ClaudeCode, "test-key", Some("ws-id"), None)
            .expect("write claude config");

        let config_path = temp.path().join(".claude").join("mcp.json");
        let content = std::fs::read_to_string(&config_path).expect("read claude config");
        let value: serde_json::Value = serde_json::from_str(&content).expect("parse json");
        let server = &value["mcpServers"]["contextstream"];
        assert_eq!(server["type"].as_str(), Some("http"));
        assert_eq!(server["url"], json!(default_remote_mcp_url()));
        assert_eq!(
            server["headers"][HEADER_WORKSPACE_ID].as_str(),
            Some("ws-id")
        );
        assert!(
            server["headers"][HEADER_PROJECT_ID].is_null()
                || server
                    .get("headers")
                    .and_then(|headers| headers.as_object())
                    .map(|headers| !headers.contains_key(HEADER_PROJECT_ID))
                    .unwrap_or(false)
        );
        assert!(server.get("env").is_none());
        assert!(server.get("command").is_none());

        if let Some(value) = previous_api_url {
            std::env::set_var("CONTEXTSTREAM_API_URL", value);
        } else {
            std::env::remove_var("CONTEXTSTREAM_API_URL");
        }
        if let Some(value) = previous_home {
            std::env::set_var("HOME", value);
        } else {
            std::env::remove_var("HOME");
        }
    }

    #[test]
    fn test_write_mcp_config_with_remote_auth_embeds_api_key_header_for_claude() {
        let _guard = env_test_mutex().lock().unwrap_or_else(|e| e.into_inner());
        let temp = tempfile::tempdir().expect("tempdir");
        let previous_home = std::env::var_os("HOME");
        let previous_api_url = std::env::var_os("CONTEXTSTREAM_API_URL");
        std::env::set_var("HOME", temp.path());
        let _xdg_guard = XdgConfigGuard::isolate_under(temp.path());
        std::env::set_var("CONTEXTSTREAM_API_URL", DEFAULT_API_URL);

        write_mcp_config_with_remote_auth(
            &Editor::ClaudeCode,
            "test-key",
            Some("ws-id"),
            None,
            None,
            None,
            Some("test-key"),
        )
        .expect("write claude config");

        let config_path = temp.path().join(".claude").join("mcp.json");
        let content = std::fs::read_to_string(&config_path).expect("read claude config");
        let value: serde_json::Value = serde_json::from_str(&content).expect("parse json");
        let server = &value["mcpServers"]["contextstream"];
        assert_eq!(server["type"].as_str(), Some("http"));
        assert_eq!(server["headers"][HEADER_API_KEY].as_str(), Some("test-key"));
        assert!(server.get("env").is_none());
        assert!(server.get("command").is_none());

        if let Some(value) = previous_api_url {
            std::env::set_var("CONTEXTSTREAM_API_URL", value);
        } else {
            std::env::remove_var("CONTEXTSTREAM_API_URL");
        }
        if let Some(value) = previous_home {
            std::env::set_var("HOME", value);
        } else {
            std::env::remove_var("HOME");
        }
    }

    #[test]
    fn test_write_project_mcp_config_claude_defaults_to_remote_when_no_existing_config() {
        let _guard = env_test_mutex().lock().unwrap_or_else(|e| e.into_inner());
        let temp = tempfile::tempdir().expect("tempdir");
        let previous_api_url = std::env::var_os("CONTEXTSTREAM_API_URL");
        std::env::set_var("CONTEXTSTREAM_API_URL", DEFAULT_API_URL);

        write_project_mcp_config(
            &Editor::ClaudeCode,
            temp.path(),
            "test-key",
            Some("ws-id"),
            Some("project-id"),
        )
        .expect("write project claude config");

        let config_path = temp.path().join(".mcp.json");
        let content = std::fs::read_to_string(&config_path).expect("read project claude config");
        let value: serde_json::Value = serde_json::from_str(&content).expect("parse json");
        let server = &value["mcpServers"]["contextstream"];
        assert_eq!(server["type"].as_str(), Some("http"));
        assert_eq!(server["url"], json!(default_remote_mcp_url()));
        assert_eq!(
            server["headers"][HEADER_WORKSPACE_ID].as_str(),
            Some("ws-id")
        );
        assert_eq!(
            server["headers"][HEADER_PROJECT_ID].as_str(),
            Some("project-id")
        );
        assert!(server.get("env").is_none());
        assert!(server.get("command").is_none());

        if let Some(value) = previous_api_url {
            std::env::set_var("CONTEXTSTREAM_API_URL", value);
        } else {
            std::env::remove_var("CONTEXTSTREAM_API_URL");
        }
    }

    #[test]
    fn test_write_project_mcp_config_with_remote_auth_embeds_api_key_header_for_claude() {
        let _guard = env_test_mutex().lock().unwrap_or_else(|e| e.into_inner());
        let temp = tempfile::tempdir().expect("tempdir");
        let previous_api_url = std::env::var_os("CONTEXTSTREAM_API_URL");
        std::env::set_var("CONTEXTSTREAM_API_URL", DEFAULT_API_URL);

        write_project_mcp_config_with_remote_auth(
            &Editor::ClaudeCode,
            temp.path(),
            "test-key",
            Some("ws-id"),
            Some("project-id"),
            None,
            None,
            Some("test-key"),
        )
        .expect("write project claude config");

        let config_path = temp.path().join(".mcp.json");
        let content = std::fs::read_to_string(&config_path).expect("read project claude config");
        let value: serde_json::Value = serde_json::from_str(&content).expect("parse json");
        let server = &value["mcpServers"]["contextstream"];
        assert_eq!(server["type"].as_str(), Some("http"));
        assert_eq!(server["headers"][HEADER_API_KEY].as_str(), Some("test-key"));
        assert_eq!(
            server["headers"][HEADER_PROJECT_ID].as_str(),
            Some("project-id")
        );

        if let Some(value) = previous_api_url {
            std::env::set_var("CONTEXTSTREAM_API_URL", value);
        } else {
            std::env::remove_var("CONTEXTSTREAM_API_URL");
        }
    }

    #[test]
    fn test_sanitize_command_path_strips_deleted_suffix_when_live_binary_exists() {
        let temp = tempfile::tempdir().expect("tempdir");
        let live_binary = temp.path().join("contextstream-mcp");
        std::fs::write(&live_binary, b"#!/bin/sh\n").expect("write binary");

        let deleted_view = std::path::PathBuf::from(format!("{} (deleted)", live_binary.display()));

        assert_eq!(
            sanitize_command_path(&deleted_view).as_deref(),
            Some(live_binary.to_string_lossy().as_ref())
        );
    }

    #[test]
    fn deleted_binary_repair_only_changes_exact_executable_strings() {
        let mut server = json!({
            "command": "/usr/local/bin/contextstream-mcp (deleted)",
            "user_note": "my-contextstream-mcp (deleted) must stay",
            "exact_user_note": "/usr/local/bin/contextstream-mcp (deleted)",
            "args": ["contextstream-mcp (deleted) --extra"]
        });

        assert!(repair_deleted_binary_strings(&mut server));
        assert_eq!(server["command"], json!("/usr/local/bin/contextstream-mcp"));
        assert_eq!(
            server["user_note"],
            json!("my-contextstream-mcp (deleted) must stay")
        );
        assert_eq!(
            server["exact_user_note"],
            json!("/usr/local/bin/contextstream-mcp (deleted)")
        );
        assert_eq!(
            server["args"][0],
            json!("contextstream-mcp (deleted) --extra")
        );
    }

    #[test]
    fn legacy_ownership_requires_a_known_path_package_or_product_signature() {
        assert!(!json_like_server_is_contextstream_managed(&json!({
            "command": "/opt/user/contextstream-mcp"
        })));
        assert!(json_like_server_is_contextstream_managed(&json!({
            "command": "/usr/local/bin/contextstream-mcp"
        })));
        assert!(json_like_server_is_contextstream_managed(&json!({
            "command": "contextstream-mcp",
            "env": {"CONTEXTSTREAM_API_URL": "https://api.contextstream.io"}
        })));
        assert!(json_like_server_is_contextstream_managed(&json!({
            "command": "npx",
            "args": ["-y", "@contextstream/mcp-server"]
        })));
    }

    #[test]
    fn test_repair_deleted_binary_path_in_file_updates_project_config() {
        let temp = tempfile::tempdir().expect("tempdir");
        let config_path = temp.path().join(".mcp.json");
        std::fs::write(
            &config_path,
            r#"{
  "mcpServers": {
    "contextstream": {
      "command": "/usr/local/bin/contextstream-mcp (deleted)"
    }
  }
}"#,
        )
        .expect("write config");

        assert!(
            repair_deleted_binary_path_in_file(&config_path, Some(&Editor::ClaudeCode))
                .expect("repair config")
        );

        let repaired = std::fs::read_to_string(&config_path).expect("read repaired config");
        assert!(repaired.contains(r#""command": "/usr/local/bin/contextstream-mcp""#));
        assert!(!repaired.contains("(deleted)"));
    }

    #[test]
    fn deleted_binary_repair_never_parses_rules_only_aider_yaml_as_json() {
        let _guard = env_test_mutex().lock().unwrap_or_else(|e| e.into_inner());
        let temp = tempfile::tempdir().expect("tempdir");
        let _home_guard = HomeGuard::isolate_under(temp.path());
        let config_path = temp.path().join(".aider.conf.yml");
        let original = concat!(
            "# User-owned Aider configuration\n",
            "model: openai/gpt-5\n",
            "command-note: /usr/local/bin/contextstream-mcp (deleted)\n",
        );
        std::fs::write(&config_path, original).expect("seed Aider YAML");

        assert_eq!(
            repair_deleted_binary_path_configs(&[Editor::Aider], Some(temp.path()))
                .expect("skip rules-only config"),
            0
        );
        assert_eq!(
            std::fs::read_to_string(&config_path).expect("read Aider YAML"),
            original
        );
    }

    #[test]
    fn deleted_binary_repair_leaves_an_unowned_same_name_server_untouched() {
        let temp = tempfile::tempdir().expect("tempdir");
        let json_path = temp.path().join(".mcp.json");
        let json_original = concat!(
            "{\n",
            "  \"mcpServers\": {\n",
            "    \"contextstream\": {\n",
            "      \"command\": \"/opt/user/contextstream-mcp (deleted)\",\n",
            "      \"note\": \"user owned\"\n",
            "    }\n",
            "  }\n",
            "}\n"
        );
        std::fs::write(&json_path, json_original).expect("seed JSON config");
        assert!(
            !repair_deleted_binary_path_in_file(&json_path, Some(&Editor::ClaudeCode))
                .expect("inspect unowned JSON server")
        );
        assert_eq!(std::fs::read_to_string(&json_path).unwrap(), json_original);

        let toml_path = temp.path().join("config.toml");
        let toml_original = concat!(
            "[mcp_servers.contextstream]\n",
            "command = \"/opt/user/contextstream-mcp (deleted)\"\n",
            "note = \"user owned\"\n"
        );
        std::fs::write(&toml_path, toml_original).expect("seed TOML config");
        assert!(!repair_deleted_binary_path_in_codex_toml(&toml_path)
            .expect("inspect unowned TOML server"));
        assert_eq!(std::fs::read_to_string(&toml_path).unwrap(), toml_original);
    }

    #[test]
    fn deleted_binary_repair_does_not_touch_other_servers() {
        let temp = tempfile::tempdir().expect("tempdir");
        let config_path = temp.path().join(".mcp.json");
        let original = r#"{
  "mcpServers": {
    "private": {
      "command": "/usr/local/bin/contextstream-mcp (deleted)"
    },
    "contextstream": {
      "command": "/usr/local/bin/contextstream-mcp"
    }
  }
}"#;
        std::fs::write(&config_path, original).expect("write config");

        assert!(
            !repair_deleted_binary_path_in_file(&config_path, Some(&Editor::ClaudeCode))
                .expect("repair config")
        );
        assert_eq!(std::fs::read_to_string(config_path).unwrap(), original);
    }

    #[test]
    fn codex_deleted_binary_repair_changes_only_the_command_value() {
        let temp = tempfile::tempdir().expect("tempdir");
        let config_path = temp.path().join("config.toml");
        let original = concat!(
            "banner = \"contextstream-mcp (deleted) is user text\"\n",
            "\n",
            "[mcp_servers.contextstream]\n",
            "command = \"/usr/local/bin/contextstream-mcp (deleted)\" # keep comment\n",
            "note = \"contextstream-mcp (deleted) is also user text\"\n",
            "\n",
            "[mcp_servers.other]\n",
            "command = \"/other/contextstream-mcp (deleted)\"\n",
        );
        std::fs::write(&config_path, original).expect("seed Codex config");

        assert!(
            repair_deleted_binary_path_in_codex_toml(&config_path).expect("repair Codex command")
        );

        let repaired = std::fs::read_to_string(&config_path).unwrap();
        let parsed = repaired
            .parse::<DocumentMut>()
            .expect("valid repaired TOML");
        assert_eq!(
            parsed["mcp_servers"]["contextstream"]["command"]
                .as_value()
                .and_then(toml_edit::Value::as_str),
            Some("/usr/local/bin/contextstream-mcp")
        );
        assert_eq!(
            parsed["mcp_servers"]["contextstream"]["note"]
                .as_value()
                .and_then(toml_edit::Value::as_str),
            Some("contextstream-mcp (deleted) is also user text")
        );
        assert_eq!(
            parsed["mcp_servers"]["other"]["command"]
                .as_value()
                .and_then(toml_edit::Value::as_str),
            Some("/other/contextstream-mcp (deleted)")
        );
        assert!(repaired.contains("# keep comment"));
        assert!(repaired.contains("banner = \"contextstream-mcp (deleted) is user text\""));
    }

    #[test]
    fn test_build_opencode_server_json_uses_local_server_shape() {
        // No api_key supplied → builder should still emit a usable local config,
        // falling back to the `{env:...}` placeholder for the API key so the
        // user can supply it via the shell env.
        let server = build_opencode_server_json(None, None, None);
        assert_eq!(server["type"].as_str(), Some("local"));
        assert_eq!(server["enabled"].as_bool(), Some(true));
        // Command must be a non-empty array (managed local-helper path or shell
        // fallback) — the previous hardcoded `npx -y contextstream-mcp` path
        // pointed at a non-existent npm package and broke spawning.
        let command = server["command"]
            .as_array()
            .expect("opencode command should be an array");
        assert!(!command.is_empty(), "opencode command should not be empty");
        let first = command[0]
            .as_str()
            .expect("first command element should be a string");
        assert_ne!(
            first, "npx",
            "opencode local command must not invoke npx (the previous hardcoded \
             `npx -y contextstream-mcp` package does not exist)"
        );
        assert_eq!(
            server["environment"]["CONTEXTSTREAM_API_KEY"].as_str(),
            Some("{env:CONTEXTSTREAM_API_KEY}"),
            "when no api_key is supplied, builder should keep the env placeholder"
        );

        // With an api_key, the literal value must appear in the environment so
        // opencode (which does NOT expand `{env:...}` in mcp.*.environment) can
        // pass real credentials to the spawned MCP child.
        let server_with_key = build_opencode_server_json(None, Some("real-key-value"), None);
        assert_eq!(
            server_with_key["environment"]["CONTEXTSTREAM_API_KEY"].as_str(),
            Some("real-key-value")
        );
        assert_eq!(
            server["environment"]["CONTEXTSTREAM_TOOLSET"].as_str(),
            Some(DEFAULT_TOOLSET)
        );
        assert_eq!(
            server["environment"][ENV_TRANSCRIPTS_ENABLED].as_str(),
            Some("true")
        );
        assert_eq!(
            server["environment"][ENV_HOOK_TRANSCRIPTS_ENABLED].as_str(),
            Some("true")
        );
        assert_eq!(
            server["environment"]["CONTEXTSTREAM_SEARCH_LIMIT"].as_str(),
            Some(DEFAULT_SEARCH_LIMIT)
        );
        assert_eq!(
            server["environment"]["CONTEXTSTREAM_SEARCH_MAX_CHARS"].as_str(),
            Some(DEFAULT_SEARCH_MAX_CHARS)
        );
        assert!(
            server["environment"]["CONTEXTSTREAM_WORKSPACE_ID"].is_null(),
            "opencode environment should not inject workspace ids"
        );
    }

    #[test]
    fn test_write_project_mcp_config_opencode_uses_mcp_root_shape() {
        let temp = tempfile::tempdir().expect("tempdir");
        let project = temp.path();
        let opencode_path = project.join("opencode.json");

        write_project_mcp_config(
            &Editor::OpenCode,
            project,
            "test-key",
            Some("ws-id"),
            Some("project-id"),
        )
        .expect("write project opencode mcp");

        let content =
            std::fs::read_to_string(&opencode_path).expect("read opencode project config");
        let value: serde_json::Value = serde_json::from_str(&content).expect("parse json");
        assert!(value.get("mcp").is_some(), "expected OpenCode mcp root");
        assert!(
            value
                .get("mcpServers")
                .and_then(serde_json::Value::as_object)
                .is_none(),
            "opencode project config should not use mcpServers"
        );
        // OpenCode supports hosted remote MCP; ingest still runs through the
        // local helper when explicitly invoked by ContextStream tools.
        let server_type = value["mcp"]["contextstream"]["type"].as_str();
        assert!(
            matches!(server_type, Some("remote") | Some("local")),
            "expected opencode server type to be 'remote' or 'local', got {:?}",
            server_type
        );
        if server_type == Some("local") {
            // opencode local-mode env must carry the literal api_key value
            // (opencode does not expand `{env:VAR}` in mcp.*.environment).
            assert_eq!(
                value["mcp"]["contextstream"]["environment"]["CONTEXTSTREAM_API_KEY"].as_str(),
                Some("test-key")
            );
            // And the command must be a non-`npx` array — the previous
            // hardcoded `["npx", "-y", "contextstream-mcp"]` referenced a
            // non-existent npm package.
            let command = value["mcp"]["contextstream"]["command"]
                .as_array()
                .expect("opencode local command should be an array");
            assert!(
                !command.is_empty() && command[0].as_str().map(|s| s != "npx").unwrap_or(false),
                "expected opencode local command to use the resolved binary, not npx, got: {:?}",
                command
            );
        }
    }

    #[test]
    fn test_migrate_project_mcp_config_claude_converts_local_env_to_remote_headers() {
        let temp = tempfile::tempdir().expect("tempdir");
        let project = temp.path();
        let mcp_path = project.join(".mcp.json");
        std::fs::write(
            &mcp_path,
            serde_json::to_string_pretty(&json!({
                "mcpServers": {
                    "contextstream": {
                        "command": "/usr/local/bin/contextstream-mcp",
                        "args": [],
                        "env": {
                            ENV_TRANSCRIPTS_ENABLED: "false",
                            ENV_HOOK_TRANSCRIPTS_ENABLED: "false",
                            ENV_SEARCH_LIMIT: "22",
                            ENV_SEARCH_MAX_CHARS: "4200",
                            ENV_WORKSPACE_ID: "ws-id",
                            ENV_PROJECT_ID: "project-id"
                        }
                    }
                }
            }))
            .expect("serialize existing local config"),
        )
        .expect("write existing local config");

        migrate_project_mcp_config(
            &Editor::ClaudeCode,
            project,
            "",
            Some("ws-id"),
            Some("project-id"),
            None,
            None,
        )
        .expect("migrate project claude config");

        let content = std::fs::read_to_string(&mcp_path).expect("read migrated config");
        let value: serde_json::Value = serde_json::from_str(&content).expect("parse json");
        let server = &value["mcpServers"]["contextstream"];
        assert_eq!(server["type"].as_str(), Some("http"));
        assert_eq!(server["url"], json!(default_remote_mcp_url()));
        assert_eq!(
            server["headers"][HEADER_TRANSCRIPTS_ENABLED].as_str(),
            Some("false")
        );
        assert_eq!(
            server["headers"][HEADER_HOOK_TRANSCRIPTS_ENABLED].as_str(),
            Some("false")
        );
        assert_eq!(server["headers"][HEADER_SEARCH_LIMIT].as_str(), Some("22"));
        assert_eq!(
            server["headers"][HEADER_SEARCH_MAX_CHARS].as_str(),
            Some("4200")
        );
        assert_eq!(
            server["headers"][HEADER_WORKSPACE_ID].as_str(),
            Some("ws-id")
        );
        assert_eq!(
            server["headers"][HEADER_PROJECT_ID].as_str(),
            Some("project-id")
        );
        assert!(server.get("env").is_none());
        assert!(server.get("command").is_none());
    }

    #[test]
    fn test_migrate_mcp_config_codex_writes_remote_config_by_default() {
        let _guard = env_test_mutex().lock().unwrap_or_else(|e| e.into_inner());
        let temp = tempfile::tempdir().expect("tempdir");
        let previous_home = std::env::var_os("HOME");
        let previous_api_url = std::env::var_os("CONTEXTSTREAM_API_URL");
        std::env::set_var("HOME", temp.path());
        let _xdg_guard = XdgConfigGuard::isolate_under(temp.path());
        std::env::set_var("CONTEXTSTREAM_API_URL", DEFAULT_API_URL);

        migrate_mcp_config(
            &Editor::Codex,
            "",
            Some("ws-id"),
            Some("project-id"),
            None,
            None,
        )
        .expect("migrate codex config");

        let config_path = temp.path().join(".codex").join("config.toml");
        let content = std::fs::read_to_string(&config_path).expect("read codex config");
        assert!(content.contains("[mcp_servers.contextstream]"));
        assert!(content.contains(&format!(r#"url = "{}""#, default_remote_mcp_url())));
        assert!(content.contains("http_headers = {"));
        assert!(!content.contains("command = "));
        assert!(!content.contains("[mcp_servers.contextstream.env]"));
        if let Some(value) = previous_api_url {
            std::env::set_var("CONTEXTSTREAM_API_URL", value);
        } else {
            std::env::remove_var("CONTEXTSTREAM_API_URL");
        }
        if let Some(value) = previous_home {
            std::env::set_var("HOME", value);
        } else {
            std::env::remove_var("HOME");
        }
    }

    #[test]
    fn test_write_mcp_config_codex_defaults_to_hosted_remote_when_available() {
        let _guard = env_test_mutex().lock().unwrap_or_else(|e| e.into_inner());
        let temp = tempfile::tempdir().expect("tempdir");
        let previous_home = std::env::var_os("HOME");
        let previous_api_url = std::env::var_os("CONTEXTSTREAM_API_URL");
        std::env::set_var("HOME", temp.path());
        let _xdg_guard = XdgConfigGuard::isolate_under(temp.path());
        std::env::set_var("CONTEXTSTREAM_API_URL", DEFAULT_API_URL);

        write_mcp_config(&Editor::Codex, "test-key", Some("ws-id"), None)
            .expect("write codex config");

        let config_path = temp.path().join(".codex").join("config.toml");
        let content = std::fs::read_to_string(&config_path).expect("read codex config");
        assert!(content.contains("[mcp_servers.contextstream]"));
        assert!(content.contains(&format!(r#"url = "{}""#, default_remote_mcp_url())));
        assert!(content.contains("http_headers = {"));
        assert!(!content.contains("command = "));
        assert!(!content.contains("[mcp_servers.contextstream.env]"));

        if let Some(value) = previous_api_url {
            std::env::set_var("CONTEXTSTREAM_API_URL", value);
        } else {
            std::env::remove_var("CONTEXTSTREAM_API_URL");
        }
        if let Some(value) = previous_home {
            std::env::set_var("HOME", value);
        } else {
            std::env::remove_var("HOME");
        }
    }

    #[test]
    fn test_write_mcp_config_claude_can_force_local_when_requested() {
        let _guard = env_test_mutex().lock().unwrap_or_else(|e| e.into_inner());
        let temp = tempfile::tempdir().expect("tempdir");
        let previous_home = std::env::var_os("HOME");
        let previous_api_url = std::env::var_os("CONTEXTSTREAM_API_URL");
        std::env::set_var("HOME", temp.path());
        let _xdg_guard = XdgConfigGuard::isolate_under(temp.path());
        std::env::set_var("CONTEXTSTREAM_API_URL", DEFAULT_API_URL);

        write_mcp_config_force_local(
            &Editor::ClaudeCode,
            "test-key",
            Some("ws-id"),
            Some("project-id"),
            None,
            None,
        )
        .expect("write claude config");

        let config_path = temp.path().join(".claude").join("mcp.json");
        let content = std::fs::read_to_string(&config_path).expect("read claude config");
        assert!(content.contains(r#""command":"#));
        assert!(content.contains(r#""env":"#));
        assert!(!content.contains(r#""type": "http""#));
        assert!(!content
            .contains(r#""url": "https://mcp.contextstream.io/mcp?default_context_mode=fast""#));

        if let Some(value) = previous_api_url {
            std::env::set_var("CONTEXTSTREAM_API_URL", value);
        } else {
            std::env::remove_var("CONTEXTSTREAM_API_URL");
        }
        if let Some(value) = previous_home {
            std::env::set_var("HOME", value);
        } else {
            std::env::remove_var("HOME");
        }
    }

    #[test]
    fn test_write_mcp_config_codex_can_force_local_when_requested() {
        let _guard = env_test_mutex().lock().unwrap_or_else(|e| e.into_inner());
        let temp = tempfile::tempdir().expect("tempdir");
        let previous_home = std::env::var_os("HOME");
        let previous_api_url = std::env::var_os("CONTEXTSTREAM_API_URL");
        std::env::set_var("HOME", temp.path());
        let _xdg_guard = XdgConfigGuard::isolate_under(temp.path());
        std::env::set_var("CONTEXTSTREAM_API_URL", DEFAULT_API_URL);

        write_mcp_config_force_local(
            &Editor::Codex,
            "test-key",
            Some("ws-id"),
            Some("project-id"),
            None,
            None,
        )
        .expect("write codex config");

        let config_path = temp.path().join(".codex").join("config.toml");
        let content = std::fs::read_to_string(&config_path).expect("read codex config");
        assert!(content.contains("command = "));
        assert!(content.contains("[mcp_servers.contextstream.env]"));
        assert!(!content.contains(&format!(r#"url = "{}""#, default_remote_mcp_url())));

        if let Some(value) = previous_api_url {
            std::env::set_var("CONTEXTSTREAM_API_URL", value);
        } else {
            std::env::remove_var("CONTEXTSTREAM_API_URL");
        }
        if let Some(value) = previous_home {
            std::env::set_var("HOME", value);
        } else {
            std::env::remove_var("HOME");
        }
    }

    #[test]
    fn test_editor_supports_remote_mcp_codex_is_enabled() {
        assert!(editor_supports_remote_mcp(&Editor::Codex));
    }

    #[test]
    fn test_write_mcp_config_codex_preserves_remote_transport_on_update() {
        let _guard = env_test_mutex().lock().unwrap_or_else(|e| e.into_inner());
        let temp = tempfile::tempdir().expect("tempdir");
        let previous_home = std::env::var_os("HOME");
        let previous_api_url = std::env::var_os("CONTEXTSTREAM_API_URL");
        std::env::set_var("HOME", temp.path());
        let _xdg_guard = XdgConfigGuard::isolate_under(temp.path());
        std::env::set_var("CONTEXTSTREAM_API_URL", DEFAULT_API_URL);

        let codex_dir = temp.path().join(".codex");
        std::fs::create_dir_all(&codex_dir).expect("create codex dir");
        let config_path = codex_dir.join("config.toml");
        std::fs::write(
            &config_path,
            format!(
                r#"# ContextStream MCP Server Configuration
[mcp_servers.contextstream]
url = "{}"
"#,
                default_remote_mcp_url()
            ),
        )
        .expect("write codex config");

        write_mcp_config(
            &Editor::Codex,
            "test-key",
            Some("ws-id"),
            Some("project-id"),
        )
        .expect("rewrite codex config");

        let content = std::fs::read_to_string(&config_path).expect("read codex config");
        assert!(content.contains(&format!(r#"url = "{}""#, default_remote_mcp_url())));
        assert!(content.contains("http_headers = {"));
        assert!(!content.contains("command = "));
        assert!(!content.contains("[mcp_servers.contextstream.env]"));

        if let Some(value) = previous_api_url {
            std::env::set_var("CONTEXTSTREAM_API_URL", value);
        } else {
            std::env::remove_var("CONTEXTSTREAM_API_URL");
        }
        if let Some(value) = previous_home {
            std::env::set_var("HOME", value);
        } else {
            std::env::remove_var("HOME");
        }
    }

    #[test]
    fn test_migrate_project_mcp_config_opencode_uses_remote_shape() {
        let _guard = env_test_mutex().lock().unwrap_or_else(|e| e.into_inner());
        let temp = tempfile::tempdir().expect("tempdir");
        let project = temp.path();
        let opencode_path = project.join("opencode.json");
        std::fs::write(
            &opencode_path,
            serde_json::to_string_pretty(&json!({
                "mcp": {
                    "contextstream": {
                        "type": "local",
                        "command": ["npx", "-y", "contextstream-mcp"],
                        "environment": {
                            "CONTEXTSTREAM_API_KEY": "{env:CONTEXTSTREAM_API_KEY}"
                        },
                        "enabled": true
                    }
                }
            }))
            .expect("serialize existing opencode config"),
        )
        .expect("write existing opencode config");

        migrate_project_mcp_config(
            &Editor::OpenCode,
            project,
            "",
            Some("ws-id"),
            Some("project-id"),
            None,
            None,
        )
        .expect("migrate opencode config");

        let content = std::fs::read_to_string(&opencode_path).expect("read opencode config");
        let value: serde_json::Value = serde_json::from_str(&content).expect("parse json");
        let server = &value["mcp"]["contextstream"];
        assert_eq!(server["type"].as_str(), Some("remote"));
        assert_eq!(server["enabled"].as_bool(), Some(true));
        assert_eq!(
            server["url"].as_str(),
            Some(default_remote_mcp_url().as_str())
        );
        assert!(server.get("environment").is_none());
        assert!(server.get("command").is_none());
    }

    #[test]
    fn test_write_mcp_config_opencode_preserves_existing_local_environment() {
        let _guard = env_test_mutex().lock().unwrap_or_else(|e| e.into_inner());
        let temp = tempfile::tempdir().expect("tempdir");
        let previous_home = std::env::var_os("HOME");
        std::env::set_var("HOME", temp.path());
        let _xdg_guard = XdgConfigGuard::isolate_under(temp.path());

        let opencode_dir = temp.path().join(".opencode");
        std::fs::create_dir_all(&opencode_dir).expect("create opencode dir");
        let config_path = opencode_dir.join("mcp.json");
        std::fs::write(
            &config_path,
            serde_json::to_string_pretty(&json!({
                "$schema": "https://example.test/team-opencode-schema.json",
                "mcp": {
                    "contextstream": {
                        "type": "local",
                        "command": ["npx", "-y", "contextstream-mcp"],
                        "environment": {
                            ENV_TRANSCRIPTS_ENABLED: "false",
                            ENV_SEARCH_LIMIT: "21"
                        },
                        "enabled": true
                    }
                }
            }))
            .expect("serialize opencode config"),
        )
        .expect("write opencode config");

        write_mcp_config(
            &Editor::OpenCode,
            "test-key",
            Some("ws-id"),
            Some("project-id"),
        )
        .expect("update opencode config");

        let content = std::fs::read_to_string(&config_path).expect("read opencode config");
        let value: serde_json::Value = serde_json::from_str(&content).expect("parse json");
        assert_eq!(
            value["$schema"],
            "https://example.test/team-opencode-schema.json"
        );
        let server = &value["mcp"]["contextstream"];
        assert_eq!(server["type"].as_str(), Some("local"));
        assert_eq!(
            server["environment"][ENV_TRANSCRIPTS_ENABLED].as_str(),
            Some("false")
        );
        assert_eq!(server["environment"][ENV_SEARCH_LIMIT].as_str(), Some("21"));

        if let Some(value) = previous_home {
            std::env::set_var("HOME", value);
        } else {
            std::env::remove_var("HOME");
        }
    }

    #[test]
    fn test_write_mcp_config_codex_preserves_existing_local_env_overrides() {
        let _guard = env_test_mutex().lock().unwrap_or_else(|e| e.into_inner());
        let temp = tempfile::tempdir().expect("tempdir");
        let previous_home = std::env::var_os("HOME");
        std::env::set_var("HOME", temp.path());
        let _xdg_guard = XdgConfigGuard::isolate_under(temp.path());

        let codex_dir = temp.path().join(".codex");
        std::fs::create_dir_all(&codex_dir).expect("create codex dir");
        let config_path = codex_dir.join("config.toml");
        std::fs::write(
            &config_path,
            r#"# ContextStream MCP Server Configuration
[mcp_servers.contextstream]
command = "/usr/local/bin/contextstream-mcp"
args = []

[mcp_servers.contextstream.env]
CONTEXTSTREAM_API_URL = "https://api.contextstream.io"
CONTEXTSTREAM_API_KEY = "test-key"
CONTEXTSTREAM_TRANSCRIPTS_ENABLED = "false"
CONTEXTSTREAM_HOOK_TRANSCRIPTS_ENABLED = "false"
CONTEXTSTREAM_SEARCH_LIMIT = "21"
CONTEXTSTREAM_SEARCH_MAX_CHARS = "4100"
"#,
        )
        .expect("write codex config");

        write_mcp_config(
            &Editor::Codex,
            "test-key",
            Some("ws-id"),
            Some("project-id"),
        )
        .expect("update codex config");

        let content = std::fs::read_to_string(&config_path).expect("read codex config");
        assert!(content.contains(r#"CONTEXTSTREAM_TRANSCRIPTS_ENABLED = "false""#));
        assert!(content.contains(r#"CONTEXTSTREAM_HOOK_TRANSCRIPTS_ENABLED = "false""#));
        assert!(content.contains(r#"CONTEXTSTREAM_SEARCH_LIMIT = "21""#));
        assert!(content.contains(r#"CONTEXTSTREAM_SEARCH_MAX_CHARS = "4100""#));

        if let Some(value) = previous_home {
            std::env::set_var("HOME", value);
        } else {
            std::env::remove_var("HOME");
        }
    }

    #[test]
    fn test_write_mcp_config_with_remote_auth_uses_hosted_remote_for_codex() {
        let _guard = env_test_mutex().lock().unwrap_or_else(|e| e.into_inner());
        let temp = tempfile::tempdir().expect("tempdir");
        let previous_home = std::env::var_os("HOME");
        let previous_api_url = std::env::var_os("CONTEXTSTREAM_API_URL");
        std::env::set_var("HOME", temp.path());
        let _xdg_guard = XdgConfigGuard::isolate_under(temp.path());
        std::env::set_var("CONTEXTSTREAM_API_URL", DEFAULT_API_URL);

        write_mcp_config_with_remote_auth(
            &Editor::Codex,
            "test-key",
            Some("ws-id"),
            None,
            None,
            None,
            Some("test-key"),
        )
        .expect("write codex config");

        let config_path = temp.path().join(".codex").join("config.toml");
        let content = std::fs::read_to_string(&config_path).expect("read codex config");
        assert!(content.contains(&format!(r#"url = "{}""#, default_remote_mcp_url())));
        assert!(content.contains(r#"http_headers = {"#));
        assert!(content.contains(r#""Authorization" = "Bearer test-key""#));
        assert!(content.contains(r#""X-ContextStream-Workspace-Id" = "ws-id""#));
        assert!(!content.contains("command = "));
        assert!(!content.contains("[mcp_servers.contextstream.env]"));

        if let Some(value) = previous_api_url {
            std::env::set_var("CONTEXTSTREAM_API_URL", value);
        } else {
            std::env::remove_var("CONTEXTSTREAM_API_URL");
        }
        if let Some(value) = previous_home {
            std::env::set_var("HOME", value);
        } else {
            std::env::remove_var("HOME");
        }
    }

    #[test]
    fn test_migrate_mcp_config_claude_embeds_remote_auth_header() {
        let _guard = env_test_mutex().lock().unwrap_or_else(|e| e.into_inner());
        let temp = tempfile::tempdir().expect("tempdir");
        let previous_home = std::env::var_os("HOME");
        let previous_api_url = std::env::var_os("CONTEXTSTREAM_API_URL");
        std::env::set_var("HOME", temp.path());
        let _xdg_guard = XdgConfigGuard::isolate_under(temp.path());
        std::env::set_var("CONTEXTSTREAM_API_URL", DEFAULT_API_URL);

        let claude_dir = temp.path().join(".claude");
        std::fs::create_dir_all(&claude_dir).expect("create claude dir");
        let config_path = claude_dir.join("mcp.json");
        std::fs::write(
            &config_path,
            r#"{
  "mcpServers": {
    "contextstream": {
      "command": "/usr/local/bin/contextstream-mcp",
      "args": [],
      "env": {
        "CONTEXTSTREAM_API_URL": "https://api.contextstream.io",
        "CONTEXTSTREAM_API_KEY": "test-key"
      }
    }
  }
}"#,
        )
        .expect("write claude config");

        migrate_mcp_config(
            &Editor::ClaudeCode,
            "test-key",
            Some("ws-id"),
            Some("project-id"),
            None,
            None,
        )
        .expect("migrate claude config");

        let content = std::fs::read_to_string(&config_path).expect("read claude config");
        assert!(content.contains(r#""type": "http""#));
        assert!(content.contains(r#""X-ContextStream-API-Key": "test-key""#));
        assert!(content.contains(r#""X-ContextStream-Workspace-Id": "ws-id""#));
        assert!(content.contains(r#""X-ContextStream-Project-Id": "project-id""#));

        if let Some(value) = previous_api_url {
            std::env::set_var("CONTEXTSTREAM_API_URL", value);
        } else {
            std::env::remove_var("CONTEXTSTREAM_API_URL");
        }
        if let Some(value) = previous_home {
            std::env::set_var("HOME", value);
        } else {
            std::env::remove_var("HOME");
        }
    }

    #[test]
    fn test_migrate_mcp_config_codex_preserves_local_env_overrides_as_remote_headers() {
        let _guard = env_test_mutex().lock().unwrap_or_else(|e| e.into_inner());
        let temp = tempfile::tempdir().expect("tempdir");
        let previous_home = std::env::var_os("HOME");
        let previous_api_url = std::env::var_os("CONTEXTSTREAM_API_URL");
        std::env::set_var("HOME", temp.path());
        let _xdg_guard = XdgConfigGuard::isolate_under(temp.path());
        std::env::set_var("CONTEXTSTREAM_API_URL", DEFAULT_API_URL);

        let codex_dir = temp.path().join(".codex");
        std::fs::create_dir_all(&codex_dir).expect("create codex dir");
        let config_path = codex_dir.join("config.toml");
        std::fs::write(
            &config_path,
            r#"# ContextStream MCP Server Configuration
[mcp_servers.contextstream]
command = "/usr/local/bin/contextstream-mcp"
args = []

[mcp_servers.contextstream.env]
CONTEXTSTREAM_API_URL = "https://api.contextstream.io"
CONTEXTSTREAM_API_KEY = "test-key"
CONTEXTSTREAM_TRANSCRIPTS_ENABLED = "false"
CONTEXTSTREAM_SEARCH_LIMIT = "21"
"#,
        )
        .expect("write codex config");

        migrate_mcp_config(
            &Editor::Codex,
            "test-key",
            Some("ws-id"),
            Some("project-id"),
            None,
            None,
        )
        .expect("migrate codex config");

        let content = std::fs::read_to_string(&config_path).expect("read codex config");
        assert!(content.contains(&format!(r#"url = "{}""#, default_remote_mcp_url())));
        assert!(content.contains("http_headers = {"));
        assert!(content.contains(&format!(r#""{}" = "false""#, HEADER_TRANSCRIPTS_ENABLED)));
        assert!(content.contains(&format!(r#""{}" = "21""#, HEADER_SEARCH_LIMIT)));
        assert!(content.contains(&format!(r#""{}" = "ws-id""#, HEADER_WORKSPACE_ID)));
        assert!(content.contains(&format!(r#""{}" = "project-id""#, HEADER_PROJECT_ID)));
        assert!(!content.contains("command = "));
        assert!(!content.contains("[mcp_servers.contextstream.env]"));

        if let Some(value) = previous_api_url {
            std::env::set_var("CONTEXTSTREAM_API_URL", value);
        } else {
            std::env::remove_var("CONTEXTSTREAM_API_URL");
        }
        if let Some(value) = previous_home {
            std::env::set_var("HOME", value);
        } else {
            std::env::remove_var("HOME");
        }
    }

    #[test]
    fn test_write_mcp_config_codex_preserves_orphaned_contextstream_comments() {
        let _guard = env_test_mutex().lock().unwrap_or_else(|e| e.into_inner());
        let temp = tempfile::tempdir().expect("tempdir");
        let previous_home = std::env::var_os("HOME");
        let previous_api_url = std::env::var_os("CONTEXTSTREAM_API_URL");
        std::env::set_var("HOME", temp.path());
        let _xdg_guard = XdgConfigGuard::isolate_under(temp.path());
        std::env::set_var("CONTEXTSTREAM_API_URL", DEFAULT_API_URL);

        let codex_dir = temp.path().join(".codex");
        std::fs::create_dir_all(&codex_dir).expect("create codex dir");
        let config_path = codex_dir.join("config.toml");
        std::fs::write(
            &config_path,
            r#"model = "gpt-5.4"

# ContextStream MCP Server Configuration
[projects."/srv/projects"]
trust_level = "trusted"

# ContextStream MCP Server Configuration

# ContextStream MCP Server Configuration
[mcp_servers.contextstream]
command = "/usr/local/bin/contextstream-mcp"
args = []

[mcp_servers.contextstream.env]
CONTEXTSTREAM_API_URL = "https://api.contextstream.io"
CONTEXTSTREAM_API_KEY = "test-key"
"#,
        )
        .expect("write codex config");

        write_mcp_config(&Editor::Codex, "test-key", Some("ws-id"), None)
            .expect("rewrite codex config");

        let content = std::fs::read_to_string(&config_path).expect("read codex config");
        assert_eq!(
            content
                .matches("# ContextStream MCP Server Configuration")
                .count(),
            3
        );
        assert!(content.contains("[projects.\"/srv/projects\"]"));
        assert!(content.contains("command = "));
        assert!(content.contains("[mcp_servers.contextstream.env]"));
        assert!(!content.contains(r#"url = "https://mcp.contextstream.io/mcp"#));

        if let Some(value) = previous_api_url {
            std::env::set_var("CONTEXTSTREAM_API_URL", value);
        } else {
            std::env::remove_var("CONTEXTSTREAM_API_URL");
        }
        if let Some(value) = previous_home {
            std::env::set_var("HOME", value);
        } else {
            std::env::remove_var("HOME");
        }
    }

    #[test]
    fn test_write_mcp_config_cline_writes_vscode_settings_key() {
        let _guard = env_test_mutex().lock().unwrap_or_else(|e| e.into_inner());
        let temp = tempfile::tempdir().expect("tempdir");
        let previous_home = std::env::var_os("HOME");
        let previous_api_url = std::env::var_os("CONTEXTSTREAM_API_URL");
        std::env::set_var("HOME", temp.path());
        let _xdg_guard = XdgConfigGuard::isolate_under(temp.path());
        std::env::set_var("CONTEXTSTREAM_API_URL", DEFAULT_API_URL);

        write_mcp_config_force_remote_with_auth(
            &Editor::Cline,
            "test-key",
            Some("ws-id"),
            None,
            None,
            None,
            Some("test-key"),
        )
        .expect("write cline config");

        let settings_path = get_vscode_settings_path().expect("vscode settings path");
        let content = std::fs::read_to_string(settings_path).expect("read settings");
        let value: serde_json::Value = serde_json::from_str(&content).expect("parse settings");
        assert_eq!(
            value["cline.mcpServers"]["contextstream"]["type"].as_str(),
            Some("http")
        );
        assert_eq!(
            value["cline.mcpServers"]["contextstream"]["headers"][HEADER_WORKSPACE_ID].as_str(),
            Some("ws-id")
        );

        if let Some(value) = previous_api_url {
            std::env::set_var("CONTEXTSTREAM_API_URL", value);
        } else {
            std::env::remove_var("CONTEXTSTREAM_API_URL");
        }
        if let Some(value) = previous_home {
            std::env::set_var("HOME", value);
        } else {
            std::env::remove_var("HOME");
        }
    }

    #[test]
    fn test_write_mcp_config_roo_writes_vscode_settings_key() {
        let _guard = env_test_mutex().lock().unwrap_or_else(|e| e.into_inner());
        let temp = tempfile::tempdir().expect("tempdir");
        let previous_home = std::env::var_os("HOME");
        let previous_api_url = std::env::var_os("CONTEXTSTREAM_API_URL");
        std::env::set_var("HOME", temp.path());
        let _xdg_guard = XdgConfigGuard::isolate_under(temp.path());
        std::env::set_var("CONTEXTSTREAM_API_URL", DEFAULT_API_URL);

        write_mcp_config_force_remote_with_auth(
            &Editor::RooCode,
            "test-key",
            Some("ws-id"),
            Some("project-id"),
            None,
            None,
            Some("test-key"),
        )
        .expect("write roo config");

        let settings_path = get_vscode_settings_path().expect("vscode settings path");
        let content = std::fs::read_to_string(settings_path).expect("read settings");
        let value: serde_json::Value = serde_json::from_str(&content).expect("parse settings");
        assert_eq!(
            value["roo-cline.mcpServers"]["contextstream"]["type"].as_str(),
            Some("http")
        );
        assert_eq!(
            value["roo-cline.mcpServers"]["contextstream"]["headers"][HEADER_PROJECT_ID].as_str(),
            Some("project-id")
        );

        if let Some(value) = previous_api_url {
            std::env::set_var("CONTEXTSTREAM_API_URL", value);
        } else {
            std::env::remove_var("CONTEXTSTREAM_API_URL");
        }
        if let Some(value) = previous_home {
            std::env::set_var("HOME", value);
        } else {
            std::env::remove_var("HOME");
        }
    }

    #[test]
    fn test_write_mcp_config_kilo_writes_global_kilo_jsonc() {
        let _guard = env_test_mutex().lock().unwrap_or_else(|e| e.into_inner());
        let temp = tempfile::tempdir().expect("tempdir");
        let previous_home = std::env::var_os("HOME");
        let previous_api_url = std::env::var_os("CONTEXTSTREAM_API_URL");
        std::env::set_var("HOME", temp.path());
        let _xdg_guard = XdgConfigGuard::isolate_under(temp.path());
        std::env::set_var("CONTEXTSTREAM_API_URL", DEFAULT_API_URL);

        write_mcp_config_force_remote_with_auth(
            &Editor::KiloCode,
            "test-key",
            Some("ws-id"),
            None,
            None,
            None,
            Some("test-key"),
        )
        .expect("write kilo config");

        // Resolve the kilo config path the same way the writer does. On macOS
        // `dirs::config_dir()` is `~/Library/Application Support`, not `~/.config`,
        // so a hardcoded `.config` path is wrong off-Linux.
        let kilo_path = crate::setup::editors::kilo_config_dir()
            .expect("kilo config dir")
            .join("kilo.jsonc");
        let content = std::fs::read_to_string(kilo_path).expect("read kilo config");
        let value: serde_json::Value = serde_json::from_str(&content).expect("parse kilo json");
        // Kilo's schema only accepts type local|remote — the generic "http"
        // transport name is invalid there (kilo.ai/docs/automate/mcp/using-in-cli).
        assert_eq!(
            value["mcp"]["contextstream"]["type"].as_str(),
            Some("remote")
        );
        assert_eq!(
            value["mcp"]["contextstream"]["headers"][HEADER_WORKSPACE_ID].as_str(),
            Some("ws-id")
        );
        assert!(
            value["mcp"]["contextstream"].get("environment").is_none(),
            "remote kilo entries must not carry the local-side environment key"
        );
        // ContextStream tools are pre-approved in the permission dock.
        assert_eq!(
            value["permission"]["contextstream_*"].as_str(),
            Some("allow")
        );

        if let Some(value) = previous_api_url {
            std::env::set_var("CONTEXTSTREAM_API_URL", value);
        } else {
            std::env::remove_var("CONTEXTSTREAM_API_URL");
        }
        if let Some(value) = previous_home {
            std::env::set_var("HOME", value);
        } else {
            std::env::remove_var("HOME");
        }
    }

    #[test]
    fn test_kilo_remote_server_json_uses_kilo_transport_name() {
        let generic = json!({
            "type": "http",
            "url": "https://mcp.contextstream.io/mcp",
            "headers": {"Authorization": "Bearer k"},
            "environment": {"CONTEXTSTREAM_API_KEY": "k"}
        });
        let kilo = kilo_remote_server_json(generic);
        assert_eq!(kilo["type"].as_str(), Some("remote"));
        assert_eq!(
            kilo["url"].as_str(),
            Some("https://mcp.contextstream.io/mcp")
        );
        assert!(kilo.get("environment").is_none());
        assert!(kilo["headers"].is_object());
    }

    #[test]
    fn test_build_kilo_server_json_strips_stale_remote_keys() {
        let existing = json!({
            "type": "remote",
            "url": "https://mcp.contextstream.io/mcp",
            "serverUrl": "https://mcp.contextstream.io/mcp",
            "headers": {"Authorization": "Bearer k"},
            "enabled": true
        });
        let local = build_kilo_server_json(Some(&existing), Some("key"), None, None, None, None);
        assert_eq!(local["type"].as_str(), Some("local"));
        assert!(local["command"].is_array());
        assert!(local.get("url").is_none());
        assert!(local.get("serverUrl").is_none());
        assert!(local.get("headers").is_none());
        assert_eq!(
            local["environment"]["CONTEXTSTREAM_API_KEY"].as_str(),
            Some("key")
        );
    }

    #[test]
    fn test_kilo_permission_respects_user_preference_and_is_idempotent() {
        // No preference → wildcard allow added.
        let mut fresh = json!({});
        ensure_kilo_contextstream_permission(&mut fresh).unwrap();
        assert_eq!(
            fresh["permission"]["contextstream_*"].as_str(),
            Some("allow")
        );

        // Re-running doesn't duplicate or alter.
        ensure_kilo_contextstream_permission(&mut fresh).unwrap();
        assert_eq!(fresh["permission"].as_object().map(|p| p.len()), Some(1));

        // An explicit user deny on any contextstream tool wins — we add nothing.
        let mut denied = json!({"permission": {"contextstream_search": "deny"}});
        ensure_kilo_contextstream_permission(&mut denied).unwrap();
        assert!(denied["permission"].get("contextstream_*").is_none());
        assert_eq!(
            denied["permission"]["contextstream_search"].as_str(),
            Some("deny")
        );

        // Unrelated permissions are preserved alongside the new wildcard.
        let mut other = json!({"permission": {"github_*": "allow"}});
        ensure_kilo_contextstream_permission(&mut other).unwrap();
        assert_eq!(other["permission"]["github_*"].as_str(), Some("allow"));
        assert_eq!(
            other["permission"]["contextstream_*"].as_str(),
            Some("allow")
        );
    }

    #[test]
    fn test_write_mcp_config_antigravity_writes_gemini_path() {
        let _guard = env_test_mutex().lock().unwrap_or_else(|e| e.into_inner());
        let temp = tempfile::tempdir().expect("tempdir");
        let previous_home = std::env::var_os("HOME");
        let previous_api_url = std::env::var_os("CONTEXTSTREAM_API_URL");
        std::env::set_var("HOME", temp.path());
        let _xdg_guard = XdgConfigGuard::isolate_under(temp.path());
        std::env::set_var("CONTEXTSTREAM_API_URL", DEFAULT_API_URL);

        write_mcp_config_force_remote_with_auth(
            &Editor::Antigravity,
            "test-key",
            Some("ws-id"),
            None,
            None,
            None,
            Some("test-key"),
        )
        .expect("write antigravity config");

        let ag_path = temp
            .path()
            .join(".gemini")
            .join("antigravity")
            .join("mcp_config.json");
        let content = std::fs::read_to_string(ag_path).expect("read antigravity config");
        let value: serde_json::Value = serde_json::from_str(&content).expect("parse json");
        assert_eq!(
            value["mcpServers"]["contextstream"]["type"].as_str(),
            Some("http")
        );
        assert_eq!(
            value["mcpServers"]["contextstream"]["headers"][HEADER_WORKSPACE_ID].as_str(),
            Some("ws-id")
        );

        if let Some(value) = previous_api_url {
            std::env::set_var("CONTEXTSTREAM_API_URL", value);
        } else {
            std::env::remove_var("CONTEXTSTREAM_API_URL");
        }
        if let Some(value) = previous_home {
            std::env::set_var("HOME", value);
        } else {
            std::env::remove_var("HOME");
        }
    }

    #[test]
    fn test_write_workspace_config_removes_project_fields_when_not_provided() {
        let temp = tempfile::tempdir().expect("tempdir");
        let project = temp.path();
        let config_path = project.join(".contextstream").join("config.json");

        write_workspace_config(
            project,
            "workspace-1",
            "Workspace 1",
            Some("project-name"),
            Some("project-1"),
        )
        .expect("seed workspace config");

        write_workspace_config(project, "workspace-1", "Workspace 1", None, None)
            .expect("rewrite workspace config");

        let content = std::fs::read_to_string(config_path).expect("read workspace config");
        let value: serde_json::Value = serde_json::from_str(&content).expect("parse json");
        let obj = value.as_object().expect("object");

        assert!(
            !obj.contains_key("project_id"),
            "project_id should be removed when not provided"
        );
        assert!(
            !obj.contains_key("project_name"),
            "project_name should be removed when not provided"
        );
    }

    #[test]
    fn test_write_workspace_config_is_byte_identical_when_binding_is_unchanged() {
        let temp = tempfile::tempdir().expect("tempdir");
        let project = temp.path();
        let config_path = project.join(".contextstream").join("config.json");

        write_workspace_config(
            project,
            "workspace-1",
            "Workspace 1",
            Some("project-name"),
            Some("project-1"),
        )
        .expect("first workspace config write");
        let first = std::fs::read(&config_path).expect("read first workspace config");

        write_workspace_config(
            project,
            "workspace-1",
            "Workspace 1",
            Some("project-name"),
            Some("project-1"),
        )
        .expect("idempotent workspace config write");
        let second = std::fs::read(&config_path).expect("read second workspace config");

        assert_eq!(
            second, first,
            "refreshing an unchanged binding must not churn associated_at or file bytes"
        );

        let mut malformed: Value =
            serde_json::from_slice(&second).expect("parse workspace config fixture");
        malformed["associated_at"] = json!("not-a-timestamp");
        std::fs::write(
            &config_path,
            serde_json::to_vec_pretty(&malformed).expect("encode malformed timestamp"),
        )
        .expect("seed malformed timestamp");
        write_workspace_config(
            project,
            "workspace-1",
            "Workspace 1",
            Some("project-name"),
            Some("project-1"),
        )
        .expect("repair malformed associated_at");
        let repaired: Value = serde_json::from_slice(
            &std::fs::read(&config_path).expect("read repaired workspace config"),
        )
        .expect("parse repaired workspace config");
        assert!(
            repaired["associated_at"]
                .as_str()
                .is_some_and(|value| chrono::DateTime::parse_from_rfc3339(value).is_ok()),
            "a malformed binding timestamp must be repaired"
        );
    }

    #[test]
    fn every_setup_editor_emits_a_bounded_client_identifier() {
        let _guard = env_test_mutex().lock().unwrap_or_else(|e| e.into_inner());
        let _api_url = EnvVarGuard::set("CONTEXTSTREAM_API_URL", DEFAULT_API_URL);
        let _allow_local = EnvVarGuard::set("CONTEXTSTREAM_ALLOW_LOCAL_MCP", "1");
        let identity = ManagedConfigIdentity {
            installation_id: Some("aaaaaaaa-bbbb-4ccc-8ddd-eeeeeeeeeeee".to_string()),
        };

        for editor in Editor::all() {
            let generated = generate_config_json_with_identity(
                editor,
                "test-key",
                Some("workspace-id"),
                Some("project-id"),
                "local",
                None,
                &identity,
            );
            let server = &generated["server_config"];
            let environment = server
                .get("env")
                .or_else(|| server.get("environment"))
                .and_then(Value::as_object)
                .expect("local environment");
            assert_eq!(
                environment.get(ENV_CLIENT_NAME).and_then(Value::as_str),
                Some(editor.id()),
                "{} local config must identify its editor",
                editor.display_name()
            );
            assert_eq!(
                environment
                    .get(ENV_MANAGED_CONFIG_VERSION)
                    .and_then(Value::as_str),
                Some(MANAGED_CONFIG_VERSION)
            );
            assert_eq!(
                environment
                    .get(ENV_TEACHING_VERSION)
                    .and_then(Value::as_str),
                Some(HARNESS_TEACHING_VERSION)
            );
            assert_eq!(
                environment.get(ENV_INSTALLATION_ID).and_then(Value::as_str),
                identity.installation_id.as_deref()
            );

            if editor_supports_remote_mcp(editor) {
                let generated = generate_config_json_with_identity(
                    editor,
                    "test-key",
                    Some("workspace-id"),
                    Some("project-id"),
                    "remote",
                    Some("test-key"),
                    &identity,
                );
                let headers = generated["server_config"]["headers"]
                    .as_object()
                    .expect("remote headers");
                assert_eq!(
                    headers.get(HEADER_CLIENT_NAME).and_then(Value::as_str),
                    Some(editor.id()),
                    "{} remote config must identify its editor",
                    editor.display_name()
                );
                assert_eq!(
                    headers
                        .get(HEADER_MANAGED_CONFIG_VERSION)
                        .and_then(Value::as_str),
                    Some(MANAGED_CONFIG_VERSION)
                );
                assert_eq!(
                    headers.get(HEADER_TEACHING_VERSION).and_then(Value::as_str),
                    Some(HARNESS_TEACHING_VERSION)
                );
                assert_eq!(
                    headers.get(HEADER_INSTALLATION_ID).and_then(Value::as_str),
                    identity.installation_id.as_deref()
                );
            }
        }
    }

    #[test]
    fn dry_run_config_write_creates_no_installation_or_editor_state() {
        let _guard = env_test_mutex().lock().unwrap_or_else(|e| e.into_inner());
        let temp = tempfile::tempdir().expect("tempdir");
        let _home = HomeGuard::isolate_under(temp.path());
        let _xdg = XdgConfigGuard::isolate_under(temp.path());
        let _api_url = EnvVarGuard::set("CONTEXTSTREAM_API_URL", DEFAULT_API_URL);
        let _dry_run = DryRunGuard::enabled();

        write_mcp_config_force_remote_with_auth(
            &Editor::Codex,
            "test-key",
            Some("workspace-id"),
            Some("project-id"),
            None,
            None,
            Some("test-key"),
        )
        .expect("dry-run config preview");

        assert!(!temp.path().join(".contextstream").exists());
        assert!(!temp.path().join(".codex").join("config.toml").exists());
        assert!(!temp
            .path()
            .join(".contextstream")
            .join("installation.lock")
            .exists());
    }

    #[test]
    fn unit_config_writes_use_a_nonpersistent_managed_identity() {
        let _guard = env_test_mutex().lock().unwrap_or_else(|e| e.into_inner());
        let temp = tempfile::tempdir().expect("tempdir");
        let _home = HomeGuard::isolate_under(temp.path());
        let _api_url = EnvVarGuard::set("CONTEXTSTREAM_API_URL", DEFAULT_API_URL);
        let project = temp.path().join("project");
        std::fs::create_dir_all(&project).expect("create project");

        write_project_mcp_config(
            &Editor::ClaudeCode,
            &project,
            "test-key",
            Some("workspace-id"),
            Some("project-id"),
        )
        .expect("write project config");

        assert!(
            !temp.path().join(".contextstream").exists(),
            "unit writer internals must not create installation state"
        );
        let config: Value = serde_json::from_slice(
            &std::fs::read(project.join(".mcp.json")).expect("read project config"),
        )
        .expect("parse project config");
        assert_eq!(
            config["mcpServers"]["contextstream"]["headers"][HEADER_INSTALLATION_ID],
            json!(TEST_MANAGED_INSTALLATION_ID)
        );
    }

    #[test]
    fn version_one_json_config_migrates_surgically_and_reruns_byte_identically() {
        let _guard = env_test_mutex().lock().unwrap_or_else(|e| e.into_inner());
        let temp = tempfile::tempdir().expect("tempdir");
        let _home = HomeGuard::isolate_under(temp.path());
        let _xdg = XdgConfigGuard::isolate_under(temp.path());
        let _api_url = EnvVarGuard::set("CONTEXTSTREAM_API_URL", DEFAULT_API_URL);
        let _identity_persistence = ManagedIdentityPersistenceGuard::enabled();

        let config_path = temp.path().join(".claude").join("mcp.json");
        std::fs::create_dir_all(config_path.parent().unwrap()).expect("create config parent");
        std::fs::write(
            &config_path,
            concat!(
                "{\n",
                "  \"theme\": \"preserve\",\n",
                "  \"mcpServers\": {\n",
                "    \"private\": {\"command\": \"keep-me\"},\n",
                "    \"contextstream\": {\n",
                "      \"command\": \"contextstream-mcp\",\n",
                "      \"custom\": {\"keep\": true},\n",
                "      \"env\": {\n",
                "        \"CONTEXTSTREAM_MANAGED_CONFIG_VERSION\": \"1\",\n",
                "        \"CONTEXTSTREAM_CLIENT\": \"codex\"\n",
                "      }\n",
                "    }\n",
                "  }\n",
                "}\n"
            ),
        )
        .expect("seed v1 config");

        write_mcp_config_force_remote_with_auth(
            &Editor::ClaudeCode,
            "test-key",
            Some("workspace-id"),
            Some("project-id"),
            None,
            None,
            Some("test-key"),
        )
        .expect("migrate v1 config");

        let installation_path = temp.path().join(".contextstream").join("installation.json");
        let installation: Value =
            serde_json::from_slice(&std::fs::read(&installation_path).unwrap()).unwrap();
        let installation_id = installation["installation_id"].as_str().unwrap();
        let updated: Value = serde_json::from_slice(&std::fs::read(&config_path).unwrap()).unwrap();
        let server = &updated["mcpServers"]["contextstream"];
        assert_eq!(updated["theme"], json!("preserve"));
        assert_eq!(
            updated["mcpServers"]["private"]["command"],
            json!("keep-me")
        );
        assert_eq!(server["custom"], json!({"keep": true}));
        assert_eq!(
            server["headers"][HEADER_MANAGED_CONFIG_VERSION],
            json!(MANAGED_CONFIG_VERSION)
        );
        assert_eq!(
            server["headers"][HEADER_CLIENT_NAME],
            json!(Editor::ClaudeCode.id())
        );
        assert_eq!(
            server["headers"][HEADER_TEACHING_VERSION],
            json!(HARNESS_TEACHING_VERSION)
        );
        assert_eq!(
            server["headers"][HEADER_INSTALLATION_ID],
            json!(installation_id)
        );

        let config_bytes = std::fs::read(&config_path).unwrap();
        let installation_bytes = std::fs::read(&installation_path).unwrap();
        write_mcp_config_force_remote_with_auth(
            &Editor::ClaudeCode,
            "test-key",
            Some("workspace-id"),
            Some("project-id"),
            None,
            None,
            Some("test-key"),
        )
        .expect("idempotent refresh");
        assert_eq!(std::fs::read(&config_path).unwrap(), config_bytes);
        assert_eq!(
            std::fs::read(&installation_path).unwrap(),
            installation_bytes
        );
    }

    #[test]
    fn version_one_codex_config_migrates_without_touching_claude_settings() {
        let _guard = env_test_mutex().lock().unwrap_or_else(|e| e.into_inner());
        let temp = tempfile::tempdir().expect("tempdir");
        let _home = HomeGuard::isolate_under(temp.path());
        let _xdg = XdgConfigGuard::isolate_under(temp.path());
        let _api_url = EnvVarGuard::set("CONTEXTSTREAM_API_URL", DEFAULT_API_URL);
        let _identity_persistence = ManagedIdentityPersistenceGuard::enabled();

        let claude_settings = temp.path().join(".claude").join("settings.json");
        std::fs::create_dir_all(claude_settings.parent().unwrap()).expect("create claude dir");
        let claude_original = b"{\n  // user hook\n  \"hooks\": {\"PreToolUse\": [\"keep\"]}\n}\n";
        std::fs::write(&claude_settings, claude_original).expect("seed Claude settings");

        let codex_path = temp.path().join(".codex").join("config.toml");
        std::fs::create_dir_all(codex_path.parent().unwrap()).expect("create codex dir");
        std::fs::write(
            &codex_path,
            concat!(
                "model = \"keep-me\"\n\n",
                "[mcp_servers.contextstream]\n",
                "url = \"https://mcp.contextstream.io/mcp\"\n",
                "http_headers = { ",
                "\"X-ContextStream-Managed-Config-Version\" = \"1\", ",
                "\"X-ContextStream-Client\" = \"claude\", ",
                "\"X-User-Header\" = \"keep-me\" }\n",
            ),
        )
        .expect("seed v1 Codex config");

        write_mcp_config_force_remote_with_auth(
            &Editor::Codex,
            "test-key",
            Some("workspace-id"),
            Some("project-id"),
            None,
            None,
            Some("test-key"),
        )
        .expect("migrate Codex v1 config");

        assert_eq!(std::fs::read(&claude_settings).unwrap(), claude_original);
        let codex = std::fs::read_to_string(&codex_path).unwrap();
        let parsed = parse_codex_toml(&codex, &codex_path).expect("parse migrated Codex config");
        let server = contextstream_toml_item(&parsed).expect("Codex server");
        assert_eq!(
            parsed
                .get("model")
                .and_then(Item::as_value)
                .and_then(toml_edit::Value::as_str),
            Some("keep-me")
        );
        assert_eq!(
            toml_nested_string(server, "http_headers", "X-User-Header"),
            Some("keep-me")
        );
        assert_eq!(
            toml_nested_string(server, "http_headers", HEADER_MANAGED_CONFIG_VERSION),
            Some(MANAGED_CONFIG_VERSION)
        );
        assert_eq!(
            toml_nested_string(server, "http_headers", HEADER_CLIENT_NAME),
            Some(Editor::Codex.id())
        );
        assert_eq!(
            toml_nested_string(server, "http_headers", HEADER_TEACHING_VERSION),
            Some(HARNESS_TEACHING_VERSION)
        );
        let installation: Value = serde_json::from_slice(
            &std::fs::read(temp.path().join(".contextstream").join("installation.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(
            toml_nested_string(server, "http_headers", HEADER_INSTALLATION_ID),
            installation["installation_id"].as_str()
        );
    }

    #[test]
    fn malformed_installation_state_blocks_config_writes_byte_for_byte() {
        let _guard = env_test_mutex().lock().unwrap_or_else(|e| e.into_inner());
        let temp = tempfile::tempdir().expect("tempdir");
        let _home = HomeGuard::isolate_under(temp.path());
        let _xdg = XdgConfigGuard::isolate_under(temp.path());
        let _api_url = EnvVarGuard::set("CONTEXTSTREAM_API_URL", DEFAULT_API_URL);
        let _identity_persistence = ManagedIdentityPersistenceGuard::enabled();

        let installation_path = temp.path().join(".contextstream").join("installation.json");
        std::fs::create_dir_all(installation_path.parent().unwrap()).expect("create state dir");
        let malformed = b"{ definitely not json";
        std::fs::write(&installation_path, malformed).expect("seed malformed state");

        let config_path = temp.path().join(".claude").join("mcp.json");
        std::fs::create_dir_all(config_path.parent().unwrap()).expect("create config dir");
        let config = b"{\n  \"mcpServers\": {}\n}\n";
        std::fs::write(&config_path, config).expect("seed config");

        assert!(write_mcp_config_force_remote_with_auth(
            &Editor::ClaudeCode,
            "test-key",
            None,
            None,
            None,
            None,
            Some("test-key"),
        )
        .is_err());
        assert_eq!(std::fs::read(&installation_path).unwrap(), malformed);
        assert_eq!(std::fs::read(&config_path).unwrap(), config);
        assert!(!safe_edit::backup_path(&config_path).unwrap().exists());
    }

    #[test]
    fn codex_uninstall_removes_only_exact_contextstream_tables() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let original = concat!(
            "[mcp_servers.contextstream_backup]\n",
            "command = \"user-backup\"\n",
            "\n",
            "[mcp_servers.contextstream]\n",
            "url = \"https://mcp.contextstream.io/mcp\"\n",
            "\n",
            "[mcp_servers.contextstream.headers]\n",
            "X-ContextStream-API-Key = \"secret\"\n",
            "\n",
            "[projects.\"/work\"]\n",
            "trust_level = \"trusted\""
        );
        std::fs::write(&path, original).unwrap();

        remove_contextstream_from_codex_toml(&path).unwrap();

        let after = std::fs::read_to_string(&path).unwrap();
        assert!(after.contains("[mcp_servers.contextstream_backup]"));
        assert!(after.contains("command = \"user-backup\""));
        assert!(!after.contains("[mcp_servers.contextstream]"));
        assert!(!after.contains("[mcp_servers.contextstream.headers]"));
        assert!(after.contains("[projects.\"/work\"]"));
        assert!(
            !after.ends_with('\n'),
            "uninstall changed the original trailing-newline convention"
        );
    }

    #[test]
    fn codex_install_trust_uninstall_restores_exact_existing_toml() {
        let _guard = env_test_mutex().lock().unwrap_or_else(|e| e.into_inner());
        let temp = tempfile::tempdir().expect("tempdir");
        let previous_home = std::env::var_os("HOME");
        let previous_api_url = std::env::var_os("CONTEXTSTREAM_API_URL");
        std::env::set_var("HOME", temp.path());
        let _xdg_guard = XdgConfigGuard::isolate_under(temp.path());
        std::env::set_var("CONTEXTSTREAM_API_URL", DEFAULT_API_URL);

        let codex_dir = temp.path().join(".codex");
        std::fs::create_dir_all(&codex_dir).expect("create codex dir");
        let config_path = codex_dir.join("config.toml");
        let original = concat!(
            "model = \"gpt-5.4\"\r\n",
            "# user-authored comment\r\n",
            "[projects.\"/already-trusted\"]\r\n",
            "custom = \"preserve-byte-for-byte\""
        );
        std::fs::write(&config_path, original).expect("seed Codex config");

        write_mcp_config_force_local(&Editor::Codex, "test-key", Some("ws-id"), None, None, None)
            .expect("install Codex MCP config");
        ensure_codex_project_trust(&temp.path().join("work")).expect("add managed trust");
        let installed = std::fs::read_to_string(&config_path).expect("read installed config");
        assert!(installed.contains("[mcp_servers.contextstream]"));
        assert!(installed.contains(CODEX_MANAGED_TRUST_COMMENT));
        assert!(
            installed
                .bytes()
                .enumerate()
                .all(|(index, byte)| byte != b'\n'
                    || installed.as_bytes().get(index.wrapping_sub(1)) == Some(&b'\r')),
            "CRLF convention changed"
        );

        remove_contextstream_from_codex_toml(&config_path).expect("uninstall Codex config");

        assert_eq!(
            std::fs::read_to_string(&config_path).unwrap(),
            original,
            "clean uninstall must restore every original byte"
        );
        assert!(!safe_edit::backup_path(&config_path).unwrap().exists());

        if let Some(value) = previous_api_url {
            std::env::set_var("CONTEXTSTREAM_API_URL", value);
        } else {
            std::env::remove_var("CONTEXTSTREAM_API_URL");
        }
        if let Some(value) = previous_home {
            std::env::set_var("HOME", value);
        } else {
            std::env::remove_var("HOME");
        }
    }

    #[test]
    fn generated_codex_config_and_managed_trust_are_removed_without_backups() {
        let _guard = env_test_mutex().lock().unwrap_or_else(|e| e.into_inner());
        let temp = tempfile::tempdir().expect("tempdir");
        let previous_home = std::env::var_os("HOME");
        let previous_api_url = std::env::var_os("CONTEXTSTREAM_API_URL");
        std::env::set_var("HOME", temp.path());
        let _xdg_guard = XdgConfigGuard::isolate_under(temp.path());
        std::env::set_var("CONTEXTSTREAM_API_URL", DEFAULT_API_URL);

        let config_path = temp.path().join(".codex").join("config.toml");
        write_mcp_config_force_local(&Editor::Codex, "test-key", None, None, None, None)
            .expect("install generated Codex config");
        ensure_codex_project_trust(&temp.path().join("work")).expect("add managed trust");
        assert!(!safe_edit::backup_path(&config_path).unwrap().exists());

        remove_contextstream_from_codex_toml(&config_path).expect("uninstall generated config");

        assert!(!config_path.exists());
        assert!(!safe_edit::backup_path(&config_path).unwrap().exists());

        if let Some(value) = previous_api_url {
            std::env::set_var("CONTEXTSTREAM_API_URL", value);
        } else {
            std::env::remove_var("CONTEXTSTREAM_API_URL");
        }
        if let Some(value) = previous_home {
            std::env::set_var("HOME", value);
        } else {
            std::env::remove_var("HOME");
        }
    }

    #[test]
    fn malformed_codex_toml_is_never_rewritten() {
        let _guard = env_test_mutex().lock().unwrap_or_else(|e| e.into_inner());
        let temp = tempfile::tempdir().expect("tempdir");
        let previous_home = std::env::var_os("HOME");
        let previous_api_url = std::env::var_os("CONTEXTSTREAM_API_URL");
        std::env::set_var("HOME", temp.path());
        let _xdg_guard = XdgConfigGuard::isolate_under(temp.path());
        std::env::set_var("CONTEXTSTREAM_API_URL", DEFAULT_API_URL);

        let codex_dir = temp.path().join(".codex");
        std::fs::create_dir_all(&codex_dir).expect("create Codex dir");
        let config_path = codex_dir.join("config.toml");
        let original = "model = \"unterminated\n[projects.bad]\n";
        std::fs::write(&config_path, original).expect("seed malformed config");

        let error =
            write_mcp_config_force_local(&Editor::Codex, "test-key", None, None, None, None)
                .expect_err("malformed TOML must fail closed");
        assert!(error.to_string().contains("not valid TOML"), "{error:#}");
        assert_eq!(std::fs::read_to_string(&config_path).unwrap(), original);
        assert!(!safe_edit::backup_path(&config_path).unwrap().exists());

        if let Some(value) = previous_api_url {
            std::env::set_var("CONTEXTSTREAM_API_URL", value);
        } else {
            std::env::remove_var("CONTEXTSTREAM_API_URL");
        }
        if let Some(value) = previous_home {
            std::env::set_var("HOME", value);
        } else {
            std::env::remove_var("HOME");
        }
    }

    #[test]
    fn json_global_install_uninstall_restores_exact_existing_content() {
        let _guard = env_test_mutex().lock().unwrap_or_else(|e| e.into_inner());
        let temp = tempfile::tempdir().expect("tempdir");
        let previous_home = std::env::var_os("HOME");
        let previous_api_url = std::env::var_os("CONTEXTSTREAM_API_URL");
        std::env::set_var("HOME", temp.path());
        let _xdg_guard = XdgConfigGuard::isolate_under(temp.path());
        std::env::set_var("CONTEXTSTREAM_API_URL", DEFAULT_API_URL);

        let config_path = temp.path().join(".claude").join("mcp.json");
        std::fs::create_dir_all(config_path.parent().unwrap()).unwrap();
        let original = concat!(
            "{\n",
            "  // preserve this exact user comment\n",
            "  \"theme\": \"custom\",\n",
            "  \"mcpServers\": { \"other\": { \"command\": \"user\" } }\n",
            "}"
        );
        std::fs::write(&config_path, original).unwrap();

        write_mcp_config_force_local(
            &Editor::ClaudeCode,
            "test-key",
            Some("ws-id"),
            None,
            None,
            None,
        )
        .expect("install Claude config");
        remove_contextstream_from_mcp_config(&Editor::ClaudeCode).expect("uninstall Claude config");

        assert_eq!(std::fs::read_to_string(&config_path).unwrap(), original);
        assert!(!safe_edit::backup_path(&config_path).unwrap().exists());

        if let Some(value) = previous_api_url {
            std::env::set_var("CONTEXTSTREAM_API_URL", value);
        } else {
            std::env::remove_var("CONTEXTSTREAM_API_URL");
        }
        if let Some(value) = previous_home {
            std::env::set_var("HOME", value);
        } else {
            std::env::remove_var("HOME");
        }
    }

    #[test]
    fn every_enabled_editor_install_uninstall_preserves_exact_user_config() {
        let _guard = env_test_mutex().lock().unwrap_or_else(|e| e.into_inner());
        let temp = tempfile::tempdir().expect("tempdir");
        let _home_guard = HomeGuard::isolate_under(temp.path());
        let _xdg_guard = XdgConfigGuard::isolate_under(temp.path());
        let _api_url_guard = EnvVarGuard::set("CONTEXTSTREAM_API_URL", DEFAULT_API_URL);

        for editor in Editor::all() {
            let path = editor.mcp_config_path().unwrap_or_else(|| {
                panic!("{} should expose its documented config path", editor.id())
            });
            std::fs::create_dir_all(path.parent().expect("config parent"))
                .expect("create config parent");

            let original = match editor {
                Editor::Codex => concat!(
                    "# preserve exact user TOML and CRLF\r\n",
                    "model = \"user-model\"\r\n",
                    "[projects.\"/user/project\"]\r\n",
                    "trust_level = \"trusted\""
                )
                .to_string(),
                Editor::Aider => concat!(
                    "# this is user-owned YAML, not an MCP config\n",
                    "model: user-model\n",
                    "read:\n",
                    "  - USER_NOTES.md\n"
                )
                .to_string(),
                Editor::Cline | Editor::RooCode | Editor::KiloCode => {
                    let root_key = mcp_root_key(editor).expect("JSONC root key");
                    format!(
                        concat!(
                            "{{\n",
                            "  // preserve this comment and trailing comma\n",
                            "  \"user.setting\": \"unchanged\",\n",
                            "  \"{root_key}\": {{\n",
                            "    \"user-server\": {{ \"command\": \"user-command\" }},\n",
                            "  }},\n",
                            "}}\n"
                        ),
                        root_key = root_key
                    )
                }
                _ => {
                    let root_key = mcp_root_key(editor).expect("JSON root key");
                    format!(
                        concat!(
                            "{{\n",
                            "  \"userSetting\": {{ \"nested\": true }},\n",
                            "  \"{root_key}\": {{\n",
                            "    \"user-server\": {{ \"command\": \"user-command\" }}\n",
                            "  }}\n",
                            "}}\n"
                        ),
                        root_key = root_key
                    )
                }
            };
            std::fs::write(&path, &original).expect("seed user config");

            write_mcp_config_force_remote_with_auth(
                editor,
                "cs_live_matrix_secret",
                Some("workspace-id"),
                Some("project-id"),
                None,
                None,
                Some("cs_live_matrix_secret"),
            )
            .unwrap_or_else(|error| panic!("{} install failed: {error:#}", editor.id()));

            if matches!(editor, Editor::Aider) {
                assert_eq!(
                    std::fs::read_to_string(&path).expect("read Aider config after install"),
                    original,
                    "Aider's user-owned YAML changed during MCP install"
                );
                assert!(
                    !safe_edit::backup_path(&path).unwrap().exists(),
                    "Aider no-op install created a backup"
                );
            } else {
                assert_ne!(
                    std::fs::read_to_string(&path).expect("read installed config"),
                    original,
                    "{} install did not add an MCP entry",
                    editor.id()
                );
            }

            remove_contextstream_from_mcp_config(editor)
                .unwrap_or_else(|error| panic!("{} uninstall failed: {error:#}", editor.id()));

            assert_eq!(
                std::fs::read_to_string(&path).expect("read restored config"),
                original,
                "{} did not restore the user's exact bytes",
                editor.id()
            );
            assert!(
                !safe_edit::backup_path(&path).unwrap().exists(),
                "{} left a consumed recovery backup behind",
                editor.id()
            );
        }
    }

    #[test]
    fn repeated_generated_json_refresh_uninstalls_without_backup_debris() {
        let _guard = env_test_mutex().lock().unwrap_or_else(|e| e.into_inner());
        let temp = tempfile::tempdir().expect("tempdir");
        let previous_home = std::env::var_os("HOME");
        let previous_api_url = std::env::var_os("CONTEXTSTREAM_API_URL");
        std::env::set_var("HOME", temp.path());
        let _xdg_guard = XdgConfigGuard::isolate_under(temp.path());
        std::env::set_var("CONTEXTSTREAM_API_URL", DEFAULT_API_URL);

        let config_path = temp.path().join(".claude").join("mcp.json");
        write_mcp_config_force_local(
            &Editor::ClaudeCode,
            "test-key",
            Some("first-workspace"),
            None,
            None,
            None,
        )
        .expect("first generated write");
        write_mcp_config_force_local(
            &Editor::ClaudeCode,
            "test-key",
            Some("second-workspace"),
            None,
            None,
            None,
        )
        .expect("refresh generated write");
        assert!(
            safe_edit::backup_path(&config_path).unwrap().exists(),
            "refresh should have a classified intermediate backup"
        );

        remove_contextstream_from_mcp_config(&Editor::ClaudeCode)
            .expect("uninstall generated config");

        assert!(!config_path.exists());
        assert!(!safe_edit::backup_path(&config_path).unwrap().exists());

        if let Some(value) = previous_api_url {
            std::env::set_var("CONTEXTSTREAM_API_URL", value);
        } else {
            std::env::remove_var("CONTEXTSTREAM_API_URL");
        }
        if let Some(value) = previous_home {
            std::env::set_var("HOME", value);
        } else {
            std::env::remove_var("HOME");
        }
    }

    #[test]
    fn generated_opencode_and_kilo_companion_settings_are_cleaned() {
        let _guard = env_test_mutex().lock().unwrap_or_else(|e| e.into_inner());
        let temp = tempfile::tempdir().expect("tempdir");
        let previous_home = std::env::var_os("HOME");
        let previous_api_url = std::env::var_os("CONTEXTSTREAM_API_URL");
        std::env::set_var("HOME", temp.path());
        let _xdg_guard = XdgConfigGuard::isolate_under(temp.path());
        std::env::set_var("CONTEXTSTREAM_API_URL", DEFAULT_API_URL);

        for editor in [Editor::OpenCode, Editor::KiloCode] {
            let config_path = editor.mcp_config_path().expect("config path");
            write_mcp_config_force_local(&editor, "test-key", None, None, None, None)
                .expect("write generated config");
            assert!(config_path.exists());

            remove_contextstream_from_mcp_config(&editor).expect("uninstall generated config");

            assert!(
                !config_path.exists(),
                "{} generated config remained after uninstall:\n{}",
                editor.display_name(),
                std::fs::read_to_string(&config_path).unwrap_or_default()
            );
            assert!(!safe_edit::backup_path(&config_path).unwrap().exists());
        }

        if let Some(value) = previous_api_url {
            std::env::set_var("CONTEXTSTREAM_API_URL", value);
        } else {
            std::env::remove_var("CONTEXTSTREAM_API_URL");
        }
        if let Some(value) = previous_home {
            std::env::set_var("HOME", value);
        } else {
            std::env::remove_var("HOME");
        }
    }

    #[test]
    fn project_json_install_uninstall_restores_exact_existing_content() {
        let _guard = env_test_mutex().lock().unwrap_or_else(|e| e.into_inner());
        let temp = tempfile::tempdir().expect("tempdir");
        let project = temp.path();
        let config_path = Editor::ClaudeCode
            .project_mcp_config_path(project)
            .expect("project config path");
        let original = concat!(
            "{\n",
            "  \"mcpServers\": {\n",
            "    \"other\": { \"command\": \"user-command\" }\n",
            "  },\n",
            "  \"unrelated\": true\n",
            "}\n"
        );
        std::fs::write(&config_path, original).unwrap();

        write_project_mcp_config_force_local(
            &Editor::ClaudeCode,
            project,
            "test-key",
            Some("ws-id"),
            Some("project-id"),
            None,
            None,
        )
        .expect("install project config");
        assert!(
            remove_contextstream_from_project_mcp_config(&Editor::ClaudeCode, project)
                .expect("uninstall project config")
        );

        assert_eq!(std::fs::read_to_string(&config_path).unwrap(), original);
        assert!(!safe_edit::backup_path(&config_path).unwrap().exists());
    }

    #[test]
    fn malformed_json_recovery_backup_blocks_uninstall_without_changing_live_bytes() {
        let temp = tempfile::tempdir().expect("tempdir");
        let config_path = temp.path().join(".mcp.json");
        let live = concat!(
            "{\n",
            "  \"mcpServers\": {\n",
            "    \"contextstream\": {\n",
            "      \"command\": \"contextstream-mcp\",\n",
            "      \"env\": { \"CONTEXTSTREAM_MANAGED_CONFIG_VERSION\": \"1\" }\n",
            "    },\n",
            "    \"user\": { \"command\": \"preserve\" }\n",
            "  },\n",
            "  \"unrelated\": true\n",
            "}\n"
        );
        std::fs::write(&config_path, live).expect("seed live config");
        let backup = safe_edit::backup_path(&config_path).expect("backup path");
        std::fs::write(&backup, "{ definitely not valid JSON").expect("seed corrupt backup");

        let error =
            remove_contextstream_from_json_path(&config_path, "mcpServers", "contextstream")
                .expect_err("a corrupt recovery snapshot must fail closed");

        assert!(error.to_string().contains("not valid JSON"), "{error:#}");
        assert_eq!(std::fs::read_to_string(&config_path).unwrap(), live);
        assert_eq!(
            std::fs::read_to_string(&backup).unwrap(),
            "{ definitely not valid JSON"
        );
    }

    #[test]
    fn uninstall_never_treats_user_null_or_empty_values_as_an_empty_config() {
        let temp = tempfile::tempdir().expect("tempdir");
        let config_path = temp.path().join(".mcp.json");
        let live = concat!(
            "{\n",
            "  \"mcpServers\": {\n",
            "    \"contextstream\": {\n",
            "      \"command\": \"contextstream-mcp\",\n",
            "      \"env\": { \"CONTEXTSTREAM_MANAGED_CONFIG_VERSION\": \"1\" }\n",
            "    }\n",
            "  },\n",
            "  \"user_null_placeholder\": null,\n",
            "  \"user_empty_settings\": {}\n",
            "}\n"
        );
        std::fs::write(&config_path, live).expect("seed live config");
        let backup = safe_edit::backup_path(&config_path).expect("backup path");
        std::fs::write(
            &backup,
            concat!(
                "{\"mcpServers\":{\"contextstream\":{",
                "\"command\":\"contextstream-mcp\",",
                "\"env\":{\"CONTEXTSTREAM_MANAGED_CONFIG_VERSION\":\"1\"}",
                "}}}\n"
            ),
        )
        .expect("seed wholly managed refresh backup");

        assert!(
            remove_contextstream_from_json_path(&config_path, "mcpServers", "contextstream")
                .expect("surgical uninstall")
        );

        assert!(config_path.exists());
        let value: Value =
            serde_json::from_str(&std::fs::read_to_string(&config_path).unwrap()).unwrap();
        assert_eq!(value["user_null_placeholder"], Value::Null);
        assert_eq!(value["user_empty_settings"], json!({}));
        assert!(value.get("mcpServers").is_none());
    }

    #[test]
    fn json_install_refuses_unowned_same_name_server_without_writing() {
        let _guard = env_test_mutex().lock().unwrap_or_else(|e| e.into_inner());
        let temp = tempfile::tempdir().expect("tempdir");
        let previous_home = std::env::var_os("HOME");
        std::env::set_var("HOME", temp.path());
        let _xdg_guard = XdgConfigGuard::isolate_under(temp.path());

        let config_path = temp.path().join(".claude").join("mcp.json");
        std::fs::create_dir_all(config_path.parent().unwrap()).unwrap();
        let original = concat!(
            "{\n",
            "  \"mcpServers\": {\n",
            "    \"contextstream\": {\n",
            "      \"command\": \"/opt/user/contextstream-mcp\",\n",
            "      \"note\": \"owned by the user\"\n",
            "    }\n",
            "  },\n",
            "  \"theme\": \"custom\"\n",
            "}\n"
        );
        std::fs::write(&config_path, original).unwrap();

        let error =
            write_mcp_config_force_local(&Editor::ClaudeCode, "key", None, None, None, None)
                .expect_err("an unowned same-name server must not be replaced");

        assert!(
            error.to_string().contains("not recognizably managed"),
            "{error:#}"
        );
        assert_eq!(std::fs::read_to_string(&config_path).unwrap(), original);
        assert!(!safe_edit::backup_path(&config_path).unwrap().exists());

        if let Some(value) = previous_home {
            std::env::set_var("HOME", value);
        } else {
            std::env::remove_var("HOME");
        }
    }

    #[test]
    fn json_uninstall_leaves_unowned_same_name_server_byte_identical() {
        let _guard = env_test_mutex().lock().unwrap_or_else(|e| e.into_inner());
        let temp = tempfile::tempdir().expect("tempdir");
        let previous_home = std::env::var_os("HOME");
        std::env::set_var("HOME", temp.path());
        let _xdg_guard = XdgConfigGuard::isolate_under(temp.path());
        let config_path = temp.path().join(".claude").join("mcp.json");
        std::fs::create_dir_all(config_path.parent().expect("config parent"))
            .expect("config directory");
        let original = concat!(
            "{\n",
            "  \"mcpServers\": {\n",
            "    \"contextstream\": { \"command\": \"/opt/user/contextstream-mcp\" }\n",
            "  }\n",
            "}\n"
        );
        std::fs::write(&config_path, original).unwrap();
        let backup_path = safe_edit::backup_path(&config_path).unwrap();
        std::fs::write(&backup_path, "{ corrupt unrelated sidecar").unwrap();

        remove_contextstream_from_mcp_config(&Editor::ClaudeCode)
            .expect("global uninstall should safely ignore an unowned server");
        assert_eq!(std::fs::read_to_string(&config_path).unwrap(), original);
        assert_eq!(
            std::fs::read_to_string(backup_path).unwrap(),
            "{ corrupt unrelated sidecar"
        );

        if let Some(value) = previous_home {
            std::env::set_var("HOME", value);
        } else {
            std::env::remove_var("HOME");
        }
    }

    #[test]
    fn json_uninstall_restores_unowned_backup_server_after_user_edits() {
        let temp = tempfile::tempdir().expect("tempdir");
        let config_path = temp.path().join(".mcp.json");
        let backup_path = safe_edit::backup_path(&config_path).unwrap();
        let backup = concat!(
            "{\n",
            "  \"mcpServers\": {\n",
            "    \"contextstream\": {\n",
            "      \"command\": \"/opt/user/contextstream-mcp\",\n",
            "      \"note\": \"restore me\"\n",
            "    },\n",
            "    \"other\": { \"command\": \"other\" }\n",
            "  },\n",
            "  \"theme\": \"before\"\n",
            "}\n"
        );
        let live = concat!(
            "{\n",
            "  \"mcpServers\": {\n",
            "    \"contextstream\": {\n",
            "      \"command\": \"contextstream-mcp\",\n",
            "      \"env\": { \"CONTEXTSTREAM_MANAGED_CONFIG_VERSION\": \"1\" }\n",
            "    },\n",
            "    \"other\": { \"command\": \"other\" }\n",
            "  },\n",
            "  \"theme\": \"edited-after-install\",\n",
            "  \"new_user_key\": true\n",
            "}\n"
        );
        std::fs::write(&config_path, live).unwrap();
        std::fs::write(&backup_path, backup).unwrap();

        assert!(
            remove_contextstream_from_json_path(&config_path, "mcpServers", "contextstream")
                .expect("surgical uninstall")
        );

        let after: Value =
            serde_json::from_str(&std::fs::read_to_string(&config_path).unwrap()).unwrap();
        assert_eq!(
            after["mcpServers"]["contextstream"]["command"],
            "/opt/user/contextstream-mcp"
        );
        assert_eq!(after["mcpServers"]["contextstream"]["note"], "restore me");
        assert_eq!(after["theme"], "edited-after-install");
        assert_eq!(after["new_user_key"], true);
        assert!(
            backup_path.exists(),
            "a non-exact recovery snapshot must remain available"
        );
    }

    #[test]
    fn codex_install_refuses_unowned_same_name_table_without_writing() {
        let _guard = env_test_mutex().lock().unwrap_or_else(|e| e.into_inner());
        let temp = tempfile::tempdir().expect("tempdir");
        let previous_home = std::env::var_os("HOME");
        std::env::set_var("HOME", temp.path());
        let _xdg_guard = XdgConfigGuard::isolate_under(temp.path());

        let config_path = temp.path().join(".codex").join("config.toml");
        std::fs::create_dir_all(config_path.parent().unwrap()).unwrap();
        let original = concat!(
            "model = \"gpt-user\"\n",
            "\n",
            "[mcp_servers.contextstream]\n",
            "command = \"/opt/user/contextstream-mcp\"\n",
            "note = \"owned by the user\"\n"
        );
        std::fs::write(&config_path, original).unwrap();

        let error = write_mcp_config_force_local(&Editor::Codex, "key", None, None, None, None)
            .expect_err("an unowned Codex table must not be replaced");

        assert!(error.to_string().contains("not recognizably"), "{error:#}");
        assert_eq!(std::fs::read_to_string(&config_path).unwrap(), original);
        assert!(!safe_edit::backup_path(&config_path).unwrap().exists());

        if let Some(value) = previous_home {
            std::env::set_var("HOME", value);
        } else {
            std::env::remove_var("HOME");
        }
    }

    #[test]
    fn codex_uninstall_leaves_unowned_same_name_table_byte_identical() {
        let temp = tempfile::tempdir().expect("tempdir");
        let config_path = temp.path().join("config.toml");
        let original = concat!(
            "model = \"gpt-user\"\n",
            "\n",
            "[mcp_servers.contextstream]\n",
            "command = \"/opt/user/contextstream-mcp\"\n"
        );
        std::fs::write(&config_path, original).unwrap();
        let backup_path = safe_edit::backup_path(&config_path).unwrap();
        std::fs::write(&backup_path, "not valid TOML = [").unwrap();

        remove_contextstream_from_codex_toml(&config_path)
            .expect("uninstall should safely ignore an unowned table");

        assert_eq!(std::fs::read_to_string(&config_path).unwrap(), original);
        assert_eq!(
            std::fs::read_to_string(backup_path).unwrap(),
            "not valid TOML = ["
        );
    }

    #[test]
    fn codex_uninstall_restores_unowned_backup_table_after_user_edits() {
        let temp = tempfile::tempdir().expect("tempdir");
        let config_path = temp.path().join("config.toml");
        let backup_path = safe_edit::backup_path(&config_path).unwrap();
        let backup = concat!(
            "model = \"before\"\n",
            "\n",
            "[mcp_servers.contextstream]\n",
            "command = \"/opt/user/contextstream-mcp\"\n",
            "note = \"restore me\"\n"
        );
        let live = concat!(
            "model = \"edited-after-install\"\n",
            "new_user_key = true\n",
            "\n",
            "# ContextStream MCP Server Configuration\n",
            "[mcp_servers.contextstream]\n",
            "command = \"contextstream-mcp\"\n",
            "args = []\n",
            "\n",
            "[mcp_servers.contextstream.env]\n",
            "CONTEXTSTREAM_MANAGED_CONFIG_VERSION = \"1\"\n"
        );
        std::fs::write(&config_path, live).unwrap();
        std::fs::write(&backup_path, backup).unwrap();

        remove_contextstream_from_codex_toml(&config_path).expect("surgical Codex uninstall");

        let after = std::fs::read_to_string(&config_path).unwrap();
        let parsed = parse_codex_toml(&after, &config_path).unwrap();
        let restored = contextstream_toml_item(&parsed).expect("original server restored");
        assert_eq!(
            toml_item_string(restored, "command"),
            Some("/opt/user/contextstream-mcp")
        );
        assert_eq!(toml_item_string(restored, "note"), Some("restore me"));
        assert_eq!(
            parsed
                .get("model")
                .and_then(Item::as_value)
                .and_then(toml_edit::Value::as_str),
            Some("edited-after-install")
        );
        assert_eq!(
            parsed
                .get("new_user_key")
                .and_then(Item::as_value)
                .and_then(toml_edit::Value::as_bool),
            Some(true)
        );
        assert!(
            backup_path.exists(),
            "a non-exact recovery snapshot must remain available"
        );
    }
}
