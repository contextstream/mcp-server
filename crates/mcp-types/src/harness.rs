//! Canonical coding-harness capabilities and readiness evidence.
//!
//! This module is dependency-neutral so setup, hooks, MCP transports, telemetry,
//! and the model registry can share one stable harness taxonomy. File-system
//! locations remain setup concerns; this module only describes capabilities
//! that affect teaching and readiness semantics.

use chrono::{DateTime, Utc};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// Schema version for serialized harness capability profiles.
pub const HARNESS_PROFILE_SCHEMA_VERSION: u16 = 1;

/// Schema version for serialized harness readiness evidence.
pub const HARNESS_READINESS_SCHEMA_VERSION: u16 = 1;

/// A ContextStream-supported coding harness or first-party MCP host surface.
///
/// Serialized ids are stable API/storage values. New variants must also be
/// added to [`HarnessId::ALL`] and, when setup-installable, to
/// [`HarnessId::INSTALLABLE`].
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize, JsonSchema,
)]
pub enum HarnessId {
    #[serde(rename = "claude")]
    ClaudeCode,
    #[serde(rename = "cursor")]
    Cursor,
    #[serde(rename = "windsurf")]
    Windsurf,
    #[serde(rename = "copilot")]
    Copilot,
    #[serde(rename = "cline")]
    Cline,
    #[serde(rename = "kilo")]
    KiloCode,
    #[serde(rename = "roo")]
    RooCode,
    #[serde(rename = "codex")]
    Codex,
    #[serde(rename = "aider")]
    Aider,
    #[serde(rename = "antigravity")]
    Antigravity,
    #[serde(rename = "opencode")]
    OpenCode,
    #[serde(rename = "chatgpt-gateway")]
    ChatGptGateway,
    #[serde(rename = "openai-responses")]
    OpenAiResponses,
    #[serde(rename = "contextstream-cli")]
    ContextStreamCli,
    /// ContextCode — ContextStream's own agent harness (`csc`). Runtime-only:
    /// it drives the hosted MCP surface directly and receives dynamic guidance
    /// through tool results rather than installed rules files or hooks.
    #[serde(rename = "contextcode")]
    ContextCode,
}

