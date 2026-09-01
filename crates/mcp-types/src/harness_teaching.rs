//! Versioned, bounded teaching shared by every ContextStream harness surface.
//!
//! The semantic workflow in this module is deliberately independent of any
//! editor, hook, or transport. Callers may change presentation and tool-name
//! syntax, but they must not maintain their own copy of the six core
//! requirements.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::harness::{
    HarnessId, HookCapabilities, McpTransportSupport, RulesFormat, TeachingLoadEvidence,
};

/// Stable version written into rules, help responses, and dynamic guidance.
pub const HARNESS_TEACHING_VERSION: &str = "harness_teaching_v4";

/// Schema version for the structured teaching response.
pub const HARNESS_TEACHING_SCHEMA_VERSION: u16 = 3;

/// The only legacy MCP revision this server currently advertises.
pub const MCP_PROTOCOL_2024_11_05: &str = "2024-11-05";

/// Legacy MCP revisions whose initialize result supports `instructions`.
pub const MCP_PROTOCOL_2025_03_26: &str = "2025-03-26";
pub const MCP_PROTOCOL_2025_06_18: &str = "2025-06-18";

/// Breaking stateless protocol revision. It is not a legacy initialize variant.
pub const MCP_PROTOCOL_2026_07_28: &str = "2026-07-28";

/// A surface on which the canonical workflow is delivered.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum HarnessTeachingDelivery {
    StaticRules,
    LifecycleInstructions,
    HelpWorkflow,
    HookReminder,
    StatelessDiscovery,
}

/// Stable identifiers for the semantic requirements.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum HarnessTeachingStepId {
    InitializeOnce,
    GroundEveryTurn,
    SearchBeforeLocalDiscovery,
    ConsultDurableKnowledge,
    PersistDurableWork,
    CreateCanonicalHandoff,
}

impl HarnessTeachingStepId {
    pub const ALL: &'static [Self] = &[
        Self::InitializeOnce,
        Self::GroundEveryTurn,
        Self::SearchBeforeLocalDiscovery,
        Self::ConsultDurableKnowledge,
        Self::PersistDurableWork,
        Self::CreateCanonicalHandoff,
    ];
}

/// One requirement in the canonical workflow.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct HarnessTeachingStep {
    pub id: HarnessTeachingStepId,
    pub title: String,
    pub requirement: String,
    pub canonical_calls: Vec<String>,
}

/// Conservative capability snapshot accompanying structured help output.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct HarnessTeachingCapabilities {
    pub installable: bool,
    pub static_rules: bool,
    pub rules_auto_loaded: bool,
    pub rules_format: RulesFormat,
    pub mcp_tools: bool,
    pub mcp_transport: McpTransportSupport,
    pub lifecycle_hooks: bool,
    pub hooks: HookCapabilities,
    pub hard_first_call_enforcement: bool,
    pub dynamic_guidance: bool,
    pub teaching_load_evidence: TeachingLoadEvidence,
}

impl HarnessTeachingCapabilities {
    fn for_harness(harness_id: Option<HarnessId>) -> Self {
        let Some(profile) = harness_id.map(HarnessId::profile) else {
            // Unknown clients still receive safe generic guidance, but no
            // delivery or enforcement capability is claimed for them.
            return Self {
                installable: false,
                static_rules: false,
                rules_auto_loaded: false,
                rules_format: RulesFormat::None,
                mcp_tools: false,
                mcp_transport: McpTransportSupport::None,
                lifecycle_hooks: false,
                hooks: HookCapabilities::none(),
                hard_first_call_enforcement: false,
                dynamic_guidance: false,
                teaching_load_evidence: TeachingLoadEvidence::NotObservable,
            };
        };

        Self {
            installable: profile.installable,
            static_rules: profile.rules_format != RulesFormat::None,
            rules_auto_loaded: profile.rules_auto_loaded,
            rules_format: profile.rules_format,
            mcp_tools: profile.mcp_support != McpTransportSupport::None,
            mcp_transport: profile.mcp_support,
            lifecycle_hooks: profile.hooks.any(),
            hooks: profile.hooks,
            hard_first_call_enforcement: profile.hard_first_call_enforcement,
            dynamic_guidance: profile.dynamic_guidance,
            teaching_load_evidence: profile.teaching_load_evidence,
        }
    }
}

