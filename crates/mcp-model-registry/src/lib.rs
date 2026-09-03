//! Shared editor + model registry for ContextStream MCP.
//!
//! This module is the single source of truth for "what model is this?" decisions
//! across the MCP runtime. It maps raw model identifiers to the curated model
//! taxonomy and raw editor identifiers to the canonical
//! [`mcp_types::HarnessId`] taxonomy used by setup and readiness.
//!
//! # Design rules
//!
//! * **Strict matching.** Unknown values return `None`. We never invent a
//!   canonical id and never fall through to client/editor strings as model
//!   identifiers. If we can't recognize it, the caller drops it (so the API
//!   stores `NULL` / `unknown`) instead of poisoning the model leaderboard
//!   with editor or provider names.
//! * **Alias normalization.** Common variants (kebab/snake/dot, vendor
//!   prefixes like `anthropic/`) collapse onto the canonical id.
//! * **Visibility flag.** Internal models (e.g. `streampilot`/`kimi`)
//!   keep their canonical id but are tagged `Visibility::Internal` so callers
//!   can filter them from public dashboards without rewriting them to a
//!   shared sentinel.
//!
//! Canonical model ids match the public catalog used by the dashboard
//! (Anthropic Claude families, OpenAI GPT-5 family, Google Gemini, xAI Grok,
//! ContextStream Composer/StreamPilot, Moonshot Kimi, etc.).

use std::collections::HashMap;
use std::sync::OnceLock;

use mcp_types::HarnessId;

/// High-level provider grouping for dashboards.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Provider {
    Anthropic,
    OpenAI,
    Google,
    XAI,
    Moonshot,
    ContextStream,
    Other,
}

/// Tokenizer encoding used by recognized OpenAI models on ContextStream's
/// exact whole-wire accounting path.
pub const OPENAI_TOKENIZER_ENCODING: &str = "o200k_base";

impl Provider {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Anthropic => "anthropic",
            Self::OpenAI => "openai",
            Self::Google => "google",
            Self::XAI => "xai",
            Self::Moonshot => "moonshot",
            Self::ContextStream => "contextstream",
            Self::Other => "other",
        }
    }
}

/// Classification controlling whether an entry shows up on the public
/// Models leaderboard.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Visibility {
    /// A real LLM that should appear in the leaderboard by default.
    Public,
    /// A real LLM curated by us but hidden from the public dashboard
    /// (internal Kimi, …) — surfaced behind a "Show internal" toggle.
    Internal,
    /// Not a model: an editor / CLI / hook / harness name (`hook`, `mcp`,
    /// `cursor`, `claude` editor, `codex` CLI, …) that older integrations
    /// emit into `model_id` because no real model id was available. The
    /// dashboard hides these by default and reveals them behind a "Show
    /// source attribution" toggle for diagnostics.
    Source,
}

/// A model curated by ContextStream.
#[derive(Debug, Clone, Copy)]
pub struct KnownModel {
    /// Canonical, dashboard-stable identifier (e.g. `claude-opus-4.7-thinking-high`).
    pub canonical_id: &'static str,
    /// Human-readable label.
    pub label: &'static str,
    pub provider: Provider,
    /// Coarse grouping inside the provider (e.g. `claude-opus-4.7`, `gpt-5`).
    pub family: &'static str,
    /// Optional semantic version when distinguishable.
    pub version: Option<&'static str>,
    pub visibility: Visibility,
}

/// Editors (hosts) we have first-class extractor support for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KnownEditor {
    ClaudeCode,
    Cursor,
    Windsurf,
    Copilot,
    Cline,
    Roo,
    Kilo,
    Codex,
    Aider,
    Antigravity,
    OpenCode,
    ChatGPTGateway,
    OpenAIResponses,
    ContextStreamCli,
    ContextCode,
    Other,
}