impl HarnessId {
    /// Every canonical harness id, including runtime-only integration surfaces.
    pub const ALL: &'static [Self] = &[
        Self::ClaudeCode,
        Self::Cursor,
        Self::Windsurf,
        Self::Copilot,
        Self::Cline,
        Self::KiloCode,
        Self::RooCode,
        Self::Codex,
        Self::Aider,
        Self::Antigravity,
        Self::OpenCode,
        Self::ChatGptGateway,
        Self::OpenAiResponses,
        Self::ContextStreamCli,
        Self::ContextCode,
    ];

    /// Harnesses that the local setup wizard can configure.
    pub const INSTALLABLE: &'static [Self] = &[
        Self::ClaudeCode,
        Self::Cursor,
        Self::Windsurf,
        Self::Copilot,
        Self::Cline,
        Self::KiloCode,
        Self::RooCode,
        Self::Codex,
        Self::Aider,
        Self::Antigravity,
        Self::OpenCode,
    ];

    /// Stable, bounded identifier used in configs, telemetry, and state.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ClaudeCode => "claude",
            Self::Cursor => "cursor",
            Self::Windsurf => "windsurf",
            Self::Copilot => "copilot",
            Self::Cline => "cline",
            Self::KiloCode => "kilo",
            Self::RooCode => "roo",
            Self::Codex => "codex",
            Self::Aider => "aider",
            Self::Antigravity => "antigravity",
            Self::OpenCode => "opencode",
            Self::ChatGptGateway => "chatgpt-gateway",
            Self::OpenAiResponses => "openai-responses",
            Self::ContextStreamCli => "contextstream-cli",
            Self::ContextCode => "contextcode",
        }
    }

    /// Human-readable product name.
    pub const fn display_name(self) -> &'static str {
        match self {
            Self::ClaudeCode => "Claude Code",
            Self::Cursor => "Cursor",
            Self::Windsurf => "Windsurf",
            Self::Copilot => "GitHub Copilot (VS Code)",
            Self::Cline => "Cline (VS Code)",
            Self::KiloCode => "Kilo Code",
            Self::RooCode => "Roo Code (VS Code)",
            Self::Codex => "OpenAI Codex CLI",
            Self::Aider => "Aider",
            Self::Antigravity => "Antigravity",
            Self::OpenCode => "OpenCode CLI",
            Self::ChatGptGateway => "ChatGPT Gateway",
            Self::OpenAiResponses => "OpenAI Responses",
            Self::ContextStreamCli => "ContextStream CLI",
            Self::ContextCode => "ContextCode",
        }
    }

    /// Resolve a documented exact alias.
    ///
    /// This deliberately performs no substring or fuzzy matching. A third-party
    /// host named `my-cursor-proxy`, for example, is not Cursor.
    pub fn from_alias(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "claude" | "claude-code" | "claude_code" => Some(Self::ClaudeCode),
            "cursor" => Some(Self::Cursor),
            "windsurf" | "cascade" => Some(Self::Windsurf),
            "copilot" | "github-copilot" | "github_copilot" => Some(Self::Copilot),
            "cline" => Some(Self::Cline),
            "kilo" | "kilo-code" | "kilo_code" | "kilocode" => Some(Self::KiloCode),
            "roo" | "roo-code" | "roo_code" | "roocode" => Some(Self::RooCode),
            "codex" | "codex-cli" | "codex_cli" => Some(Self::Codex),
            "aider" => Some(Self::Aider),
            "antigravity" | "gemini-antigravity" => Some(Self::Antigravity),
            "opencode" | "open-code" | "open_code" => Some(Self::OpenCode),
            "chatgpt"
            | "chatgpt-gateway"
            | "chatgpt_gateway"
            | "chatgpt-mcp-gateway"
            | "chatgpt-gateway-e2e" => Some(Self::ChatGptGateway),
            "openai-responses" | "openai_responses" | "openai-responses-e2e" => {
                Some(Self::OpenAiResponses)
            }
            "contextstream-cli" | "contextstream_cli" | "cli" => Some(Self::ContextStreamCli),
            "contextcode" | "context-code" | "context_code" | "csc" | "contextcode-cli"
            | "contextcode-engine" | "contextcode-vscode" => Some(Self::ContextCode),
            _ => None,
        }
    }

    /// Resolve an exact product id with an optional version/user-agent suffix.
    ///
    /// Examples accepted: `Claude-Code/1.0`, `cursor 0.46`, and `codex`.
    /// Unknown prefixes remain unknown even if they contain a known name.
    pub fn from_client_hint(value: &str) -> Option<Self> {
        let normalized = value.trim().to_ascii_lowercase();
        if let Some(id) = Self::from_alias(&normalized) {
            return Some(id);
        }

        for separator in ['/', ' '] {
            if let Some((product, suffix)) = normalized.split_once(separator) {
                if !suffix.trim().is_empty() {
                    if let Some(id) = Self::from_alias(product) {
                        return Some(id);
                    }
                }
            }
        }

        None
    }

    /// Capability profile for this harness.
    pub const fn profile(self) -> HarnessProfile {
        match self {
            Self::ClaudeCode => HarnessProfile::installable(
                self,
                McpConfigFormat::Json,
                RulesFormat::Markdown,
                HookCapabilities::all(),
                true,
                true,
                TeachingLoadEvidence::DirectHook,
            ),
            Self::Cursor => HarnessProfile::installable(
                self,
                McpConfigFormat::Json,
                RulesFormat::CursorMdc,
                HookCapabilities::new(true, true, true, true, false),
                true,
                true,
                TeachingLoadEvidence::BehavioralInference,
            ),
            Self::Windsurf => HarnessProfile::installable(
                self,
                McpConfigFormat::Json,
                RulesFormat::Markdown,
                HookCapabilities::new(false, true, true, true, false),
                true,
                true,
                TeachingLoadEvidence::BehavioralInference,
            ),
            Self::Copilot => HarnessProfile::installable(
                self,
                McpConfigFormat::Json,
                RulesFormat::Markdown,
                HookCapabilities::none(),
                false,
                false,
                TeachingLoadEvidence::BehavioralInference,
            ),
            Self::Cline => HarnessProfile::installable(
                self,
                McpConfigFormat::VsCodeJson,
                RulesFormat::Markdown,
                HookCapabilities::new(false, true, true, true, false),
                true,
                true,
                TeachingLoadEvidence::BehavioralInference,
            ),
            Self::KiloCode => HarnessProfile::installable(
                self,
                McpConfigFormat::Jsonc,
                RulesFormat::Markdown,
                HookCapabilities::none(),
                false,
                true,
                TeachingLoadEvidence::BehavioralInference,
            ),
            Self::RooCode => HarnessProfile::installable(
                self,
                McpConfigFormat::VsCodeJson,
                RulesFormat::Markdown,
                HookCapabilities::new(false, true, true, true, false),
                false,
                true,
                TeachingLoadEvidence::BehavioralInference,
            ),
            Self::Codex => HarnessProfile::installable(
                self,
                McpConfigFormat::Toml,
                RulesFormat::Markdown,
                HookCapabilities::none(),
                false,
                false,
                TeachingLoadEvidence::BehavioralInference,
            ),
            Self::Aider => HarnessProfile {
                schema_version: HARNESS_PROFILE_SCHEMA_VERSION,
                id: self,
                display_name: self.display_name(),
                installable: true,
                mcp_support: McpTransportSupport::None,
                mcp_config_format: McpConfigFormat::None,
                rules_format: RulesFormat::AiderYaml,
                rules_auto_loaded: true,
                hooks: HookCapabilities::none(),
                hard_first_call_enforcement: false,
                dynamic_guidance: false,
                teaching_load_evidence: TeachingLoadEvidence::NotObservable,
            },
            Self::Antigravity => HarnessProfile::installable(
                self,
                McpConfigFormat::Json,
                RulesFormat::Markdown,
                HookCapabilities::none(),
                false,
                false,
                TeachingLoadEvidence::BehavioralInference,
            ),
            Self::OpenCode => HarnessProfile::installable(
                self,
                McpConfigFormat::Json,
                RulesFormat::Markdown,
                HookCapabilities::none(),
                false,
                false,
                TeachingLoadEvidence::BehavioralInference,
            ),
            Self::ChatGptGateway | Self::OpenAiResponses => HarnessProfile::runtime_only(
                self,
                McpTransportSupport::RemoteOnly,
                TeachingLoadEvidence::BehavioralInference,
            ),
            Self::ContextStreamCli => HarnessProfile::runtime_only(
                self,
                McpTransportSupport::LocalAndRemote,
                TeachingLoadEvidence::BehavioralInference,
            ),
            // ContextCode reaches the hosted MCP surface over both transports
            // and re-reads tool-result guidance every turn, so it gets the
            // capability-aware (MCP-tools) teaching path instead of the
            // capability-free unknown-client path. No rules files, hooks, or
            // hard first-call enforcement are claimed for it.
            Self::ContextCode => HarnessProfile::runtime_only(
                self,
                McpTransportSupport::LocalAndRemote,
                TeachingLoadEvidence::BehavioralInference,
            ),
        }
    }
}