/// Character, line, and dependency-free token-proxy budget for a rendered surface.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct HarnessTeachingBudget {
    pub max_chars: usize,
    pub max_lines: usize,
    pub max_estimated_tokens: usize,
    pub chars: usize,
    pub lines: usize,
    pub estimated_tokens: usize,
    pub within_budget: bool,
}

impl HarnessTeachingBudget {
    fn limits(delivery: HarnessTeachingDelivery) -> (usize, usize, usize) {
        match delivery {
            HarnessTeachingDelivery::StaticRules => (3_000, 16, 750),
            HarnessTeachingDelivery::LifecycleInstructions => (3_000, 12, 750),
            HarnessTeachingDelivery::HelpWorkflow => (3_000, 16, 750),
            HarnessTeachingDelivery::HookReminder => (1_600, 10, 400),
            HarnessTeachingDelivery::StatelessDiscovery => (3_000, 12, 750),
        }
    }

    fn measure(delivery: HarnessTeachingDelivery, rendered: &str) -> Self {
        let (max_chars, max_lines, max_estimated_tokens) = Self::limits(delivery);
        let chars = rendered.chars().count();
        let lines = rendered.lines().count();
        // Keep the same dependency-free byte/character proxy used by the wire
        // budget fallback. Tests below also enforce the exact o200k_base count.
        let estimated_tokens = chars.div_ceil(4);
        Self {
            max_chars,
            max_lines,
            max_estimated_tokens,
            chars,
            lines,
            estimated_tokens,
            within_budget: chars <= max_chars
                && lines <= max_lines
                && estimated_tokens <= max_estimated_tokens,
        }
    }
}

/// Structured, versioned workflow returned by help and reusable by adapters.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct HarnessTeachingContract {
    pub schema_version: u16,
    pub teaching_version: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub harness_id: Option<HarnessId>,
    pub harness_name: String,
    pub recognized_harness: bool,
    pub delivery: HarnessTeachingDelivery,
    pub capabilities: HarnessTeachingCapabilities,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub delivery_notice: Option<String>,
    pub steps: Vec<HarnessTeachingStep>,
    pub rendered_guidance: String,
    pub budget: HarnessTeachingBudget,
}

/// Required pieces of the 2026 stateless adapter.
///
/// The adapter must not advertise the revision unless every requirement is
/// implemented. This keeps a partial `server/discover` experiment from
/// masquerading as protocol conformance.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct StatelessMcpConformance {
    pub server_discover: bool,
    pub no_initialize_or_protocol_sessions: bool,
    pub removed_legacy_core_methods: bool,
    pub per_request_protocol_version: bool,
    pub per_request_client_capabilities: bool,
    pub response_server_identity: bool,
    pub result_type: bool,
    pub cacheable_results: bool,
    pub http_header_routing: bool,
}

impl StatelessMcpConformance {
    pub const fn fully_conformant(self) -> bool {
        self.server_discover
            && self.no_initialize_or_protocol_sessions
            && self.removed_legacy_core_methods
            && self.per_request_protocol_version
            && self.per_request_client_capabilities
            && self.response_server_identity
            && self.result_type
            && self.cacheable_results
            && self.http_header_routing
    }
}

fn tool_prefix(harness_id: Option<HarnessId>) -> &'static str {
    if harness_id == Some(HarnessId::ClaudeCode) {
        "mcp__contextstream__"
    } else {
        ""
    }
}

fn tool_call(prefix: &str, tool: &str, arguments: &str) -> String {
    format!("{prefix}{tool}({arguments})")
}