impl KnownEditor {
    pub const fn harness_id(self) -> Option<HarnessId> {
        match self {
            Self::ClaudeCode => Some(HarnessId::ClaudeCode),
            Self::Cursor => Some(HarnessId::Cursor),
            Self::Windsurf => Some(HarnessId::Windsurf),
            Self::Copilot => Some(HarnessId::Copilot),
            Self::Cline => Some(HarnessId::Cline),
            Self::Roo => Some(HarnessId::RooCode),
            Self::Kilo => Some(HarnessId::KiloCode),
            Self::Codex => Some(HarnessId::Codex),
            Self::Aider => Some(HarnessId::Aider),
            Self::Antigravity => Some(HarnessId::Antigravity),
            Self::OpenCode => Some(HarnessId::OpenCode),
            Self::ChatGPTGateway => Some(HarnessId::ChatGptGateway),
            Self::OpenAIResponses => Some(HarnessId::OpenAiResponses),
            Self::ContextStreamCli => Some(HarnessId::ContextStreamCli),
            Self::ContextCode => Some(HarnessId::ContextCode),
            Self::Other => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        self.harness_id().map(HarnessId::as_str).unwrap_or("other")
    }
}

impl From<HarnessId> for KnownEditor {
    fn from(value: HarnessId) -> Self {
        match value {
            HarnessId::ClaudeCode => Self::ClaudeCode,
            HarnessId::Cursor => Self::Cursor,
            HarnessId::Windsurf => Self::Windsurf,
            HarnessId::Copilot => Self::Copilot,
            HarnessId::Cline => Self::Cline,
            HarnessId::KiloCode => Self::Kilo,
            HarnessId::RooCode => Self::Roo,
            HarnessId::Codex => Self::Codex,
            HarnessId::Aider => Self::Aider,
            HarnessId::Antigravity => Self::Antigravity,
            HarnessId::OpenCode => Self::OpenCode,
            HarnessId::ChatGptGateway => Self::ChatGPTGateway,
            HarnessId::OpenAiResponses => Self::OpenAIResponses,
            HarnessId::ContextStreamCli => Self::ContextStreamCli,
            HarnessId::ContextCode => Self::ContextCode,
        }
    }
}

macro_rules! public_model {
    ($id:literal, $label:literal, $provider:expr, $family:literal, $version:literal) => {
        KnownModel {
            canonical_id: $id,
            label: $label,
            provider: $provider,
            family: $family,
            version: Some($version),
            visibility: Visibility::Public,
        }
    };
}

const MODELS: &[KnownModel] = &[
    public_model!(
        "gpt-5.6-sol-none",
        "GPT-5.6 Sol (none)",
        Provider::OpenAI,
        "gpt-5.6-sol",
        "5.6"
    ),
    public_model!(
        "gpt-5.6-sol-low",
        "GPT-5.6 Sol (low)",
        Provider::OpenAI,
        "gpt-5.6-sol",
        "5.6"
    ),
    public_model!(
        "gpt-5.6-sol-medium",
        "GPT-5.6 Sol (medium)",
        Provider::OpenAI,
        "gpt-5.6-sol",
        "5.6"
    ),
    public_model!(
        "gpt-5.6-sol-high",
        "GPT-5.6 Sol (high)",
        Provider::OpenAI,
        "gpt-5.6-sol",
        "5.6"
    ),
    public_model!(
        "gpt-5.6-sol-xhigh",
        "GPT-5.6 Sol (xhigh)",
        Provider::OpenAI,
        "gpt-5.6-sol",
        "5.6"
    ),
    public_model!(
        "gpt-5.6-sol-max",
        "GPT-5.6 Sol (max)",
        Provider::OpenAI,
        "gpt-5.6-sol",
        "5.6"
    ),
    public_model!(
        "gpt-5.6-terra-none",
        "GPT-5.6 Terra (none)",
        Provider::OpenAI,
        "gpt-5.6-terra",
        "5.6"
    ),
    public_model!(
        "gpt-5.6-terra-low",
        "GPT-5.6 Terra (low)",
        Provider::OpenAI,
        "gpt-5.6-terra",
        "5.6"
    ),
    public_model!(
        "gpt-5.6-terra-medium",
        "GPT-5.6 Terra (medium)",
        Provider::OpenAI,
        "gpt-5.6-terra",
        "5.6"
    ),
    public_model!(
        "gpt-5.6-terra-high",
        "GPT-5.6 Terra (high)",
        Provider::OpenAI,
        "gpt-5.6-terra",
        "5.6"
    ),
    public_model!(
        "gpt-5.6-terra-xhigh",
        "GPT-5.6 Terra (xhigh)",
        Provider::OpenAI,
        "gpt-5.6-terra",
        "5.6"
    ),
    public_model!(
        "gpt-5.6-terra-max",
        "GPT-5.6 Terra (max)",
        Provider::OpenAI,
        "gpt-5.6-terra",
        "5.6"
    ),
    public_model!(
        "gpt-5.6-luna-none",
        "GPT-5.6 Luna (none)",
        Provider::OpenAI,
        "gpt-5.6-luna",
        "5.6"
    ),
    public_model!(
        "gpt-5.6-luna-low",
        "GPT-5.6 Luna (low)",
        Provider::OpenAI,
        "gpt-5.6-luna",
        "5.6"
    ),
    public_model!(
        "gpt-5.6-luna-medium",
        "GPT-5.6 Luna (medium)",
        Provider::OpenAI,
        "gpt-5.6-luna",
        "5.6"
    ),
    public_model!(
        "gpt-5.6-luna-high",
        "GPT-5.6 Luna (high)",
        Provider::OpenAI,
        "gpt-5.6-luna",
        "5.6"
    ),
    public_model!(
        "gpt-5.6-luna-xhigh",
        "GPT-5.6 Luna (xhigh)",
        Provider::OpenAI,
        "gpt-5.6-luna",
        "5.6"
    ),
    public_model!(
        "gpt-5.6-luna-max",
        "GPT-5.6 Luna (max)",
        Provider::OpenAI,
        "gpt-5.6-luna",
        "5.6"
    ),
    public_model!(
        "claude-fable-5-thinking-low",
        "Claude Fable 5 (low)",
        Provider::Anthropic,
        "claude-fable-5",
        "5"
    ),
    public_model!(
        "claude-fable-5-thinking-medium",
        "Claude Fable 5 (medium)",
        Provider::Anthropic,
        "claude-fable-5",
        "5"
    ),
    public_model!(
        "claude-fable-5-thinking-high",
        "Claude Fable 5 (high)",
        Provider::Anthropic,
        "claude-fable-5",
        "5"
    ),
    public_model!(
        "claude-fable-5-thinking-xhigh",
        "Claude Fable 5 (xhigh)",
        Provider::Anthropic,
        "claude-fable-5",
        "5"
    ),
    public_model!(
        "claude-fable-5-thinking-max",
        "Claude Fable 5 (max)",
        Provider::Anthropic,
        "claude-fable-5",
        "5"
    ),
    // ---------------- Anthropic Claude family ----------------
    KnownModel {
        canonical_id: "claude-opus-4.7-thinking-low",
        label: "Claude Opus 4.7 Thinking (low)",
        provider: Provider::Anthropic,
        family: "claude-opus-4.7",
        version: Some("4.7"),
        visibility: Visibility::Public,
    },
    KnownModel {
        canonical_id: "claude-opus-4.7-thinking-medium",
        label: "Claude Opus 4.7 Thinking (medium)",
        provider: Provider::Anthropic,
        family: "claude-opus-4.7",
        version: Some("4.7"),
        visibility: Visibility::Public,
    },
    KnownModel {
        canonical_id: "claude-opus-4.7-thinking-high",
        label: "Claude Opus 4.7 Thinking (high)",
        provider: Provider::Anthropic,
        family: "claude-opus-4.7",
        version: Some("4.7"),
        visibility: Visibility::Public,
    },
    // ---------------- Anthropic Claude Opus 4.8 (effort ladder) ----------------
    // Opus 4.8 replaces the manual thinking budget with the `effort` parameter
    // (low | medium | high | xhigh | max), default `high`. Claude Code writes the
    // bare id `claude-opus-4-8` on the wire; the bare/dotted aliases below map it
    // to the `high` variant to match 4.8's real default effort.
    KnownModel {
        canonical_id: "claude-opus-4.8-thinking-low",
        label: "Claude Opus 4.8 Thinking (low)",
        provider: Provider::Anthropic,
        family: "claude-opus-4.8",
        version: Some("4.8"),
        visibility: Visibility::Public,
    },
    KnownModel {
        canonical_id: "claude-opus-4.8-thinking-medium",
        label: "Claude Opus 4.8 Thinking (medium)",
        provider: Provider::Anthropic,
        family: "claude-opus-4.8",
        version: Some("4.8"),
        visibility: Visibility::Public,
    },
    KnownModel {
        canonical_id: "claude-opus-4.8-thinking-high",
        label: "Claude Opus 4.8 Thinking (high)",
        provider: Provider::Anthropic,
        family: "claude-opus-4.8",
        version: Some("4.8"),
        visibility: Visibility::Public,
    },
    KnownModel {
        canonical_id: "claude-opus-4.8-thinking-xhigh",
        label: "Claude Opus 4.8 Thinking (xhigh)",
        provider: Provider::Anthropic,
        family: "claude-opus-4.8",
        version: Some("4.8"),
        visibility: Visibility::Public,
    },
    KnownModel {
        canonical_id: "claude-opus-4.8-thinking-max",
        label: "Claude Opus 4.8 Thinking (max)",
        provider: Provider::Anthropic,
        family: "claude-opus-4.8",
        version: Some("4.8"),
        visibility: Visibility::Public,
    },
    KnownModel {
        canonical_id: "claude-opus-4.5",
        label: "Claude Opus 4.5",
        provider: Provider::Anthropic,
        family: "claude-opus-4.5",
        version: Some("4.5"),
        visibility: Visibility::Public,
    },
    KnownModel {
        canonical_id: "claude-sonnet-4.5",
        label: "Claude Sonnet 4.5",
        provider: Provider::Anthropic,
        family: "claude-sonnet-4.5",
        version: Some("4.5"),
        visibility: Visibility::Public,
    },
    KnownModel {
        canonical_id: "claude-haiku-4.5",
        label: "Claude Haiku 4.5",
        provider: Provider::Anthropic,
        family: "claude-haiku-4.5",
        version: Some("4.5"),
        visibility: Visibility::Public,
    },
    // ---------------- OpenAI GPT family ----------------
    KnownModel {
        canonical_id: "gpt-5",
        label: "GPT-5",
        provider: Provider::OpenAI,
        family: "gpt-5",
        version: Some("5"),
        visibility: Visibility::Public,
    },
    KnownModel {
        canonical_id: "gpt-5-codex-medium",
        label: "GPT-5 Codex (medium)",
        provider: Provider::OpenAI,
        family: "gpt-5-codex",
        version: Some("5"),
        visibility: Visibility::Public,
    },
    KnownModel {
        canonical_id: "gpt-5-codex-high",
        label: "GPT-5 Codex (high)",
        provider: Provider::OpenAI,
        family: "gpt-5-codex",
        version: Some("5"),
        visibility: Visibility::Public,
    },
    KnownModel {
        canonical_id: "gpt-5.4-medium",
        label: "GPT-5.4 (medium)",
        provider: Provider::OpenAI,
        family: "gpt-5.4",
        version: Some("5.4"),
        visibility: Visibility::Public,
    },
    KnownModel {
        canonical_id: "gpt-5.4-high",
        label: "GPT-5.4 (high)",
        provider: Provider::OpenAI,
        family: "gpt-5.4",
        version: Some("5.4"),
        visibility: Visibility::Public,
    },
    // ---------------- Google Gemini ----------------
    KnownModel {
        canonical_id: "gemini-2.5-pro",
        label: "Gemini 2.5 Pro",
        provider: Provider::Google,
        family: "gemini-2.5",
        version: Some("2.5"),
        visibility: Visibility::Public,
    },
    KnownModel {
        canonical_id: "gemini-2.5-flash",
        label: "Gemini 2.5 Flash",
        provider: Provider::Google,
        family: "gemini-2.5",
        version: Some("2.5"),
        visibility: Visibility::Public,
    },
    // ---------------- xAI Grok ----------------
    KnownModel {
        canonical_id: "grok-4",
        label: "Grok 4",
        provider: Provider::XAI,
        family: "grok-4",
        version: Some("4"),
        visibility: Visibility::Public,
    },
    KnownModel {
        canonical_id: "grok-4-fast",
        label: "Grok 4 Fast",
        provider: Provider::XAI,
        family: "grok-4",
        version: Some("4"),
        visibility: Visibility::Public,
    },
    // ---------------- Moonshot Kimi (internal) ----------------
    KnownModel {
        canonical_id: "kimi-k2.5",
        label: "Kimi K2.5",
        provider: Provider::Moonshot,
        family: "kimi-k2",
        version: Some("2.5"),
        visibility: Visibility::Internal,
    },
    // ---------------- ContextStream-curated models ----------------
    KnownModel {
        canonical_id: "composer-2-fast",
        label: "Composer 2 Fast",
        provider: Provider::ContextStream,
        family: "composer-2",
        version: Some("2"),
        visibility: Visibility::Public,
    },
    KnownModel {
        canonical_id: "composer-2",
        label: "Composer 2",
        provider: Provider::ContextStream,
        family: "composer-2",
        version: Some("2"),
        visibility: Visibility::Public,
    },
    KnownModel {
        canonical_id: "streampilot",
        label: "StreamPilot",
        provider: Provider::ContextStream,
        family: "streampilot",
        version: None,
        visibility: Visibility::Internal,
    },
    // ----------------------------------------------------------------------
    // Source / editor / CLI / harness attributions.
    //
    // These are NOT models. They are the emission source — the editor, CLI,
    // hook, MCP runtime, or test harness — that older integrations write
    // into `model_id` when no real model id is available. Tagged
    // `Visibility::Source` so the dashboard hides them from the Models
    // leaderboard by default. The "Show source attribution" toggle reveals
    // them so operators can pinpoint which integration is still leaking
    // a source name as a model id.
    // ----------------------------------------------------------------------
    KnownModel {
        canonical_id: "claude",
        label: "Claude Code (editor)",
        provider: Provider::Anthropic,
        family: "source",
        version: None,
        visibility: Visibility::Source,
    },
    KnownModel {
        canonical_id: "cursor",
        label: "Cursor (editor)",
        provider: Provider::Other,
        family: "source",
        version: None,
        visibility: Visibility::Source,
    },
    KnownModel {
        canonical_id: "codex",
        label: "Codex (CLI)",
        provider: Provider::OpenAI,
        family: "source",
        version: None,
        visibility: Visibility::Source,
    },
    KnownModel {
        canonical_id: "codex-cli",
        label: "Codex CLI",
        provider: Provider::OpenAI,
        family: "source",
        version: None,
        visibility: Visibility::Source,
    },
    KnownModel {
        canonical_id: "contextstream",
        label: "ContextStream (CLI)",
        provider: Provider::ContextStream,
        family: "source",
        version: None,
        visibility: Visibility::Source,
    },
    KnownModel {
        canonical_id: "streampilot-cli",
        label: "StreamPilot CLI",
        provider: Provider::ContextStream,
        family: "source",
        version: None,
        visibility: Visibility::Source,
    },
    KnownModel {
        canonical_id: "mcp",
        label: "MCP runtime",
        provider: Provider::ContextStream,
        family: "source",
        version: None,
        visibility: Visibility::Source,
    },
    KnownModel {
        canonical_id: "hook",
        label: "MCP hook",
        provider: Provider::ContextStream,
        family: "source",
        version: None,
        visibility: Visibility::Source,
    },
    KnownModel {
        canonical_id: "benchmark",
        label: "Benchmark suite",
        provider: Provider::Other,
        family: "source",
        version: None,
        visibility: Visibility::Source,
    },
    KnownModel {
        canonical_id: "kilo",
        label: "Kilo (editor)",
        provider: Provider::Other,
        family: "source",
        version: None,
        visibility: Visibility::Source,
    },
    KnownModel {
        canonical_id: "cline",
        label: "Cline (editor)",
        provider: Provider::Other,
        family: "source",
        version: None,
        visibility: Visibility::Source,
    },
    KnownModel {
        canonical_id: "roo",
        label: "Roo (editor)",
        provider: Provider::Other,
        family: "source",
        version: None,
        visibility: Visibility::Source,
    },
    KnownModel {
        canonical_id: "windsurf",
        label: "Windsurf (editor)",
        provider: Provider::Other,
        family: "source",
        version: None,
        visibility: Visibility::Source,
    },
];

/// Static alias map (raw -> canonical_id). Built once.
struct AliasIndex {
    by_alias: HashMap<String, &'static KnownModel>,
}

impl AliasIndex {
    fn new() -> Self {
        let mut by_alias: HashMap<String, &'static KnownModel> = HashMap::new();

        for model in MODELS.iter() {
            // Always register the canonical id (and its normalized form).
            insert_alias(&mut by_alias, model, model.canonical_id);

            // Register hand-curated aliases per model below.
        }

        // Hand-curated aliases for variants we know editors send. Keep this
        // list explicit — fuzzy matching causes the exact noise we're trying
        // to fix.
        let aliases: &[(&str, &str)] = &[
            ("gpt-5.6", "gpt-5.6-sol-medium"),
            ("gpt5.6", "gpt-5.6-sol-medium"),
            ("gpt-5.5", "gpt-5.6-sol-medium"),
            ("gpt-5-5", "gpt-5.6-sol-medium"),
            ("gpt5.5", "gpt-5.6-sol-medium"),
            ("openai/gpt-5.5", "gpt-5.6-sol-medium"),
            ("openai/gpt-5.5-medium", "gpt-5.6-sol-medium"),
            ("openai/gpt-5.5-high", "gpt-5.6-sol-high"),
            ("gpt-5-5-medium", "gpt-5.6-sol-medium"),
            ("gpt-5-5-high", "gpt-5.6-sol-high"),
            ("gpt5.5-medium", "gpt-5.6-sol-medium"),
            ("gpt5.5-high", "gpt-5.6-sol-high"),
            ("gpt-5.6-sol", "gpt-5.6-sol-medium"),
            ("openai/gpt-5.6-sol", "gpt-5.6-sol-medium"),
            ("gpt-5.6-terra", "gpt-5.6-terra-medium"),
            ("openai/gpt-5.6-terra", "gpt-5.6-terra-medium"),
            ("gpt-5.6-luna", "gpt-5.6-luna-medium"),
            ("openai/gpt-5.6-luna", "gpt-5.6-luna-medium"),
            ("claude-fable-5", "claude-fable-5-thinking-high"),
            ("claude-fable-5.0", "claude-fable-5-thinking-high"),
            ("fable-5", "claude-fable-5-thinking-high"),
            ("anthropic/claude-fable-5", "claude-fable-5-thinking-high"),
            ("claude-fable-5-low", "claude-fable-5-thinking-low"),
            ("claude-fable-5-medium", "claude-fable-5-thinking-medium"),
            ("claude-fable-5-high", "claude-fable-5-thinking-high"),
            ("claude-fable-5-xhigh", "claude-fable-5-thinking-xhigh"),
            ("claude-fable-5-max", "claude-fable-5-thinking-max"),
            // Anthropic — vendor prefixes, dashed thinking variants, dotted forms
            // Opus 4.8 — effort ladder; bare/dotted id defaults to `high`.
            (
                "claude-opus-4-8-thinking-low",
                "claude-opus-4.8-thinking-low",
            ),
            (
                "claude-opus-4-8-thinking-medium",
                "claude-opus-4.8-thinking-medium",
            ),
            (
                "claude-opus-4-8-thinking-high",
                "claude-opus-4.8-thinking-high",
            ),
            (
                "claude-opus-4-8-thinking-xhigh",
                "claude-opus-4.8-thinking-xhigh",
            ),
            (
                "claude-opus-4-8-thinking-max",
                "claude-opus-4.8-thinking-max",
            ),
            ("claude-opus-4-8", "claude-opus-4.8-thinking-high"),
            ("claude-opus-4.8", "claude-opus-4.8-thinking-high"),
            ("anthropic/claude-opus-4-8", "claude-opus-4.8-thinking-high"),
            ("anthropic/claude-opus-4.8", "claude-opus-4.8-thinking-high"),
            (
                "anthropic/claude-opus-4-8-thinking-xhigh",
                "claude-opus-4.8-thinking-xhigh",
            ),
            (
                "anthropic/claude-opus-4-8-thinking-max",
                "claude-opus-4.8-thinking-max",
            ),
            // Opus 4.7
            (
                "claude-opus-4-7-thinking-low",
                "claude-opus-4.7-thinking-low",
            ),
            (
                "claude-opus-4-7-thinking-medium",
                "claude-opus-4.7-thinking-medium",
            ),
            (
                "claude-opus-4-7-thinking-high",
                "claude-opus-4.7-thinking-high",
            ),
            ("claude-opus-4-7", "claude-opus-4.7-thinking-medium"),
            ("claude-opus-4_7", "claude-opus-4.7-thinking-medium"),
            ("claude-opus-4.7", "claude-opus-4.7-thinking-medium"),
            (
                "anthropic/claude-opus-4.7",
                "claude-opus-4.7-thinking-medium",
            ),
            (
                "anthropic/claude-opus-4-7-thinking-high",
                "claude-opus-4.7-thinking-high",
            ),
            ("claude-opus-4-5", "claude-opus-4.5"),
            ("claude-opus-4_5", "claude-opus-4.5"),
            ("anthropic/claude-opus-4.5", "claude-opus-4.5"),
            ("claude-sonnet-4-5", "claude-sonnet-4.5"),
            ("claude-sonnet-4_5", "claude-sonnet-4.5"),
            ("anthropic/claude-sonnet-4.5", "claude-sonnet-4.5"),
            ("claude-haiku-4-5", "claude-haiku-4.5"),
            ("claude-haiku-4_5", "claude-haiku-4.5"),
            ("anthropic/claude-haiku-4.5", "claude-haiku-4.5"),
            // OpenAI — vendor prefixes, suffix variants
            ("openai/gpt-5", "gpt-5"),
            ("gpt5", "gpt-5"),
            ("gpt-5-codex", "gpt-5-codex-medium"),
            ("openai/gpt-5-codex", "gpt-5-codex-medium"),
            ("openai/gpt-5-codex-high", "gpt-5-codex-high"),
            ("openai/gpt-5-codex-medium", "gpt-5-codex-medium"),
            ("gpt-5-4", "gpt-5.4-medium"),
            ("gpt-5_4", "gpt-5.4-medium"),
            ("gpt-5.4", "gpt-5.4-medium"),
            ("openai/gpt-5.4", "gpt-5.4-medium"),
            ("openai/gpt-5.4-medium", "gpt-5.4-medium"),
            ("openai/gpt-5.4-high", "gpt-5.4-high"),
            // Google
            ("google/gemini-2.5-pro", "gemini-2.5-pro"),
            ("gemini-2-5-pro", "gemini-2.5-pro"),
            ("gemini-2_5-pro", "gemini-2.5-pro"),
            ("google/gemini-2.5-flash", "gemini-2.5-flash"),
            ("gemini-2-5-flash", "gemini-2.5-flash"),
            // xAI
            ("xai/grok-4", "grok-4"),
            ("xai/grok-4-fast", "grok-4-fast"),
            ("grok4", "grok-4"),
            // Moonshot Kimi
            ("moonshotai.kimi-k2.5", "kimi-k2.5"),
            ("moonshot/kimi-k2.5", "kimi-k2.5"),
            ("kimi-k2-5", "kimi-k2.5"),
            ("kimi-k2_5", "kimi-k2.5"),
            // ContextStream-curated
            ("contextstream/composer-2-fast", "composer-2-fast"),
            ("contextstream/composer-2", "composer-2"),
            ("contextstream/streampilot", "streampilot"),
            // Host / editor / CLI aliases.
            ("claude-code", "claude"),
            ("claude_code", "claude"),
            ("anthropic/claude", "claude"),
            ("cursor-ide", "cursor"),
            ("cursor.sh", "cursor"),
            ("codex_cli", "codex-cli"),
            ("openai/codex", "codex"),
            ("openai/codex-cli", "codex-cli"),
            ("streampilot_cli", "streampilot-cli"),
            ("stream-pilot-cli", "streampilot-cli"),
            ("contextstream-cli", "contextstream"),
            ("contextstream-mcp", "mcp"),
            ("contextstream/mcp", "mcp"),
            ("contextstream/hook", "hook"),
            ("contextstream/contextstream", "contextstream"),
            // Sibling editor aliases.
            ("kilo-code", "kilo"),
            ("kilocode", "kilo"),
            ("cline-bot", "cline"),
            ("roo-cline", "roo"),
            ("roo-code", "roo"),
            ("windsurf-cascade", "windsurf"),
            ("cascade", "windsurf"),
        ];

        for (alias, canonical) in aliases {
            if let Some(model) = MODELS.iter().find(|m| m.canonical_id == *canonical) {
                insert_alias(&mut by_alias, model, alias);
            }
        }

        Self { by_alias }
    }
}

fn insert_alias(
    by_alias: &mut HashMap<String, &'static KnownModel>,
    model: &'static KnownModel,
    raw: &str,
) {
    let key = normalize_alias(raw);
    if !key.is_empty() {
        by_alias.entry(key).or_insert(model);
    }
}

fn registry() -> &'static AliasIndex {
    static INDEX: OnceLock<AliasIndex> = OnceLock::new();
    INDEX.get_or_init(AliasIndex::new)
}