/// MCP transport support exposed by a harness.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum McpTransportSupport {
    None,
    LocalOnly,
    RemoteOnly,
    LocalAndRemote,
}

impl McpTransportSupport {
    pub const fn supports_local(self) -> bool {
        matches!(self, Self::LocalOnly | Self::LocalAndRemote)
    }

    pub const fn supports_remote(self) -> bool {
        matches!(self, Self::RemoteOnly | Self::LocalAndRemote)
    }
}

/// Editor-owned storage format for its MCP server entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum McpConfigFormat {
    None,
    Json,
    Jsonc,
    VsCodeJson,
    Toml,
}

/// Managed rules format understood by a harness.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum RulesFormat {
    None,
    Markdown,
    CursorMdc,
    AiderYaml,
}

/// Lifecycle events that can provide dynamic teaching or readiness evidence.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct HookCapabilities {
    pub session_start: bool,
    pub user_prompt_submit: bool,
    pub pre_tool_use: bool,
    pub post_tool_use: bool,
    pub instructions_loaded: bool,
}

impl HookCapabilities {
    pub const fn new(
        session_start: bool,
        user_prompt_submit: bool,
        pre_tool_use: bool,
        post_tool_use: bool,
        instructions_loaded: bool,
    ) -> Self {
        Self {
            session_start,
            user_prompt_submit,
            pre_tool_use,
            post_tool_use,
            instructions_loaded,
        }
    }

