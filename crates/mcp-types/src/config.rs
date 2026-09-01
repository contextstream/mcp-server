//! Configuration types for the MCP server.

use crate::account_mode::AccountModePreference;
use serde::{Deserialize, Serialize};
use std::str::FromStr;
use uuid::Uuid;

/// Default ContextStream API URL.
pub const DEFAULT_API_URL: &str = "https://api.contextstream.io";

/// MCP server version.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Server configuration loaded from environment variables or config file.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    /// Base API URL (e.g., https://api.contextstream.io)
    pub api_url: String,

    /// API key for authentication (mutually exclusive with jwt)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub api_key: Option<String>,

    /// JWT token for authentication (mutually exclusive with api_key)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub jwt: Option<String>,

    /// Default workspace ID (UUID)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub default_workspace_id: Option<Uuid>,

    /// Default project ID (UUID)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub default_project_id: Option<Uuid>,

    /// Custom user agent string
    pub user_agent: String,

    /// Allow header-based authentication (no credentials required)
    #[serde(default)]
    pub allow_header_auth: bool,

    /// Enable Context Pack in context_smart
    #[serde(default = "default_true")]
    pub context_pack_enabled: bool,

    /// Show timing information in responses
    #[serde(default)]
    pub show_timing: bool,

    /// Tool mode: light, standard, or complete
    #[serde(default)]
    pub toolset: Toolset,

    /// Log level: quiet, normal, or verbose
    #[serde(default)]
    pub log_level: LogLevel,

    /// Output format: compact or pretty
    #[serde(default)]
    pub output_format: OutputFormat,

    /// Enable progressive disclosure mode (start with core tools only)
    #[serde(default)]
    pub progressive_mode: bool,

    /// Enable router mode (meta-tools only)
    #[serde(default)]
    pub router_mode: bool,

    /// Enable consolidated domain tools
    #[serde(default = "default_true")]
    pub consolidated_mode: bool,

    /// Auto-hide integration tools when not connected
    #[serde(default = "default_true")]
    pub auto_hide_integrations: bool,

    /// Enable the ContextCapsule domain tool. Defaults to true; set
    /// `CONTEXTSTREAM_CAPSULE_ENABLED=0` (or `false`/`no`/`off`) to opt out.
    #[serde(default = "default_true")]
    pub capsule_enabled: bool,

    /// Default search result limit
    #[serde(default = "default_search_limit")]
    pub search_limit: usize,

    /// Maximum characters per search result
    #[serde(default = "default_search_max_chars")]
    pub search_max_chars: usize,

    /// Whether transcript capture is enabled by default for this session
    #[serde(default)]
    pub transcripts_enabled: bool,

    /// Whether hook transcript capture is enabled by default
    #[serde(default)]
    pub hook_transcripts_enabled: bool,

    /// Tool surface profile for client-specific tool exposure.
    #[serde(default)]
    pub tool_surface_profile: ToolSurfaceProfile,

    /// True when running as an HTTP gateway (remote transport).
    /// Disables local filesystem validation for client-side paths.
    #[serde(default)]
    pub is_http_transport: bool,

    /// Non-interactive default for team vs personal execution mode.
    #[serde(default)]
    pub account_mode_preference: AccountModePreference,
}

fn default_true() -> bool {
    true
}

fn default_search_limit() -> usize {
    10
}

fn default_search_max_chars() -> usize {
    800
}