/// Normalize a raw alias for lookup: lowercase, strip whitespace, keep `.` `-` `/` `:`.
fn normalize_alias(raw: &str) -> String {
    let mut trimmed = raw.trim().to_ascii_lowercase();
    if trimmed.is_empty() {
        return String::new();
    }

    // Strip a trailing context-window display suffix like `[1m]` or `[200k]`.
    // Claude Code's transcript wire id is clean (`claude-opus-4-8`), but some
    // hosts/display surfaces append the bracketed window variant. Additive:
    // this only changes ids that would otherwise fail to match.
    if let Some(idx) = trimmed.find('[') {
        trimmed.truncate(idx);
        trimmed = trimmed.trim_end().to_string();
        if trimmed.is_empty() {
            return String::new();
        }
    }

    // Replace underscores with dashes; collapse repeated separators. Keep dots,
    // dashes, slashes, and colons because those are meaningful in model ids
    // (e.g. `gpt-5.4-medium`, `anthropic/claude-opus-4.5`,
    // `openai:gpt-5-codex-high`).
    let mut out = String::with_capacity(trimmed.len());
    let mut prev_dash = false;
    for c in trimmed.chars() {
        let next = match c {
            '_' | ' ' => '-',
            ':' => '/',
            other => other,
        };
        if next == '-' {
            if prev_dash {
                continue;
            }
            prev_dash = true;
        } else {
            prev_dash = false;
        }
        out.push(next);
    }

    // Normalize duplicate slashes to single.
    while out.contains("//") {
        out = out.replace("//", "/");
    }

    out
}