fn canonical_steps(harness_id: Option<HarnessId>) -> Vec<HarnessTeachingStep> {
    let prefix = tool_prefix(harness_id);
    let init = tool_call(
        prefix,
        "init",
        "folder_path=\"<project_path>\", workspace_id=\"<id>\", project_id=\"<id>\"",
    );
    let context = tool_call(
        prefix,
        "context",
        "user_message=\"...\", session_id=\"<id>\", workspace_id=\"<current_workspace_id>\"",
    );
    let ground = tool_call(
        prefix,
        "session",
        "action=\"ground\", user_message=\"...\", session_id=\"<id>\", workspace_id=\"<current_workspace_id>\"",
    );
    let search = tool_call(prefix, "search", "mode=\"auto\", query=\"...\"");
    let memory_search = tool_call(prefix, "memory", "action=\"search\", query=\"...\"");
    let decisions = tool_call(prefix, "memory", "action=\"decisions\", query=\"...\"");
    let lessons = tool_call(prefix, "session", "action=\"get_lessons\", query=\"...\"");
    let plan = tool_call(
        prefix,
        "session",
        "action=\"capture_plan\", title=\"...\", steps=[...], create_tasks=true",
    );
    let lesson = tool_call(
        prefix,
        "session",
        "action=\"capture_lesson\", title=\"...\", trigger=\"...\", impact=\"...\", prevention=\"...\"",
    );
    let decision = tool_call(
        prefix,
        "session",
        "action=\"capture\", event_type=\"decision\", title=\"...\", content=\"...\"",
    );
    let handoff = tool_call(
        prefix,
        "entity",
        "kind=\"handoff\", action=\"create\", body={\"title\":\"...\",\"summary\":\"...\",\"scope\":\"...\",\"next_steps\":[...]}",
    );
    let capsule = tool_call(
        prefix,
        "capsule",
        "action=\"create\", scope=\"session\", session_id=\"<current session id>\", purpose=\"handoff\"",
    );

    vec![
        HarnessTeachingStep {
            id: HarnessTeachingStepId::InitializeOnce,
            title: "Initialize once".to_string(),
            requirement: "Initialize before other ContextStream calls; reuse its scope/session id. workspace_id is mandatory for workspace-scoped calls: use IDs from managed rules or init/context and pass workspace_id explicitly to memory, session, entity, and task calls. Never rely on implicit session scope; omit only during initialization when unavailable.".to_string(),
            canonical_calls: vec![init],
        },
        HarnessTeachingStep {
            id: HarnessTeachingStepId::GroundEveryTurn,
            title: "Ground every turn".to_string(),
            requirement: "Ground the current user message before acting. Prefer context; use session ground only when context is not exposed on the current MCP surface.".to_string(),
            canonical_calls: vec![context, ground],
        },
        HarnessTeachingStep {
            id: HarnessTeachingStepId::SearchBeforeLocalDiscovery,
            title: "Search before local discovery".to_string(),
            requirement: "Use ContextStream search before broad local code or file discovery. Read the returned paths; use local discovery only after a targeted zero-result retry or for known-new edits.".to_string(),
            canonical_calls: vec![search],
        },
        HarnessTeachingStep {
            id: HarnessTeachingStepId::ConsultDurableKnowledge,
            title: "Consult durable knowledge".to_string(),
            requirement: "Before guessing about prior work, policy, preferences, or architecture, query the matching ContextStream memory, decision, lesson, document, plan, or task surface.".to_string(),
            canonical_calls: vec![memory_search, decisions, lessons],
        },
        HarnessTeachingStep {
            id: HarnessTeachingStepId::PersistDurableWork,
            title: "Persist durable work canonically".to_string(),
            requirement: "Store plans, lessons, and decisions in their canonical ContextStream tools so they can surface in future sessions; do not substitute local scratch files or generic plan events.".to_string(),
            canonical_calls: vec![plan, lesson, decision],
        },
        HarnessTeachingStep {
            id: HarnessTeachingStepId::CreateCanonicalHandoff,
            title: "Create canonical handoffs".to_string(),
            requirement: "Create every requested agent/session handoff as a ContextStream handoff entity, preserving verified facts, eliminated hypotheses, scope, and actionable next steps. HANDOFF.md, a generic document, a scratch prompt, or prose alone is not a substitute. Add a capsule when a portable bundle or share link is requested; omit to_user_id when the recipient is unknown rather than inventing it.".to_string(),
            canonical_calls: vec![handoff, capsule],
        },
    ]
}