impl Default for Config {
    fn default() -> Self {
        Self {
            api_url: std::env::var("CONTEXTSTREAM_API_URL")
                .ok()
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| DEFAULT_API_URL.to_string()),
            api_key: None,
            jwt: None,
            default_workspace_id: None,
            default_project_id: None,
            user_agent: format!("contextstream-mcp-rust/{}", VERSION),
            allow_header_auth: false,
            context_pack_enabled: true,
            show_timing: false,
            toolset: Toolset::default(),
            log_level: LogLevel::default(),
            output_format: OutputFormat::default(),
            progressive_mode: false,
            router_mode: false,
            consolidated_mode: true,
            auto_hide_integrations: true,
            capsule_enabled: std::env::var("CONTEXTSTREAM_CAPSULE_ENABLED")
                .ok()
                .map(|value| !matches!(value.as_str(), "0" | "false" | "FALSE" | "no" | "off"))
                .unwrap_or(true),
            search_limit: 10,
            search_max_chars: 800,
            transcripts_enabled: false,
            hook_transcripts_enabled: false,
            tool_surface_profile: ToolSurfaceProfile::default(),
            is_http_transport: false,
            account_mode_preference: std::env::var("CONTEXTSTREAM_ACCOUNT_MODE")
                .ok()
                .and_then(|value| value.parse().ok())
                .unwrap_or_default(),
        }
    }
}

impl Config {
    /// Check if the config has valid authentication.
    pub fn has_auth(&self) -> bool {
        self.api_key.is_some() || self.jwt.is_some() || self.allow_header_auth
    }

    /// Get the authentication header value.
    pub fn auth_header(&self) -> Option<(&'static str, String)> {
        if let Some(ref key) = self.api_key {
            Some(("X-API-Key", key.clone()))
        } else if let Some(ref jwt) = self.jwt {
            Some(("Authorization", format!("Bearer {}", jwt)))
        } else {
            None
        }
    }

    /// Return a copy of this config with a request-scoped override applied.
    pub fn apply_override(&self, override_config: &ConfigOverride) -> Self {
        let mut next = self.clone();

        if let Some(value) = override_config.context_pack_enabled {
            next.context_pack_enabled = value;
        }
        if let Some(value) = override_config.toolset {
            next.toolset = value;
        }
        if let Some(value) = override_config.output_format {
            next.output_format = value;
        }
        if let Some(value) = override_config.progressive_mode {
            next.progressive_mode = value;
        }
        if let Some(value) = override_config.router_mode {
            next.router_mode = value;
        }
        if let Some(value) = override_config.consolidated_mode {
            next.consolidated_mode = value;
        }
        if let Some(value) = override_config.auto_hide_integrations {
            next.auto_hide_integrations = value;
        }
        if let Some(value) = override_config.search_limit {
            next.search_limit = value;
        }
        if let Some(value) = override_config.search_max_chars {
            next.search_max_chars = value;
        }
        if let Some(value) = override_config.transcripts_enabled {
            next.transcripts_enabled = value;
        }
        if let Some(value) = override_config.hook_transcripts_enabled {
            next.hook_transcripts_enabled = value;
        }
        if let Some(value) = override_config.tool_surface_profile {
            next.tool_surface_profile = value;
        }

        next
    }
}

/// Request-scoped config overrides for hosted remote MCP usage.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct ConfigOverride {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub context_pack_enabled: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub toolset: Option<Toolset>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output_format: Option<OutputFormat>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub progressive_mode: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub router_mode: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub consolidated_mode: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub auto_hide_integrations: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub search_limit: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub search_max_chars: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub transcripts_enabled: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hook_transcripts_enabled: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_surface_profile: Option<ToolSurfaceProfile>,
    /// Per-request override for the acceleration layer's master enable
    /// flag. The new header/field takes precedence over the deprecated
    /// Atlas alias below.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub acceleration_enabled: Option<bool>,
    /// Deprecated compatibility alias for
    /// [`ConfigOverride::acceleration_enabled`]. Accepted during the
    /// migration window so older clients can still force-disable or
    /// canary the optional layer, but new clients must send
    /// `acceleration_enabled` / `X-ContextStream-Acceleration-Enabled`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub atlas_enabled: Option<bool>,
}

impl ConfigOverride {
    pub fn effective_acceleration_enabled(&self) -> Option<bool> {
        self.acceleration_enabled.or(self.atlas_enabled)
    }