    pub const fn all() -> Self {
        Self::new(true, true, true, true, true)
    }

    pub const fn none() -> Self {
        Self::new(false, false, false, false, false)
    }

    pub const fn any(self) -> bool {
        self.session_start
            || self.user_prompt_submit
            || self.pre_tool_use
            || self.post_tool_use
            || self.instructions_loaded
    }
}

/// Strongest evidence available that teaching entered a harness context.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum TeachingLoadEvidence {
    /// A host lifecycle event directly reports the instruction file load.
    DirectHook,
    /// Correct behavior can support an explicitly labelled inference.
    BehavioralInference,
    /// This integration has no trustworthy load or behavior observation.
    NotObservable,
}

/// Canonical capability snapshot for one harness.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, JsonSchema)]
pub struct HarnessProfile {
    pub schema_version: u16,
    pub id: HarnessId,
    pub display_name: &'static str,
    pub installable: bool,
    pub mcp_support: McpTransportSupport,
    pub mcp_config_format: McpConfigFormat,
    pub rules_format: RulesFormat,
    pub rules_auto_loaded: bool,
    pub hooks: HookCapabilities,
    pub hard_first_call_enforcement: bool,
    pub dynamic_guidance: bool,
    pub teaching_load_evidence: TeachingLoadEvidence,
}

impl HarnessProfile {
    const fn installable(
        id: HarnessId,
        mcp_config_format: McpConfigFormat,
        rules_format: RulesFormat,
        hooks: HookCapabilities,
        hard_first_call_enforcement: bool,
        dynamic_guidance: bool,
        teaching_load_evidence: TeachingLoadEvidence,
    ) -> Self {
        Self {
            schema_version: HARNESS_PROFILE_SCHEMA_VERSION,
            id,
            display_name: id.display_name(),
            installable: true,
            mcp_support: McpTransportSupport::LocalAndRemote,
            mcp_config_format,
            rules_format,
            rules_auto_loaded: true,
            hooks,
            hard_first_call_enforcement,
            dynamic_guidance,
            teaching_load_evidence,
        }
    }

    const fn runtime_only(
        id: HarnessId,
        mcp_support: McpTransportSupport,
        teaching_load_evidence: TeachingLoadEvidence,
    ) -> Self {
        Self {
            schema_version: HARNESS_PROFILE_SCHEMA_VERSION,
            id,
            display_name: id.display_name(),
            installable: false,
            mcp_support,
            mcp_config_format: McpConfigFormat::None,
            rules_format: RulesFormat::None,
            rules_auto_loaded: false,
            hooks: HookCapabilities::none(),
            hard_first_call_enforcement: false,
            dynamic_guidance: false,
            teaching_load_evidence,
        }
    }
}

/// Ordered readiness milestones for one installed harness.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize, JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum HarnessReadinessStage {
    Configured,
    Taught,
    Loaded,
    Connected,
    Grounded,
    Practicing,
}

impl HarnessReadinessStage {
    pub const ALL: &'static [Self] = &[
        Self::Configured,
        Self::Taught,
        Self::Loaded,
        Self::Connected,
        Self::Grounded,
        Self::Practicing,
    ];

    pub const fn rank(self) -> u8 {
        match self {
            Self::Configured => 0,
            Self::Taught => 1,
            Self::Loaded => 2,
            Self::Connected => 3,
            Self::Grounded => 4,
            Self::Practicing => 5,
        }
    }
}