fn delivery_notice(
    harness_id: Option<HarnessId>,
    capabilities: HarnessTeachingCapabilities,
) -> Option<String> {
    match harness_id {
        Some(harness_id) if !capabilities.mcp_tools => Some(format!(
            "Delivery limitation: {} has no native ContextStream MCP tool transport. Treat these calls as reference guidance for a configured bridge; do not claim they executed in this harness.",
            harness_id.display_name()
        )),
        None => Some(
            "Delivery limitation: client identity is unknown. Use only ContextStream tools actually exposed by this MCP surface; no hook, rules-load, or enforcement capability is assumed."
                .to_string(),
        ),
        Some(_) => None,
    }
}

fn full_render(steps: &[HarnessTeachingStep], heading: &str, notice: Option<&str>) -> String {
    let mut lines = vec![
        format!("<!-- contextstream-teaching-version: {HARNESS_TEACHING_VERSION} -->"),
        format!("## {heading} ({HARNESS_TEACHING_VERSION})"),
    ];
    if let Some(notice) = notice {
        lines.push(notice.to_string());
    }
    for (index, step) in steps.iter().enumerate() {
        lines.push(format!(
            "{}. **{}:** {} Calls: `{}`.",
            index + 1,
            step.title,
            step.requirement,
            step.canonical_calls.join("`; fallback/related: `")
        ));
    }
    lines.join("\n")
}

fn compact_render(steps: &[HarnessTeachingStep], notice: Option<&str>) -> String {
    let mut lines = vec![format!(
        "[CONTEXTSTREAM WORKFLOW {HARNESS_TEACHING_VERSION}]"
    )];
    if let Some(notice) = notice {
        lines.push(notice.to_string());
    }
    for (index, step) in steps.iter().enumerate() {
        lines.push(format!(
            "{}) {} — `{}`",
            index + 1,
            step.title,
            step.canonical_calls.join("` / `")
        ));
    }
    lines.join("\n")
}

/// Build the canonical teaching contract for a known or unknown harness.
pub fn build_harness_teaching(
    harness_id: Option<HarnessId>,
    delivery: HarnessTeachingDelivery,
) -> HarnessTeachingContract {
    let steps = canonical_steps(harness_id);
    let capabilities = HarnessTeachingCapabilities::for_harness(harness_id);
    let delivery_notice = delivery_notice(harness_id, capabilities);
    let rendered_guidance = match delivery {
        HarnessTeachingDelivery::StaticRules => full_render(
            &steps,
            "Core ContextStream Workflow",
            delivery_notice.as_deref(),
        ),
        HarnessTeachingDelivery::LifecycleInstructions => full_render(
            &steps,
            "ContextStream MCP Workflow",
            delivery_notice.as_deref(),
        ),
        HarnessTeachingDelivery::HelpWorkflow => {
            full_render(&steps, "ContextStream Workflow", delivery_notice.as_deref())
        }
        HarnessTeachingDelivery::HookReminder => compact_render(&steps, delivery_notice.as_deref()),
        HarnessTeachingDelivery::StatelessDiscovery => full_render(
            &steps,
            "ContextStream Stateless MCP Workflow",
            delivery_notice.as_deref(),
        ),
    };
    let budget = HarnessTeachingBudget::measure(delivery, &rendered_guidance);
    debug_assert!(
        budget.within_budget,
        "built-in ContextStream teaching exceeded its {:?} budget: {:?}",
        delivery, budget
    );

    HarnessTeachingContract {
        schema_version: HARNESS_TEACHING_SCHEMA_VERSION,
        teaching_version: HARNESS_TEACHING_VERSION.to_string(),
        harness_id,
        harness_name: harness_id
            .map(|id| id.display_name().to_string())
            .unwrap_or_else(|| "Unknown MCP client".to_string()),
        recognized_harness: harness_id.is_some(),
        delivery,
        capabilities,
        delivery_notice,
        steps,
        rendered_guidance,
        budget,
    }
}

/// Whether a legacy initialize response may legally contain `instructions`.
///
/// This is an allowlist, not a lexical date comparison: unknown future
/// revisions must be implemented and conformance-tested before being claimed.
pub fn legacy_protocol_supports_instructions(protocol_version: &str) -> bool {
    matches!(
        protocol_version,
        MCP_PROTOCOL_2025_03_26 | MCP_PROTOCOL_2025_06_18
    )
}