    pub fn is_empty(&self) -> bool {
        self.context_pack_enabled.is_none()
            && self.toolset.is_none()
            && self.output_format.is_none()
            && self.progressive_mode.is_none()
            && self.router_mode.is_none()
            && self.consolidated_mode.is_none()
            && self.auto_hide_integrations.is_none()
            && self.search_limit.is_none()
            && self.search_max_chars.is_none()
            && self.transcripts_enabled.is_none()
            && self.hook_transcripts_enabled.is_none()
            && self.tool_surface_profile.is_none()
            && self.acceleration_enabled.is_none()
            && self.atlas_enabled.is_none()
    }

    pub fn affects_tool_registry(&self) -> bool {
        self.toolset.is_some()
            || self.progressive_mode.is_some()
            || self.router_mode.is_some()
            || self.consolidated_mode.is_some()
            || self.tool_surface_profile.is_some()
    }
}

/// Tool surface profile for client-specific tool exposure.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolSurfaceProfile {
    /// Default MCP behavior with the current broad tool surface.
    #[default]
    Default,
    /// OpenAI/GPT agentic mode with a compact default tool surface and discovery meta-tools.
    OpenaiAgentic,
}

impl FromStr for ToolSurfaceProfile {
    type Err = ();

    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        match s.trim().to_ascii_lowercase().as_str() {
            "default" | "standard" | "full" => Ok(Self::Default),
            "openai" | "openai_agentic" | "agentic" | "agentic_openai" => Ok(Self::OpenaiAgentic),
            _ => Err(()),
        }
    }
}

impl ToolSurfaceProfile {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Default => "default",
            Self::OpenaiAgentic => "openai_agentic",
        }
    }
}

/// Tool mode determining which tools are available.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Toolset {
    /// Minimal set of core tools (~33 tools)
    Light,
    /// Default set including workspace management (~50+ tools)
    #[default]
    Standard,
    /// All tools including integrations and advanced features
    Complete,
}

impl FromStr for Toolset {
    type Err = ();

    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        match s.trim().to_ascii_lowercase().as_str() {
            "light" | "lite" | "minimal" => Ok(Self::Light),
            "standard" | "default" => Ok(Self::Standard),
            "complete" | "full" | "all" => Ok(Self::Complete),
            _ => Err(()),
        }
    }
}

/// Log level for the server.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LogLevel {
    /// Minimal output, only errors
    #[default]
    Quiet,
    /// Clean, user-friendly output
    Normal,
    /// Full debug output
    Verbose,
}

impl FromStr for LogLevel {
    type Err = ();

    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        match s.trim().to_ascii_lowercase().as_str() {
            "quiet" | "silent" => Ok(Self::Quiet),
            "normal" | "default" => Ok(Self::Normal),
            "verbose" | "debug" => Ok(Self::Verbose),
            _ => Err(()),
        }
    }
}

impl LogLevel {
    pub fn is_quiet(&self) -> bool {
        matches!(self, Self::Quiet)
    }

    pub fn is_verbose(&self) -> bool {
        matches!(self, Self::Verbose)
    }
}

/// Output format for tool responses.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum OutputFormat {
    /// Compact output (~30% fewer tokens)
    #[default]
    Compact,
    /// Pretty-printed output
    Pretty,
}

impl FromStr for OutputFormat {
    type Err = ();

    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        match s.trim().to_ascii_lowercase().as_str() {
            "compact" | "default" => Ok(Self::Compact),
            "pretty" | "full" => Ok(Self::Pretty),
            _ => Err(()),
        }
    }
}

/// Authentication override for per-request auth.
#[derive(Debug, Clone, Default)]
pub struct AuthOverride {
    pub api_key: Option<String>,
    pub jwt: Option<String>,
    pub workspace_id: Option<Uuid>,
    pub project_id: Option<Uuid>,
    /// Trusted, low-cardinality request classification. This can only be
    /// constructed from the exact allowlisted ingress value; arbitrary caller
    /// strings must never be stored or forwarded.
    pub traffic_class: Option<TrafficClass>,
}