/// Confidence/outcome attached to a readiness stage observation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ReadinessEvidenceStatus {
    Verified,
    Inferred,
    Pending,
    NotObservable,
    Stale,
    Failed,
}

impl ReadinessEvidenceStatus {
    pub const fn is_ready(self) -> bool {
        matches!(self, Self::Verified | Self::Inferred)
    }
}

/// Typed source for readiness evidence. No free-form user content is stored.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ReadinessEvidenceSource {
    ManagedMcpConfig,
    ManagedRules,
    InstructionsLoadedHook,
    McpProtocolRequest,
    InitTool,
    ContextTool,
    ComplianceCheck,
    RuntimeBehavior,
}

/// Privacy-bounded evidence record used by local and remote readiness ledgers.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct HarnessReadinessEvidence {
    pub schema_version: u16,
    pub harness_id: HarnessId,
    pub stage: HarnessReadinessStage,
    pub status: ReadinessEvidenceStatus,
    pub source: ReadinessEvidenceSource,
    #[schemars(with = "String")]
    pub observed_at: DateTime<Utc>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub teaching_version: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub managed_config_version: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rules_hash: Option<String>,
}

impl HarnessReadinessEvidence {
    pub fn new(
        harness_id: HarnessId,
        stage: HarnessReadinessStage,
        status: ReadinessEvidenceStatus,
        source: ReadinessEvidenceSource,
        observed_at: DateTime<Utc>,
    ) -> Self {
        Self {
            schema_version: HARNESS_READINESS_SCHEMA_VERSION,
            harness_id,
            stage,
            status,
            source,
            observed_at,
            teaching_version: None,
            managed_config_version: None,
            rules_hash: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn canonical_ids_and_aliases_are_unique_and_round_trip() {
        let mut ids = HashSet::new();
        for harness in HarnessId::ALL {
            assert!(ids.insert(harness.as_str()), "duplicate id: {harness:?}");
            assert_eq!(HarnessId::from_alias(harness.as_str()), Some(*harness));

            let encoded = serde_json::to_string(harness).expect("serialize harness id");
            let decoded: HarnessId =
                serde_json::from_str(&encoded).expect("deserialize harness id");
            assert_eq!(decoded, *harness);
        }
    }

    #[test]
    fn installable_profiles_are_exhaustive_and_bounded() {
        assert_eq!(HarnessId::INSTALLABLE.len(), 11);
        let unique: HashSet<_> = HarnessId::INSTALLABLE.iter().copied().collect();
        assert_eq!(unique.len(), HarnessId::INSTALLABLE.len());

        for harness in HarnessId::ALL {
            let profile = harness.profile();
            assert_eq!(profile.schema_version, HARNESS_PROFILE_SCHEMA_VERSION);
            assert_eq!(profile.id, *harness);
            assert_eq!(profile.display_name, harness.display_name());
            assert_eq!(
                profile.installable,
                HarnessId::INSTALLABLE.contains(harness)
            );
            assert!(harness.as_str().len() <= 64);
        }
    }

    #[test]
    fn client_hint_matching_is_strict_not_substring_based() {
        assert_eq!(
            HarnessId::from_client_hint("Claude-Code/1.0"),
            Some(HarnessId::ClaudeCode)
        );
        assert_eq!(
            HarnessId::from_client_hint("cursor 0.46"),
            Some(HarnessId::Cursor)
        );
        assert_eq!(
            HarnessId::from_client_hint("OpenAI-Responses"),
            Some(HarnessId::OpenAiResponses)
        );
        assert_eq!(
            HarnessId::from_client_hint("chatgpt-gateway-e2e"),
            Some(HarnessId::ChatGptGateway)
        );
        assert_eq!(
            HarnessId::from_client_hint("openai-responses-e2e"),
            Some(HarnessId::OpenAiResponses)
        );
        assert_eq!(
            HarnessId::from_client_hint("contextcode-vscode/0.7.156"),
            Some(HarnessId::ContextCode)
        );
        assert_eq!(
            HarnessId::from_client_hint("csc 0.7"),
            Some(HarnessId::ContextCode)
        );
        assert_eq!(HarnessId::from_client_hint("my-cursor-proxy"), None);
        assert_eq!(HarnessId::from_client_hint("claude-code-wrapper"), None);
        assert_eq!(HarnessId::from_client_hint(""), None);
    }

    #[test]
    fn direct_instruction_load_evidence_is_never_inferred_for_other_hosts() {
        let direct: Vec<_> = HarnessId::ALL
            .iter()
            .copied()
            .filter(|harness| {
                harness.profile().teaching_load_evidence == TeachingLoadEvidence::DirectHook
            })
            .collect();
        assert_eq!(direct, vec![HarnessId::ClaudeCode]);
        assert!(HarnessId::ClaudeCode.profile().hooks.instructions_loaded);
        assert!(!HarnessId::Cursor.profile().hooks.instructions_loaded);
    }

    #[test]
    fn contextcode_is_runtime_only_with_real_mcp_transport() {
        let profile = HarnessId::ContextCode.profile();
        assert!(!profile.installable);
        assert!(!HarnessId::INSTALLABLE.contains(&HarnessId::ContextCode));
        assert_eq!(profile.mcp_support, McpTransportSupport::LocalAndRemote);
        assert_eq!(profile.rules_format, RulesFormat::None);
        assert_eq!(profile.hooks, HookCapabilities::none());
        assert!(!profile.hard_first_call_enforcement);
        assert_eq!(
            profile.teaching_load_evidence,
            TeachingLoadEvidence::BehavioralInference
        );
        for alias in ["contextcode", "csc", "context-code", "contextcode-cli"] {
            assert_eq!(HarnessId::from_alias(alias), Some(HarnessId::ContextCode));
        }
    }

    #[test]
    fn aider_has_rules_but_no_mcp_or_behavioral_readiness_claim() {
        let profile = HarnessId::Aider.profile();
        assert_eq!(profile.mcp_support, McpTransportSupport::None);
        assert_eq!(profile.mcp_config_format, McpConfigFormat::None);
        assert_eq!(profile.rules_format, RulesFormat::AiderYaml);
        assert_eq!(
            profile.teaching_load_evidence,
            TeachingLoadEvidence::NotObservable
        );
    }

    #[test]
    fn readiness_stages_are_monotonic_and_evidence_round_trips() {
        for (index, stage) in HarnessReadinessStage::ALL.iter().enumerate() {
            assert_eq!(stage.rank(), index as u8);
        }

        let observed_at = DateTime::parse_from_rfc3339("2026-07-28T12:00:00Z")
            .expect("timestamp")
            .with_timezone(&Utc);
        let evidence = HarnessReadinessEvidence::new(
            HarnessId::Codex,
            HarnessReadinessStage::Grounded,
            ReadinessEvidenceStatus::Verified,
            ReadinessEvidenceSource::ContextTool,
            observed_at,
        );
        let encoded = serde_json::to_string(&evidence).expect("serialize evidence");
        let decoded: HarnessReadinessEvidence =
            serde_json::from_str(&encoded).expect("deserialize evidence");
        assert_eq!(decoded, evidence);
        assert!(decoded.status.is_ready());
        assert!(!ReadinessEvidenceStatus::NotObservable.is_ready());
    }

    #[test]
    fn json_schemas_include_stable_enum_values() {
        let harness_schema =
            serde_json::to_value(schemars::schema_for!(HarnessId)).expect("harness schema");
        let readiness_schema =
            serde_json::to_value(schemars::schema_for!(HarnessReadinessEvidence))
                .expect("readiness schema");
        let harness_text = harness_schema.to_string();
        let readiness_text = readiness_schema.to_string();
        assert!(harness_text.contains("\"claude\""));
        assert!(harness_text.contains("\"opencode\""));
        assert!(readiness_text.contains("\"not_observable\""));
        assert!(readiness_text.contains("\"practicing\""));
    }
}