/// Strict model lookup: returns `Some(&KnownModel)` only when `raw` matches a
/// curated alias. Unknown values return `None` — callers must NOT invent.
pub fn match_model(raw: &str) -> Option<&'static KnownModel> {
    let mut key = normalize_alias(raw);
    if key.is_empty() {
        return None;
    }
    // Keep the retired canonical records available for historical labels, but
    // migrate any newly observed GPT-5.5 effort id to the matching Sol effort.
    key = match key.as_str() {
        "gpt-5.5-medium" => "gpt-5.6-sol-medium".to_string(),
        "gpt-5.5-high" => "gpt-5.6-sol-high".to_string(),
        _ => key,
    };
    registry().by_alias.get(&key).copied()
}

/// Identify the host editor from a client/host hint. Returns `None` when the
/// hint doesn't map to a known editor — callers should record `client_name`
/// separately rather than guessing.
pub fn match_editor(client_hint: Option<&str>, hook_event: Option<&str>) -> Option<KnownEditor> {
    if let Some(raw) = client_hint {
        if let Some(harness) = HarnessId::from_client_hint(raw) {
            return Some(harness.into());
        }
    }

    if let Some(event) = hook_event {
        let lower = event.trim().to_ascii_lowercase();
        // Windsurf uses `pre_mcp_tool_use` / `pre_user_prompt` event names.
        if lower == "pre_mcp_tool_use" || lower == "pre_user_prompt" {
            return Some(KnownEditor::Windsurf);
        }
        // Claude Code hook events use TitleCase like `SessionStart`.
        if matches!(
            event,
            "SessionStart"
                | "InstructionsLoaded"
                | "UserPromptSubmit"
                | "PreToolUse"
                | "PostToolUse"
        ) {
            return Some(KnownEditor::ClaudeCode);
        }
    }

    None
}