/// Allowlisted traffic classifications that may cross the MCP/API boundary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrafficClass {
    SyntheticProbe,
}

impl TrafficClass {
    pub const HEADER_NAME: &'static str = "X-ContextStream-Traffic-Class";
    pub const SYNTHETIC_PROBE_VALUE: &'static str = "synthetic-probe";

    /// Parse only the canonical, case-sensitive wire value. Returning a typed
    /// enum prevents arbitrary high-cardinality input from reaching API logs
    /// or metrics through this propagation path.
    pub fn from_header_value(value: &str) -> Option<Self> {
        match value {
            Self::SYNTHETIC_PROBE_VALUE => Some(Self::SyntheticProbe),
            _ => None,
        }
    }

    pub const fn as_header_value(self) -> &'static str {
        match self {
            Self::SyntheticProbe => Self::SYNTHETIC_PROBE_VALUE,
        }
    }

    pub fn is_header_name(value: &str) -> bool {
        value.eq_ignore_ascii_case(Self::HEADER_NAME)
    }
}

/// Per-request identity key used to partition `SessionState` so one caller's
/// session fields (folder_path, workspace_id, project_id, etc.) can never be
/// observed by another caller sharing the same MCP server process.
///
/// Set on each HTTP request by the transport layer from the authenticated
/// subject; consumed by `SessionManager` when it picks the caller's state
/// bucket out of its `DashMap<SessionKey, _>`.
///
/// `Local` is the fallback for CLI / unauthenticated / background-task
/// execution where the MCP process is single-tenant by construction. It is
/// intentionally the ONLY bucket that is shared across callers, and it
/// should only ever be used when auth is not configured at all.
#[derive(Debug, Clone, Hash, PartialEq, Eq)]
pub enum SessionKey {
    /// SHA-256 partition derived from JWT `sub` + optional MCP session id.
    Jwt(String),
    /// SHA-256 hash of the API key — stable per-key bucketing for
    /// api-key-authenticated requests (we don't store the raw key).
    ApiKey(String),
    /// Unauthenticated HTTP request/session. The HTTP transport partitions
    /// this by the validated MCP session id, or by a fresh request nonce when
    /// the header is absent. It is deliberately never cacheable.
    AnonymousHttp(String),
    /// Local CLI / unauthenticated mode. Single-tenant by construction.
    /// This variant is only safe when explicitly installed by the stdio
    /// transport; absence of task-local state is not equivalent to local.
    Local,
}

impl AuthOverride {
    pub fn is_empty(&self) -> bool {
        self.api_key.is_none()
            && self.jwt.is_none()
            && self.workspace_id.is_none()
            && self.project_id.is_none()
            && self.traffic_class.is_none()
    }
}

impl SessionKey {
    /// Build the HTTP session partition for an authenticated JWT subject.
    /// The returned key contains only a versioned SHA-256 digest; neither the
    /// subject nor the MCP session id is retained in the key.
    pub fn for_http_jwt(subject: &str, mcp_session_id: Option<&str>) -> Self {
        Self::Jwt(http_session_partition_digest(
            "jwt",
            subject,
            mcp_session_id,
        ))
    }

    /// Build the HTTP session partition for an API key without retaining the
    /// credential. Variant/domain framing prevents cross-protocol reuse.
    pub fn for_http_api_key(api_key: &str, mcp_session_id: Option<&str>) -> Self {
        Self::ApiKey(http_session_partition_digest(
            "api_key",
            api_key,
            mcp_session_id,
        ))
    }

    /// Build a non-cacheable anonymous HTTP session partition. Callers must
    /// pass either the validated MCP session id or a request-unique nonce.
    pub fn for_anonymous_http(partition_id: &str) -> Self {
        Self::AnonymousHttp(http_session_partition_digest(
            "anonymous_http",
            partition_id,
            None,
        ))
    }