/// Render initialize instructions only for a protocol revision that defines
/// the field. The currently advertised 2024 revision therefore returns None.
pub fn legacy_initialize_instructions(
    protocol_version: &str,
    harness_id: Option<HarnessId>,
) -> Option<String> {
    legacy_protocol_supports_instructions(protocol_version).then(|| {
        build_harness_teaching(harness_id, HarnessTeachingDelivery::LifecycleInstructions)
            .rendered_guidance
    })
}

/// Build stateless discovery teaching only after the complete 2026 adapter is
/// proven. Exact version matching prevents a partial or future protocol claim.
pub fn stateless_discovery_teaching(
    protocol_version: &str,
    conformance: StatelessMcpConformance,
    harness_id: Option<HarnessId>,
) -> Option<HarnessTeachingContract> {
    (protocol_version == MCP_PROTOCOL_2026_07_28 && conformance.fully_conformant())
        .then(|| build_harness_teaching(harness_id, HarnessTeachingDelivery::StatelessDiscovery))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn initialization_teaching_reuses_managed_scope_ids() {
        let contract =
            build_harness_teaching(Some(HarnessId::Codex), HarnessTeachingDelivery::StaticRules);
        let initialize = contract
            .steps
            .iter()
            .find(|step| step.id == HarnessTeachingStepId::InitializeOnce)
            .expect("initialize teaching step");

        assert!(initialize.requirement.contains("managed rules"));
        assert!(initialize.requirement.contains("workspace_id is mandatory"));
        assert!(initialize
            .requirement
            .contains("Never rely on implicit session scope"));
        assert!(initialize.canonical_calls[0].contains("workspace_id="));
        assert!(initialize.canonical_calls[0].contains("project_id="));

        let context = contract
            .steps
            .iter()
            .find(|step| step.id == HarnessTeachingStepId::GroundEveryTurn)
            .expect("grounding teaching step");
        assert!(context
            .canonical_calls
            .iter()
            .all(|call| call.contains("workspace_id=\"<current_workspace_id>\"")));
    }

    #[test]
    fn canonical_handoff_step_rejects_local_file_substitutes() {
        assert_eq!(HARNESS_TEACHING_VERSION, "harness_teaching_v4");
        assert_eq!(HARNESS_TEACHING_SCHEMA_VERSION, 3);

        for harness in [Some(HarnessId::ClaudeCode), Some(HarnessId::Codex), None] {
            let contract = build_harness_teaching(harness, HarnessTeachingDelivery::HelpWorkflow);
            let handoff = contract
                .steps
                .iter()
                .find(|step| step.id == HarnessTeachingStepId::CreateCanonicalHandoff)
                .expect("canonical handoff teaching step");

            for required in [
                "verified facts",
                "eliminated hypotheses",
                "HANDOFF.md",
                "not a substitute",
                "portable bundle or share link",
                "omit to_user_id",
            ] {
                assert!(handoff.requirement.contains(required), "missing {required}");
            }

            let expected_prefix = if harness == Some(HarnessId::ClaudeCode) {
                "mcp__contextstream__"
            } else {
                ""
            };
            assert!(handoff.canonical_calls[0].starts_with(&format!("{expected_prefix}entity(")));
            assert!(handoff.canonical_calls[0].contains("kind=\"handoff\""));
            assert!(!handoff.canonical_calls[0].contains("to_user_id"));
            assert!(handoff.canonical_calls[1].starts_with(&format!("{expected_prefix}capsule(")));
            assert!(handoff.canonical_calls[1].contains("purpose=\"handoff\""));
        }
    }

    #[test]
    fn every_delivery_has_the_same_ordered_semantic_contract() {
        let expected = HarnessTeachingStepId::ALL.to_vec();
        for harness in HarnessId::ALL.iter().copied().map(Some).chain([None]) {
            let baseline =
                build_harness_teaching(harness, HarnessTeachingDelivery::StaticRules).steps;
            for delivery in [
                HarnessTeachingDelivery::StaticRules,
                HarnessTeachingDelivery::LifecycleInstructions,
                HarnessTeachingDelivery::HelpWorkflow,
                HarnessTeachingDelivery::HookReminder,
                HarnessTeachingDelivery::StatelessDiscovery,
            ] {
                let contract = build_harness_teaching(harness, delivery);
                assert_eq!(
                    contract
                        .steps
                        .iter()
                        .map(|step| step.id)
                        .collect::<Vec<_>>(),
                    expected
                );
                assert_eq!(
                    contract.steps, baseline,
                    "delivery changed semantics for {harness:?} {delivery:?}"
                );
                assert_eq!(contract.teaching_version, HARNESS_TEACHING_VERSION);
                assert!(contract.budget.within_budget, "{contract:#?}");
                assert!(contract
                    .rendered_guidance
                    .contains(HARNESS_TEACHING_VERSION));
            }
        }
    }

    #[test]
    fn render_budgets_are_strict_for_every_capability_class() {
        let tokenizer = tiktoken_rs::o200k_base_singleton();
        let mut observed_classes = HashSet::new();
        for harness in HarnessId::ALL.iter().copied().map(Some).chain([None]) {
            let capabilities = HarnessTeachingCapabilities::for_harness(harness);
            observed_classes.insert((
                capabilities.installable,
                capabilities.static_rules,
                capabilities.rules_auto_loaded,
                capabilities.mcp_tools,
                capabilities.lifecycle_hooks,
                capabilities.hooks.session_start,
                capabilities.hooks.user_prompt_submit,
                capabilities.hooks.pre_tool_use,
                capabilities.hooks.post_tool_use,
                capabilities.hooks.instructions_loaded,
                capabilities.hard_first_call_enforcement,
                capabilities.dynamic_guidance,
            ));
            for delivery in [
                HarnessTeachingDelivery::StaticRules,
                HarnessTeachingDelivery::LifecycleInstructions,
                HarnessTeachingDelivery::HelpWorkflow,
                HarnessTeachingDelivery::HookReminder,
                HarnessTeachingDelivery::StatelessDiscovery,
            ] {
                let contract = build_harness_teaching(harness, delivery);
                assert!(contract.budget.within_budget, "{contract:#?}");
                assert!(contract.budget.chars <= contract.budget.max_chars);
                assert!(contract.budget.lines <= contract.budget.max_lines);
                assert!(contract.budget.estimated_tokens <= contract.budget.max_estimated_tokens);
                let exact_tokens = tokenizer.encode_ordinary(&contract.rendered_guidance).len();
                assert!(
                    exact_tokens <= contract.budget.max_estimated_tokens,
                    "exact o200k_base budget exceeded ({exact_tokens}): {contract:#?}"
                );
            }
        }
        assert!(
            observed_classes.len() >= 5,
            "capability coverage collapsed: {observed_classes:?}"
        );
    }

    #[test]
    fn tool_syntax_changes_but_semantics_do_not() {
        let claude = build_harness_teaching(
            Some(HarnessId::ClaudeCode),
            HarnessTeachingDelivery::StaticRules,
        );
        let codex =
            build_harness_teaching(Some(HarnessId::Codex), HarnessTeachingDelivery::StaticRules);
        assert!(claude
            .rendered_guidance
            .contains("mcp__contextstream__init"));
        assert!(codex.rendered_guidance.contains("`init("));
        assert!(!codex.rendered_guidance.contains("mcp__contextstream__"));
        for (left, right) in claude.steps.iter().zip(&codex.steps) {
            assert_eq!(left.id, right.id);
            assert_eq!(left.title, right.title);
            assert_eq!(left.requirement, right.requirement);
        }
    }

    #[test]
    fn representative_capability_classes_match_their_golden_contract() {
        let goldens = [
            (
                Some(HarnessId::ClaudeCode),
                HarnessTeachingCapabilities {
                    installable: true,
                    static_rules: true,
                    rules_auto_loaded: true,
                    rules_format: RulesFormat::Markdown,
                    mcp_tools: true,
                    mcp_transport: McpTransportSupport::LocalAndRemote,
                    lifecycle_hooks: true,
                    hooks: HookCapabilities::all(),
                    hard_first_call_enforcement: true,
                    dynamic_guidance: true,
                    teaching_load_evidence: TeachingLoadEvidence::DirectHook,
                },
                "mcp__contextstream__init",
            ),
            (
                Some(HarnessId::Codex),
                HarnessTeachingCapabilities {
                    installable: true,
                    static_rules: true,
                    rules_auto_loaded: true,
                    rules_format: RulesFormat::Markdown,
                    mcp_tools: true,
                    mcp_transport: McpTransportSupport::LocalAndRemote,
                    lifecycle_hooks: false,
                    hooks: HookCapabilities::none(),
                    hard_first_call_enforcement: false,
                    dynamic_guidance: false,
                    teaching_load_evidence: TeachingLoadEvidence::BehavioralInference,
                },
                "`init(",
            ),
            (
                Some(HarnessId::Aider),
                HarnessTeachingCapabilities {
                    installable: true,
                    static_rules: true,
                    rules_auto_loaded: true,
                    rules_format: RulesFormat::AiderYaml,
                    mcp_tools: false,
                    mcp_transport: McpTransportSupport::None,
                    lifecycle_hooks: false,
                    hooks: HookCapabilities::none(),
                    hard_first_call_enforcement: false,
                    dynamic_guidance: false,
                    teaching_load_evidence: TeachingLoadEvidence::NotObservable,
                },
                "`init(",
            ),
            (
                Some(HarnessId::ChatGptGateway),
                HarnessTeachingCapabilities {
                    installable: false,
                    static_rules: false,
                    rules_auto_loaded: false,
                    rules_format: RulesFormat::None,
                    mcp_tools: true,
                    mcp_transport: McpTransportSupport::RemoteOnly,
                    lifecycle_hooks: false,
                    hooks: HookCapabilities::none(),
                    hard_first_call_enforcement: false,
                    dynamic_guidance: false,
                    teaching_load_evidence: TeachingLoadEvidence::BehavioralInference,
                },
                "`init(",
            ),
            (
                None,
                HarnessTeachingCapabilities {
                    installable: false,
                    static_rules: false,
                    rules_auto_loaded: false,
                    rules_format: RulesFormat::None,
                    mcp_tools: false,
                    mcp_transport: McpTransportSupport::None,
                    lifecycle_hooks: false,
                    hooks: HookCapabilities::none(),
                    hard_first_call_enforcement: false,
                    dynamic_guidance: false,
                    teaching_load_evidence: TeachingLoadEvidence::NotObservable,
                },
                "`init(",
            ),
        ];

        for (harness_id, expected_capabilities, expected_syntax) in goldens {
            let contract =
                build_harness_teaching(harness_id, HarnessTeachingDelivery::HelpWorkflow);
            assert_eq!(contract.capabilities, expected_capabilities);
            assert!(contract.rendered_guidance.contains(expected_syntax));
            if expected_capabilities.mcp_tools {
                assert!(contract.delivery_notice.is_none());
            } else {
                assert!(contract
                    .delivery_notice
                    .as_deref()
                    .is_some_and(|notice| notice.contains("Delivery limitation")));
            }
            assert_eq!(
                contract
                    .steps
                    .iter()
                    .map(|step| step.id)
                    .collect::<Vec<_>>(),
                HarnessTeachingStepId::ALL
            );
        }
    }

    #[test]
    fn unknown_clients_receive_guidance_without_capability_claims() {
        let contract = build_harness_teaching(None, HarnessTeachingDelivery::HelpWorkflow);
        assert!(!contract.recognized_harness);
        assert_eq!(contract.harness_name, "Unknown MCP client");
        assert!(!contract.capabilities.installable);
        assert!(!contract.capabilities.static_rules);
        assert!(!contract.capabilities.rules_auto_loaded);
        assert!(!contract.capabilities.mcp_tools);
        assert!(!contract.capabilities.lifecycle_hooks);
        assert_eq!(contract.capabilities.hooks, HookCapabilities::none());
        assert!(contract
            .rendered_guidance
            .contains("client identity is unknown"));
        assert_eq!(contract.steps.len(), HarnessTeachingStepId::ALL.len());
        assert!(contract.rendered_guidance.contains("`init("));
    }

    #[test]
    fn structured_capabilities_project_every_canonical_profile_exactly() {
        for harness_id in HarnessId::ALL {
            let profile = harness_id.profile();
            let capabilities = HarnessTeachingCapabilities::for_harness(Some(*harness_id));
            assert_eq!(capabilities.installable, profile.installable);
            assert_eq!(
                capabilities.static_rules,
                profile.rules_format != RulesFormat::None
            );
            assert_eq!(capabilities.rules_auto_loaded, profile.rules_auto_loaded);
            assert_eq!(capabilities.rules_format, profile.rules_format);
            assert_eq!(
                capabilities.mcp_tools,
                profile.mcp_support != McpTransportSupport::None
            );
            assert_eq!(capabilities.mcp_transport, profile.mcp_support);
            assert_eq!(capabilities.lifecycle_hooks, profile.hooks.any());
            assert_eq!(capabilities.hooks, profile.hooks);
            assert_eq!(
                capabilities.hard_first_call_enforcement,
                profile.hard_first_call_enforcement
            );
            assert_eq!(capabilities.dynamic_guidance, profile.dynamic_guidance);
            assert_eq!(
                capabilities.teaching_load_evidence,
                profile.teaching_load_evidence
            );
        }
    }

    #[test]
    fn lifecycle_instructions_are_revision_gated() {
        assert!(!legacy_protocol_supports_instructions(
            MCP_PROTOCOL_2024_11_05
        ));
        assert!(
            legacy_initialize_instructions(MCP_PROTOCOL_2024_11_05, Some(HarnessId::Codex))
                .is_none()
        );
        for version in [MCP_PROTOCOL_2025_03_26, MCP_PROTOCOL_2025_06_18] {
            let instructions = legacy_initialize_instructions(version, Some(HarnessId::Codex))
                .expect("supported legacy instructions");
            assert!(instructions.contains(HARNESS_TEACHING_VERSION));
        }
        assert!(legacy_initialize_instructions("2099-01-01", None).is_none());
        assert!(legacy_initialize_instructions(MCP_PROTOCOL_2026_07_28, None).is_none());
    }

    #[test]
    fn stateless_adapter_refuses_every_partial_conformance_state() {
        let complete = StatelessMcpConformance {
            server_discover: true,
            no_initialize_or_protocol_sessions: true,
            removed_legacy_core_methods: true,
            per_request_protocol_version: true,
            per_request_client_capabilities: true,
            response_server_identity: true,
            result_type: true,
            cacheable_results: true,
            http_header_routing: true,
        };
        let partials = [
            StatelessMcpConformance {
                server_discover: false,
                ..complete
            },
            StatelessMcpConformance {
                no_initialize_or_protocol_sessions: false,
                ..complete
            },
            StatelessMcpConformance {
                removed_legacy_core_methods: false,
                ..complete
            },
            StatelessMcpConformance {
                per_request_protocol_version: false,
                ..complete
            },
            StatelessMcpConformance {
                per_request_client_capabilities: false,
                ..complete
            },
            StatelessMcpConformance {
                response_server_identity: false,
                ..complete
            },
            StatelessMcpConformance {
                result_type: false,
                ..complete
            },
            StatelessMcpConformance {
                cacheable_results: false,
                ..complete
            },
            StatelessMcpConformance {
                http_header_routing: false,
                ..complete
            },
        ];
        for partial in partials {
            assert!(!partial.fully_conformant());
            assert!(stateless_discovery_teaching(MCP_PROTOCOL_2026_07_28, partial, None).is_none());
        }

        let contract = stateless_discovery_teaching(MCP_PROTOCOL_2026_07_28, complete, None)
            .expect("complete adapter");
        assert_eq!(
            contract.delivery,
            HarnessTeachingDelivery::StatelessDiscovery
        );
        assert!(stateless_discovery_teaching(MCP_PROTOCOL_2025_06_18, complete, None).is_none());
    }

    #[test]
    fn structured_contract_round_trips_and_schema_is_versioned() {
        let contract = build_harness_teaching(
            Some(HarnessId::OpenCode),
            HarnessTeachingDelivery::HelpWorkflow,
        );
        let encoded = serde_json::to_string(&contract).expect("serialize teaching");
        let decoded: HarnessTeachingContract =
            serde_json::from_str(&encoded).expect("deserialize teaching");
        assert_eq!(decoded, contract);

        let schema = serde_json::to_value(schemars::schema_for!(HarnessTeachingContract))
            .expect("teaching schema");
        let schema_text = schema.to_string();
        assert!(schema_text.contains("initialize_once"));
        assert!(schema_text.contains("hook_reminder"));
    }
}