/// Lookup helper returning canonical id only. Useful for callers that don't
/// need the full record.
pub fn canonical_id(raw: &str) -> Option<&'static str> {
    match_model(raw).map(|m| m.canonical_id)
}

/// Resolve the tokenizer encoding for a curated model.
///
/// This is intentionally strict: only registry-recognized models whose
/// provider is OpenAI select `o200k_base`. Editor/client identifiers, unknown
/// model strings, and models from every other provider return `None` so callers
/// retain proxy accounting rather than guessing an incompatible tokenizer.
pub fn tokenizer_encoding(raw: &str) -> Option<&'static str> {
    let model = match_model(raw)?;
    (model.provider == Provider::OpenAI && model.visibility != Visibility::Source)
        .then_some(OPENAI_TOKENIZER_ENCODING)
}

/// Approximate usable input-context window (in tokens) for a recognized model,
/// when we special-case it. Used to size context-pressure thresholds so
/// large-window models (Opus 4.8 = 1M) are not warned about imminent
/// compaction while the window is barely full.
///
/// Returns `None` for any model whose window we do not explicitly track —
/// callers MUST fall back to their existing conservative default, so older and
/// unrecognized models keep their current behavior unchanged.
pub fn context_window(raw: &str) -> Option<u32> {
    let model = match_model(raw)?;
    match model.family {
        // Opus 4.8 ships with the 1M-token context window by default.
        "claude-opus-4.8" | "claude-fable-5" | "gpt-5.6-sol" | "gpt-5.6-terra" | "gpt-5.6-luna" => {
            Some(1_000_000)
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_canonical_ids() {
        assert_eq!(
            match_model("claude-opus-4.7-thinking-high").map(|m| m.canonical_id),
            Some("claude-opus-4.7-thinking-high")
        );
        assert_eq!(
            match_model("gpt-5-codex-high").map(|m| m.canonical_id),
            Some("gpt-5-codex-high")
        );
    }

    #[test]
    fn matches_dash_to_dot_aliases() {
        assert_eq!(
            match_model("claude-opus-4-7-thinking-high").map(|m| m.canonical_id),
            Some("claude-opus-4.7-thinking-high")
        );
        assert_eq!(
            match_model("gpt-5_4").map(|m| m.canonical_id),
            Some("gpt-5.4-medium")
        );
    }

    #[test]
    fn matches_vendor_prefix_aliases() {
        assert_eq!(
            match_model("anthropic/claude-sonnet-4.5").map(|m| m.canonical_id),
            Some("claude-sonnet-4.5")
        );
        assert_eq!(
            match_model("openai/gpt-5-codex-high").map(|m| m.canonical_id),
            Some("gpt-5-codex-high")
        );
    }

    #[test]
    fn unknown_returns_none() {
        assert!(match_model("not-a-real-model").is_none());
        assert!(match_model("").is_none());
        assert!(match_model("  ").is_none());
        // Truly unknown strings still return None.
        assert!(match_model("chatgpt").is_none());
        assert!(match_model("some-random-model-id").is_none());
    }

    #[test]
    fn source_attributions_match_with_literal_canonical_ids_and_source_visibility() {
        // Editor / CLI / hook / harness names ARE recognized so legacy
        // traffic (where `model_id` was being populated with the source
        // name) can be attributed properly on the dashboard. They are NOT
        // rewritten to a real LLM model id and they are tagged
        // `Visibility::Source` so the Models leaderboard hides them by
        // default.
        let cases = [
            ("cursor", "cursor"),
            ("claude-code", "claude"),
            ("contextstream-mcp", "mcp"),
            ("contextstream", "contextstream"),
            ("hook", "hook"),
            ("streampilot-cli", "streampilot-cli"),
            ("codex", "codex"),
            ("CODEX-CLI", "codex-cli"),
        ];
        for (raw, canonical) in cases {
            let m = match_model(raw).expect(canonical);
            assert_eq!(m.canonical_id, canonical);
            assert!(
                matches!(m.visibility, Visibility::Source),
                "{raw} must be classified as a source attribution"
            );
            assert_eq!(m.family, "source");
        }
    }

    #[test]
    fn internal_models_keep_id_but_marked_internal() {
        let m = match_model("kimi-k2.5").expect("kimi present");
        assert_eq!(m.canonical_id, "kimi-k2.5");
        assert!(matches!(m.visibility, Visibility::Internal));

        let m2 = match_model("moonshotai.kimi-k2.5").expect("dotted alias");
        assert_eq!(m2.canonical_id, "kimi-k2.5");
        assert!(matches!(m2.visibility, Visibility::Internal));
    }

    #[test]
    fn provider_and_family_populated() {
        let m = match_model("claude-opus-4.7-thinking-high").unwrap();
        assert_eq!(m.provider.as_str(), "anthropic");
        assert_eq!(m.family, "claude-opus-4.7");

        let g = match_model("gpt-5-codex-medium").unwrap();
        assert_eq!(g.provider.as_str(), "openai");
        assert_eq!(g.family, "gpt-5-codex");
    }

    #[test]
    fn editor_matching_is_strict() {
        assert_eq!(
            match_editor(Some("Claude-Code/1.0"), None),
            Some(KnownEditor::ClaudeCode)
        );
        assert_eq!(
            match_editor(Some("cursor"), None),
            Some(KnownEditor::Cursor)
        );
        assert_eq!(match_editor(Some("Cline"), None), Some(KnownEditor::Cline));
        assert_eq!(
            match_editor(None, Some("pre_mcp_tool_use")),
            Some(KnownEditor::Windsurf)
        );
        assert_eq!(
            match_editor(None, Some("SessionStart")),
            Some(KnownEditor::ClaudeCode)
        );
        assert_eq!(
            match_editor(None, Some("InstructionsLoaded")),
            Some(KnownEditor::ClaudeCode)
        );
        assert_eq!(match_editor(Some(""), None), None);
        assert_eq!(match_editor(Some("totally-unknown-host"), None), None);
        assert_eq!(match_editor(Some("my-cursor-proxy"), None), None);
        assert_eq!(match_editor(Some("claude-code-wrapper"), None), None);
        assert_eq!(
            match_editor(Some("GitHub-Copilot/1.2"), None),
            Some(KnownEditor::Copilot)
        );
    }

    #[test]
    fn every_canonical_harness_maps_to_a_known_editor() {
        for harness in HarnessId::ALL {
            let editor = KnownEditor::from(*harness);
            assert_eq!(editor.harness_id(), Some(*harness));
            assert_eq!(editor.as_str(), harness.as_str());
        }
        assert_eq!(KnownEditor::Other.harness_id(), None);
    }

    #[test]
    fn alias_normalization_collapses_repeats_and_separators() {
        assert_eq!(normalize_alias("  Claude-Opus-4_7   "), "claude-opus-4-7");
        assert_eq!(normalize_alias("openai:gpt-5"), "openai/gpt-5");
        assert_eq!(normalize_alias("gpt--5"), "gpt-5");
    }

    #[test]
    fn canonical_id_helper() {
        assert_eq!(
            canonical_id("anthropic/claude-opus-4.5"),
            Some("claude-opus-4.5")
        );
        assert_eq!(canonical_id("nope"), None);
    }

    #[test]
    fn tokenizer_encoding_is_openai_model_only() {
        assert_eq!(tokenizer_encoding("gpt-5-codex-high"), Some("o200k_base"));
        assert_eq!(
            tokenizer_encoding("openai/gpt-5.6-terra"),
            Some("o200k_base")
        );

        for raw in [
            "claude-opus-4-8",
            "google/gemini-2.5-pro",
            "codex",
            "codex-cli",
            "openai-responses",
            "totally-unknown-model",
        ] {
            assert_eq!(
                tokenizer_encoding(raw),
                None,
                "unexpected inference for {raw}"
            );
        }
    }

    #[test]
    fn opus_4_8_bare_id_defaults_to_high() {
        // Claude Code writes the bare wire id; it must canonicalize to the
        // `high` effort variant (4.8's real default effort), not to `unknown`.
        let m = match_model("claude-opus-4-8").expect("claude-opus-4-8 recognized");
        assert_eq!(m.canonical_id, "claude-opus-4.8-thinking-high");
        assert_eq!(m.family, "claude-opus-4.8");
        assert_eq!(m.provider.as_str(), "anthropic");
        assert!(matches!(m.visibility, Visibility::Public));
        // Dotted + vendor-prefixed bare forms collapse onto the same default.
        assert_eq!(
            canonical_id("claude-opus-4.8"),
            Some("claude-opus-4.8-thinking-high")
        );
        assert_eq!(
            canonical_id("anthropic/claude-opus-4.8"),
            Some("claude-opus-4.8-thinking-high")
        );
    }

    #[test]
    fn opus_4_8_full_effort_ladder_matches() {
        for (raw, canonical) in [
            (
                "claude-opus-4-8-thinking-low",
                "claude-opus-4.8-thinking-low",
            ),
            (
                "claude-opus-4-8-thinking-medium",
                "claude-opus-4.8-thinking-medium",
            ),
            (
                "claude-opus-4-8-thinking-high",
                "claude-opus-4.8-thinking-high",
            ),
            (
                "claude-opus-4-8-thinking-xhigh",
                "claude-opus-4.8-thinking-xhigh",
            ),
            (
                "claude-opus-4-8-thinking-max",
                "claude-opus-4.8-thinking-max",
            ),
            // Dotted canonical forms are auto-registered.
            (
                "claude-opus-4.8-thinking-xhigh",
                "claude-opus-4.8-thinking-xhigh",
            ),
        ] {
            assert_eq!(canonical_id(raw), Some(canonical), "alias {raw}");
        }
    }

    #[test]
    fn strips_bracketed_window_suffix() {
        // Display/runtime forms can append `[1m]`; the bracketed suffix must be
        // stripped before lookup so the model still canonicalizes.
        assert_eq!(
            canonical_id("claude-opus-4-8[1m]"),
            Some("claude-opus-4.8-thinking-high")
        );
        assert_eq!(
            canonical_id("claude-opus-4.7-thinking-high[1m]"),
            Some("claude-opus-4.7-thinking-high")
        );
        assert_eq!(normalize_alias("claude-opus-4-8 [1m]"), "claude-opus-4-8");
        // A bare bracket expression normalizes to empty (no false match).
        assert_eq!(normalize_alias("[1m]"), "");
    }

    #[test]
    fn context_window_known_for_frontier_large_context_models() {
        // Opus 4.8 (any effort variant) reports the 1M window.
        assert_eq!(context_window("claude-opus-4-8"), Some(1_000_000));
        assert_eq!(
            context_window("claude-opus-4.8-thinking-max"),
            Some(1_000_000)
        );
        assert_eq!(context_window("claude-fable-5"), Some(1_000_000));
        assert_eq!(context_window("gpt-5.6-sol"), Some(1_000_000));
        // Older / other models are intentionally untracked -> None so callers
        // keep their existing conservative threshold (backward compatible).
        assert_eq!(context_window("claude-opus-4.7-thinking-high"), None);
        assert_eq!(context_window("claude-sonnet-4.5"), None);
        assert_eq!(context_window("gpt-5-codex-high"), None);
        assert_eq!(context_window("not-a-real-model"), None);
    }

    #[test]
    fn gpt56_and_fable_effort_aliases_match() {
        for (raw, expected) in [
            ("gpt-5.5", "gpt-5.6-sol-medium"),
            ("gpt-5.5-medium", "gpt-5.6-sol-medium"),
            ("gpt-5.5-high", "gpt-5.6-sol-high"),
            ("openai/gpt-5.5-high", "gpt-5.6-sol-high"),
            ("gpt-5.6", "gpt-5.6-sol-medium"),
            ("openai/gpt-5.6-terra", "gpt-5.6-terra-medium"),
            ("gpt-5.6-luna-max", "gpt-5.6-luna-max"),
            ("claude-fable-5", "claude-fable-5-thinking-high"),
            ("claude-fable-5-xhigh", "claude-fable-5-thinking-xhigh"),
        ] {
            assert_eq!(canonical_id(raw), Some(expected), "{raw}");
        }
    }
}