    /// Stable per-caller token suitable for inclusion in Atlas
    /// warm-cache `scope_hash` and Atlas Search `$eq` filters. Two
    /// calls from the SAME identity must return the same token; two
    /// calls from DIFFERENT identities must never collide.
    ///
    /// Returns `None` for `Local` and `AnonymousHttp`. Local stdio gets a
    /// separate per-process identity in the cache layer; anonymous HTTP must
    /// bypass caches entirely.
    ///
    /// The `csuc:v2` migration namespace and length-framed, domain-separated
    /// SHA-256 input make the token stable across binaries while preventing
    /// delimiter ambiguity and cross-variant reuse.
    pub fn atlas_user_scope_token(&self) -> Option<String> {
        match self {
            Self::Jwt(identity) => Some(format!(
                "csuc:v2:j:{}",
                framed_sha256("contextstream-atlas-user-scope-v2", &["jwt", identity])
            )),
            Self::ApiKey(identity) => Some(format!(
                "csuc:v2:k:{}",
                framed_sha256("contextstream-atlas-user-scope-v2", &["api_key", identity])
            )),
            Self::AnonymousHttp(_) | Self::Local => None,
        }
    }
}

fn http_session_partition_digest(
    variant: &str,
    identity: &str,
    mcp_session_id: Option<&str>,
) -> String {
    let digest = match mcp_session_id {
        Some(session_id) => framed_sha256(
            "contextstream-http-session-partition-v2",
            &[variant, identity, "session:some", session_id],
        ),
        None => framed_sha256(
            "contextstream-http-session-partition-v2",
            &[variant, identity, "session:none"],
        ),
    };
    format!("cshttp:v2:{variant}:{}", digest)
}

fn framed_sha256(domain: &str, fields: &[&str]) -> String {
    use sha2::{Digest, Sha256};

    fn update_framed(hasher: &mut Sha256, value: &[u8]) {
        hasher.update((value.len() as u64).to_be_bytes());
        hasher.update(value);
    }

    let mut hasher = Sha256::new();
    update_framed(&mut hasher, domain.as_bytes());
    hasher.update((fields.len() as u64).to_be_bytes());
    for field in fields {
        update_framed(&mut hasher, field.as_bytes());
    }
    hex::encode(hasher.finalize())
}

#[cfg(test)]
mod session_key_tests {
    use super::*;

    #[test]
    fn atlas_user_scope_token_stable_per_identity() {
        let a1 = SessionKey::Jwt("user-abc".to_string()).atlas_user_scope_token();
        let a2 = SessionKey::Jwt("user-abc".to_string()).atlas_user_scope_token();
        assert_eq!(a1, a2, "same JWT sub must yield same token");
    }

    #[test]
    fn atlas_user_scope_token_isolates_distinct_jwts() {
        let a = SessionKey::Jwt("user-a".to_string()).atlas_user_scope_token();
        let b = SessionKey::Jwt("user-b".to_string()).atlas_user_scope_token();
        assert_ne!(a, b, "different JWT subs must yield different tokens");
    }

    #[test]
    fn atlas_user_scope_token_isolates_jwt_from_api_key() {
        let a = SessionKey::Jwt("same-bytes".to_string()).atlas_user_scope_token();
        let b = SessionKey::ApiKey("same-bytes".to_string()).atlas_user_scope_token();
        assert_ne!(
            a, b,
            "JWT and api-key with identical bytes must never collide"
        );
    }

    #[test]
    fn http_session_partitions_do_not_retain_credentials_or_session_ids() {
        let secret = "api-key-must-never-be-retained";
        let session_id = "mcp-session-private";
        let key = SessionKey::for_http_api_key(secret, Some(session_id));
        let SessionKey::ApiKey(partition) = key else {
            panic!("expected API-key partition")
        };
        assert!(partition.starts_with("cshttp:v2:api_key:"));
        assert!(!partition.contains(secret));
        assert!(!partition.contains(session_id));
    }

    #[test]
    fn caller_tokens_use_pinned_v2_vectors() {
        assert_eq!(
            SessionKey::Jwt("user-abc".to_string()).atlas_user_scope_token(),
            Some(
                "csuc:v2:j:c2d1d6439c0ebc2a4947732a3b54e11bb94c1076a74e94f168601b1f6edea36a"
                    .to_string()
            )
        );
        assert_eq!(
            SessionKey::ApiKey("digest-abc".to_string()).atlas_user_scope_token(),
            Some(
                "csuc:v2:k:43289b63aa6e64146c7f41d4d7eca2d6dea0c126da01b9da689d3205171a4488"
                    .to_string()
            )
        );
        assert_eq!(
            SessionKey::for_http_api_key("secret-abc", Some("session-123")),
            SessionKey::ApiKey(
                concat!(
                    "cshttp", ":v2:ap", "i_key:", "f4acd1", "bf4545", "09209c", "57e65a", "61bb2c",
                    "d8adb2", "59af4f", "102705", "d5e831", "e2a590", "446a"
                )
                .to_string()
            )
        );
    }

    #[test]
    fn atlas_user_scope_token_local_is_none() {
        assert_eq!(SessionKey::Local.atlas_user_scope_token(), None);
        assert_eq!(
            SessionKey::for_anonymous_http("request-1").atlas_user_scope_token(),
            None
        );
    }
}

/// Saved credentials from ~/.contextstream/credentials.json
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SavedCredentials {
    #[serde(default = "default_api_url")]
    pub api_url: String,
    pub api_key: String,
}

fn default_api_url() -> String {
    DEFAULT_API_URL.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn traffic_class_only_accepts_the_canonical_synthetic_probe_value() {
        assert_eq!(
            TrafficClass::from_header_value("synthetic-probe"),
            Some(TrafficClass::SyntheticProbe)
        );
        for rejected in [
            "customer",
            "synthetic_probe",
            "Synthetic-Probe",
            " synthetic-probe",
            "synthetic-probe ",
            "synthetic-probe,customer",
            "",
        ] {
            assert_eq!(
                TrafficClass::from_header_value(rejected),
                None,
                "unexpected accepted traffic class: {rejected:?}"
            );
        }
    }

    #[test]
    fn test_default_config() {
        let config = Config::default();
        assert_eq!(config.api_url, DEFAULT_API_URL);
        assert!(!config.has_auth());
    }

    #[test]
    fn test_config_with_api_key() {
        let config = Config {
            api_key: Some("test-key".to_string()),
            ..Default::default()
        };
        assert!(config.has_auth());
        let (header, value) = config.auth_header().unwrap();
        assert_eq!(header, "X-API-Key");
        assert_eq!(value, "test-key");
    }

    #[test]
    fn test_config_apply_override() {
        let config = Config::default();
        let effective = config.apply_override(&ConfigOverride {
            search_limit: Some(25),
            transcripts_enabled: Some(true),
            toolset: Some(Toolset::Complete),
            ..ConfigOverride::default()
        });

        assert_eq!(effective.search_limit, 25);
        assert!(effective.transcripts_enabled);
        assert_eq!(effective.toolset, Toolset::Complete);
    }

    #[test]
    fn test_toolset_parsing() {
        assert_eq!("light".parse::<Toolset>().unwrap(), Toolset::Light);
        assert_eq!("COMPLETE".parse::<Toolset>().unwrap(), Toolset::Complete);
        assert!("unknown".parse::<Toolset>().is_err());
    }

    #[test]
    fn test_log_level_parsing() {
        assert_eq!("quiet".parse::<LogLevel>().unwrap(), LogLevel::Quiet);
        assert_eq!("DEBUG".parse::<LogLevel>().unwrap(), LogLevel::Verbose);
        assert!("invalid".parse::<LogLevel>().is_err());
    }

    #[test]
    fn test_output_format_parsing() {
        assert_eq!(
            "compact".parse::<OutputFormat>().unwrap(),
            OutputFormat::Compact
        );
        assert_eq!(
            "FULL".parse::<OutputFormat>().unwrap(),
            OutputFormat::Pretty
        );
        assert!("invalid".parse::<OutputFormat>().is_err());
    }
}
