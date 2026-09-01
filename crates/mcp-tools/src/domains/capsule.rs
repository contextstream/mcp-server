//! Consolidated ContextCapsule domain tool.

use async_trait::async_trait;
use chrono::{DateTime, Duration, Utc};
use mcp_client::{
    resolve_capsule_share_token_str, CapsuleAckParams, CapsuleAuditParams, CapsuleChunkParams,
    CapsuleContextDocParams, CapsuleListSharesParams, CapsuleOpenParams, CapsulePrimerParams,
    CapsuleShareParams, CapsuleStreamParams, ContextStreamClient, CreateCapsuleParams,
    ListCapsulesParams,
};
use mcp_types::{
    api::{
        ContextCapsuleAuditEventResponse, ContextCapsuleChunkResponse, ContextCapsuleResponse,
        ContextCapsuleShareResponse,
    },
    tool::{ToolAnnotations, ToolCategory, ToolMetadata, ToolResult},
    Error, Result,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::sync::Arc;
use uuid::Uuid;

use crate::registry::ToolHandler;
use crate::schema::SchemaBuilder;

pub(crate) const VALID_ACTIONS: &[&str] = &[
    "open",
    "get",
    "delete",
    "list",
    "create",
    "share",
    "chunk",
    "stream",
    "context_doc",
    "bootstrap_prompt",
    "graph",
    "audit",
    "list_shares",
    "revoke_share",
    "ack",
    "primer",
    "diff",
    "schedule",
    "schedule_list",
    "schedule_delete",
    "explain",
];
pub(crate) const VALID_GRAPHS: &[&str] = &["explorer", "knowledge", "code"];
const VALID_FORMATS: &[&str] = &["summary", "markdown", "text", "ndjson"];
const VALID_AUDIENCES: &[&str] = &[
    "self",
    "team",
    "external_agent",
    "public_link",
    "support",
    "bootstrap_link",
];
const VALID_INCLUDE_CODE: &[&str] = &["none", "lazy", "inline"];
const VALID_REDACTION_LEVELS: &[&str] = &["none", "standard", "strict"];
const VALID_PURPOSES: &[&str] = &[
    "bootstrap",
    "handoff",
    "snapshot",
    "debug",
    "review",
    "onboarding",
    "external_agent",
    "custom",
];
const VALID_SCOPES: &[&str] = &["workspace", "project", "session"];
const VALID_MODES: &[&str] = &["live", "snapshot"];
const VALID_ACCESS_SCOPES: &[&str] = &["authenticated", "authenticated_share", "public_share"];
const VALID_TOKEN_SHARE_AUDIENCES: &[&str] = &[
    "team",
    "external_agent",
    "public_link",
    "support",
    "bootstrap_link",
];

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct CapsuleInput {
    pub action: String,
    pub capsule_id: Option<String>,
    pub share_token: Option<String>,
    pub share_id: Option<String>,
    pub url: Option<String>,
    pub format: Option<String>,
    pub hydrate: Option<bool>,
    pub chunk_id: Option<String>,
    pub cursor_chunk_id: Option<String>,
    pub audience: Option<String>,
    pub expires_in_days: Option<i64>,
    pub multi_use: Option<bool>,
    pub include_personal: Option<bool>,
    pub include_code: Option<String>,
    pub redaction_level: Option<String>,
    pub permissions: Option<String>,
    pub scope: Option<String>,
    pub workspace_id: Option<String>,
    pub project_id: Option<String>,
    /// External AI-tool session id for `action="create", scope="session"`.
    /// The backend resolves this to the owner-visible transcript and packages
    /// the full session as lazy chunks.
    pub session_id: Option<String>,
    /// Exact transcript UUID for `action="create", scope="session"`. Takes
    /// precedence over session_id when both are supplied.
    pub transcript_id: Option<String>,
    pub purpose: Option<String>,
    pub name: Option<String>,
    pub mode: Option<String>,
    pub sections: Option<Vec<String>>,
    pub event_kind: Option<String>,
    pub access_scope: Option<String>,
    pub limit: Option<i64>,
    pub offset: Option<i64>,
    pub graph: Option<String>,
    pub max_uses: Option<i32>,
    pub max_inline_tokens: Option<u32>,
    pub refresh_if_stale: Option<bool>,
    /// Optional folder path. When set with no `project_id`, the MCP
    /// client resolves it locally against `indexed-projects.json` (same
    /// lookup used by `init`). Eliminates empty-capsule failures caused
    /// by guessing the wrong workspace/project id.
    pub folder_path: Option<String>,
    /// Optional project name. Backend resolves it via
    /// `find_by_name_in_workspace` when no `project_id` is set.
    pub project_name: Option<String>,
    /// Explicit opt-in to risky external/public share settings. Only
    /// honored on `action="share"` when audience is external_agent /
    /// public_link / support. When false (default) the backend rejects
    /// shares whose policy still has personal artifacts, code, no
    /// redaction, or an expiry beyond 7 days — and the error body
    /// enumerates each risky setting plus three suggested fixes.
    pub allow_risky_policy: Option<bool>,
    /// For `action="ack"`: the list of section ids the recipient
    /// actually read (e.g. `["decisions", "lessons"]`). Surfaces in
    /// list_shares so the sender can see what the recipient focused on.
    pub sections_read: Option<Vec<String>>,
    /// For `action="ack"`: free-text note the consuming agent attaches
    /// to its receipt, e.g. "picking up Shark training restart".
    pub notes: Option<String>,
    /// Force-include specific items by id in their corresponding
    /// sections (Phase 3 — plan-step-18). Each entry is
    /// `{ "kind": "doc"|"event"|"task"|"decision"|"lesson"|"todo"|...,
    /// "id": "<uuid-or-external-ref>" }`. UUID ids get a `pinned: true`
    /// marker in the manifest payload; non-UUID ids are preserved as
    /// handoff references.
    pub pin_items: Option<Vec<serde_json::Value>>,
    /// External handoff references that are not ContextStream table rows
    /// (GitHub runs, commit SHAs, deployment URLs, etc.).
    pub external_refs: Option<Vec<serde_json::Value>>,
    /// For `action="diff"`: the capsule id to use as the "from" side
    /// of the comparison (Phase 4 — plan-step-21). Pair with
    /// `to_capsule_id` for the "to" side.
    pub from_capsule_id: Option<String>,
    /// For `action="diff"`: the capsule id to use as the "to" side.
    pub to_capsule_id: Option<String>,
    /// For `action="schedule"`: cron expression for regenerating the
    /// capsule (Phase 4 — plan-step-22).
    pub cron: Option<String>,
    /// For `action="share"`: opt the share into "install as a Skill on
    /// the recipient's ack" (Phase 4 — plan-step-20). The share's
    /// config records the intent; the skill is created when the
    /// recipient calls `capsule(action="ack")` with their own auth.
    pub auto_install_skill: Option<bool>,
    /// For `action="schedule_delete"`: the schedule id to delete.
    pub schedule_id: Option<String>,
    /// For `action="schedule"`: when true, the worker also re-shares
    /// the refreshed capsule with its existing share audience.
    pub refresh_shares: Option<bool>,
    /// For `action="share"`: gate the rich Tier-2 destinations behind a
    /// generated revocable unlock key. The Tier-1 overview + navigation
    /// map stay open with the link; the key is returned ONCE in the
    /// result and sent by recipients as the X-Capsule-Key header. Keyed
    /// shares default to multi-use. Default false.
    pub require_unlock_key: Option<bool>,
    /// For `action="share"` with require_unlock_key=true: which Tier-2
    /// destinations the key unlocks (explorer, knowledge_graph,
    /// code_graph, chunks, stream). Omit for all.
    pub unlock_destinations: Option<Vec<String>>,
}

pub struct CapsuleTool {
    client: ContextStreamClient,
    atlas_layer: mcp_types::atlas_layer::AtlasLayer,
}

impl CapsuleTool {
    pub fn new(client: ContextStreamClient) -> Self {
        Self::with_atlas(client, mcp_types::atlas_layer::noop_layer())
    }

    pub fn with_atlas(
        client: ContextStreamClient,
        atlas_layer: mcp_types::atlas_layer::AtlasLayer,
    ) -> Self {
        Self {
            client,
            atlas_layer,
        }
    }

    fn parse_uuid(value: &Option<String>, field_name: &str) -> Result<Option<Uuid>> {
        match value.as_deref() {
            Some(raw) => {
                Ok(Some(Uuid::parse_str(raw).map_err(|_| {
                    Error::Validation(format!("Invalid {}", field_name))
                })?))
            }
            None => Ok(None),
        }
    }

    fn require_capsule_id(input: &CapsuleInput) -> Result<String> {
        input
            .capsule_id
            .clone()
            .filter(|value| !value.trim().is_empty())
            .ok_or_else(|| Error::Validation("capsule_id is required".to_string()))
    }

    fn require_chunk_id(input: &CapsuleInput) -> Result<String> {
        input
            .chunk_id
            .clone()
            .filter(|value| !value.trim().is_empty())
            .ok_or_else(|| Error::Validation("chunk_id is required".to_string()))
    }

    fn require_share_id(input: &CapsuleInput) -> Result<Uuid> {
        let raw = input
            .share_id
            .as_deref()
            .ok_or_else(|| Error::Validation("share_id is required".to_string()))?;
        Uuid::parse_str(raw).map_err(|_| Error::Validation("Invalid share_id".to_string()))
    }

    fn validate_share_audience(audience: &Option<String>) -> Result<()> {
        if let Some(value) = audience.as_deref() {
            if value == "self" {
                return Err(Error::Validation(
                    "audience=self does not mint share tokens; use authenticated capsule endpoints instead"
                        .to_string(),
                ));
            }
            if !VALID_TOKEN_SHARE_AUDIENCES.contains(&value) {
                return Err(Error::Validation(
                    "capsule token shares only support audiences: team, external_agent, public_link, support".to_string(),
                ));
            }
        }
        Ok(())
    }
}

#[async_trait]
impl ToolHandler for CapsuleTool {
    async fn execute(&self, input: Value) -> Result<ToolResult> {
        let input: CapsuleInput =
            serde_json::from_value(input).map_err(|e| Error::Validation(e.to_string()))?;
        let action = input.action.to_lowercase();

        match action.as_str() {
            "open" => {
                if is_context_doc_format(input.format.as_deref()) {
                    let response = self
                        .client
                        .capsule_context_doc(CapsuleContextDocParams {
                            capsule_id: input.capsule_id.clone(),
                            share_token: input.share_token.clone(),
                            url: input.url.clone(),
                            format: input.format.clone(),
                        })
                        .await?;
                    return Ok(ToolResult::text(response));
                }

                if is_stream_format(input.format.as_deref()) {
                    let response = self
                        .client
                        .capsule_stream(CapsuleStreamParams {
                            capsule_id: input.capsule_id.clone(),
                            share_token: input.share_token.clone(),
                            url: input.url.clone(),
                            cursor_chunk_id: input.cursor_chunk_id.clone(),
                        })
                        .await?;
                    let text = format_stream_summary(&response);
                    return Ok(ToolResult::with_structured(
                        text,
                        serde_json::json!({ "ndjson": response }),
                    ));
                }

                // P1 #9 — CapsuleOpen warm cache. 24 hr TTL.
                // Capsules are immutable artifacts; same capsule_id
                // always produces the same response. Only cache the
                // default summary path (format unset, hydrate=false)
                // — other paths fetch additional streams or render
                // differently and are session-specific.
                let cache_eligible = !input.hydrate.unwrap_or(false)
                    && input.capsule_id.is_some()
                    && input.share_token.is_none()
                    && input.url.is_none();
                let workspace_for_cache = input
                    .workspace_id
                    .as_deref()
                    .and_then(|s| Uuid::parse_str(s).ok());
                let user_scope_token =
                    super::atlas_warm_cache::current_user_scope_token();
                let cached_response = if cache_eligible {
                    if let (Some(ws), Some(cap_id)) =
                        (workspace_for_cache, input.capsule_id.as_deref())
                    {
                        let scope = mcp_types::atlas_layer::AtlasFederationScope {
                            workspace_id: ws,
                            project_id: None,
                            scope_hash: super::atlas_warm_cache::scope_hash_for_capsule_open(
                                ws,
                                user_scope_token.as_deref(),
                                cap_id,
                            ),
                            user_scope: user_scope_token.clone(),
                        };
                        super::atlas_warm_cache::try_lookup(
                            &self.atlas_layer,
                            mcp_types::atlas_layer::AtlasWarmCacheKind::CapsuleOpen,
                            scope,
                            500,
                        )
                        .await
                    } else {
                        None
                    }
                } else {
                    None
                };
                let response: ContextCapsuleResponse = if let Some(bundle) = cached_response {
                    match serde_json::from_value(bundle.payload) {
                        Ok(parsed) => parsed,
                        Err(_) => {
                            self.client
                                .open_capsule(CapsuleOpenParams {
                                    capsule_id: input.capsule_id.clone(),
                                    share_token: input.share_token.clone(),
                                    url: input.url.clone(),
                                    format: input.format.clone(),
                                    hydrate: input.hydrate,
                                })
                                .await?
                        }
                    }
                } else {
                    let r = self
                        .client
                        .open_capsule(CapsuleOpenParams {
                            capsule_id: input.capsule_id.clone(),
                            share_token: input.share_token.clone(),
                            url: input.url.clone(),
                            format: input.format.clone(),
                            hydrate: input.hydrate,
                        })
                        .await?;
                    if cache_eligible {
                        if let (Some(ws), Some(cap_id)) =
                            (workspace_for_cache, input.capsule_id.as_deref())
                        {
                            if let Ok(payload) = serde_json::to_value(&r) {
                                let scope = mcp_types::atlas_layer::AtlasFederationScope {
                                    workspace_id: ws,
                                    project_id: None,
                                    scope_hash:
                                        super::atlas_warm_cache::scope_hash_for_capsule_open(
                                            ws,
                                            user_scope_token.as_deref(),
                                            cap_id,
                                        ),
                                    user_scope: user_scope_token.clone(),
                                };
                                super::atlas_warm_cache::put_in_background(
                                    self.atlas_layer.clone(),
                                    mcp_types::atlas_layer::AtlasWarmCacheKind::CapsuleOpen,
                                    scope,
                                    payload,
                                );
                            }
                        }
                    }
                    r
                };
                let text = format!(
                    "{}\n\n{}",
                    format_capsule_open_headline(&response),
                    format_capsule_summary(&response)
                );
                if input.hydrate.unwrap_or(false) {
                    let ndjson = self
                        .client
                        .capsule_stream(CapsuleStreamParams {
                            capsule_id: input.capsule_id.clone(),
                            share_token: input.share_token.clone(),
                            url: input.url.clone(),
                            cursor_chunk_id: input.cursor_chunk_id.clone(),
                        })
                        .await?;
                    Ok(ToolResult::with_structured(
                        format!("{}\nHydrated stream attached.", text),
                        serde_json::json!({
                            "capsule": response,
                            "ndjson": ndjson,
                        }),
                    ))
                } else {
                    Ok(ToolResult::with_structured(
                        text,
                        serde_json::to_value(response).unwrap_or_default(),
                    ))
                }
            }
            "get" => {
                let capsule_id = Self::require_capsule_id(&input)?;
                let response = self.client.get_capsule(&capsule_id).await?;
                let text = format!(
                    "{}\n\n{}",
                    format_capsule_open_headline(&response),
                    format_capsule_summary(&response)
                );
                Ok(ToolResult::with_structured(
                    text,
                    serde_json::to_value(response).unwrap_or_default(),
                ))
            }
            "delete" => {
                let capsule_id = Self::require_capsule_id(&input)?;
                let response = self.client.delete_capsule(&capsule_id).await?;
                Ok(ToolResult::with_structured(
                    format!("ContextCapsule {} deleted.", capsule_id),
                    response,
                ))
            }
            "list" => {
                let config = self.client.config().await;
                let response = self
                    .client
                    .list_capsules(ListCapsulesParams {
                        workspace_id: Self::parse_uuid(&input.workspace_id, "workspace_id")?
                            .or(config.default_workspace_id),
                        project_id: Self::parse_uuid(&input.project_id, "project_id")?
                            .or(config.default_project_id),
                        limit: input.limit,
                    })
                    .await?;
                let text = format_list_capsules_summary(&response);
                Ok(ToolResult::with_structured(
                    text,
                    serde_json::to_value(response).unwrap_or_default(),
                ))
            }
            "graph" => {
                let graph_kind = input
                    .graph
                    .as_deref()
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(str::to_lowercase)
                    .ok_or_else(|| {
                        Error::Validation("graph is required for action=graph (explorer|knowledge|code)".to_string())
                    })?;
                if !VALID_GRAPHS.contains(&graph_kind.as_str()) {
                    return Err(Error::Validation(format!(
                        "Invalid graph {:?}. Use one of: {}",
                        graph_kind,
                        VALID_GRAPHS.join(", ")
                    )));
                }
                let locator = input
                    .share_token
                    .clone()
                    .or_else(|| input.url.clone())
                    .filter(|s| !s.trim().is_empty())
                    .ok_or_else(|| {
                        Error::Validation(
                            "share_token or url is required for action=graph".to_string(),
                        )
                    })?;
                let token = resolve_capsule_share_token_str(&locator)?;
                let value = match graph_kind.as_str() {
                    "explorer" => self.client.capsule_explorer_graph(&token).await?,
                    "knowledge" => self.client.capsule_knowledge_graph(&token).await?,
                    "code" => self.client.capsule_code_graph(&token).await?,
                    _ => unreachable!(),
                };
                Ok(ToolResult::with_structured(
                    format_graph_summary(&value),
                    value,
                ))
            }
            "create" => {
                if matches!(input.scope.as_deref(), Some("session"))
                    && input
                        .session_id
                        .as_deref()
                        .map(str::trim)
                        .filter(|value| !value.is_empty())
                        .is_none()
                    && input.transcript_id.is_none()
                {
                    return Err(Error::Validation(
                        "session_id or transcript_id is required for action=create with scope=session"
                            .to_string(),
                    ));
                }
                let normalized_create_audience = input
                    .audience
                    .as_deref()
                    .map(str::trim)
                    .filter(|value| !value.is_empty())
                    .map(|value| value.to_ascii_lowercase());
                let response = self
                    .client
                    .create_capsule(CreateCapsuleParams {
                        scope: input.scope.clone(),
                        workspace_id: Self::parse_uuid(&input.workspace_id, "workspace_id")?,
                        project_id: Self::parse_uuid(&input.project_id, "project_id")?,
                        session_id: input.session_id.clone(),
                        transcript_id: Self::parse_uuid(&input.transcript_id, "transcript_id")?,
                        name: input.name.clone(),
                        purpose: input.purpose.clone(),
                        mode: input.mode.clone(),
                        sections: input.sections.clone(),
                        audience: normalized_create_audience,
                        include_personal: input.include_personal,
                        include_code: input.include_code.clone(),
                        redaction_level: input.redaction_level.clone(),
                        permissions: input.permissions.clone(),
                        max_inline_tokens: input.max_inline_tokens,
                        refresh_if_stale: input.refresh_if_stale,
                        folder_path: input.folder_path.clone(),
                        project_name: input.project_name.clone(),
                        pin_items: input.pin_items.clone(),
                        notes: input.notes.clone(),
                        external_refs: input.external_refs.clone(),
                    })
                    .await?;

                let auto_share_audience = session_create_auto_share_audience(&input);
                if let Some(audience) = auto_share_audience {
                    if let Err(err) = Self::validate_share_audience(&Some(audience.clone())) {
                        let error = err.to_string();
                        let warning = format!(
                            "Session share links were not created because auto-share audience was invalid: {}",
                            error
                        );
                        let text = format_capsule_create_result_text(
                            &response,
                            None,
                            Some(warning.as_str()),
                        );
                        let structured =
                            capsule_create_structured(&response, Some(false), None, Some(&error));
                        return Ok(ToolResult::with_structured(text, structured));
                    }
                    if response.capsule_id.trim().is_empty() {
                        let error = "capsule response did not include a capsule_id".to_string();
                        let warning =
                            format!("Session share links were not created because the {error}.");
                        let text = format_capsule_create_result_text(
                            &response,
                            None,
                            Some(warning.as_str()),
                        );
                        let structured =
                            capsule_create_structured(&response, Some(false), None, Some(&error));
                        return Ok(ToolResult::with_structured(text, structured));
                    }

                    let share_params = apply_safe_share_defaults(CapsuleShareParams {
                        name: input.name.clone().or_else(|| response.name.clone()),
                        audience: Some(audience),
                        include_personal: input.include_personal,
                        include_code: input.include_code.clone(),
                        redaction_level: input.redaction_level.clone(),
                        permissions: input.permissions.clone(),
                        expires_in_days: input.expires_in_days,
                        expires_at: None,
                        multi_use: input.multi_use,
                        max_uses: input.max_uses,
                        allow_risky_policy: input.allow_risky_policy,
                        auto_install_skill: input.auto_install_skill,
                        require_unlock_key: input.require_unlock_key,
                        unlock_destinations: input.unlock_destinations.clone(),
                    });
                    let share_result = self
                        .client
                        .capsule_share(&response.capsule_id, share_params)
                        .await;
                    match share_result {
                        Ok(share_response) => {
                            let text = format_capsule_create_result_text(
                                &response,
                                Some(&share_response),
                                None,
                            );
                            let structured = capsule_create_structured(
                                &response,
                                Some(true),
                                Some(&share_response),
                                None,
                            );
                            return Ok(ToolResult::with_structured(text, structured));
                        }
                        Err(err) => {
                            let error = err.to_string();
                            let warning = format!(
                                "Session share links were not created because auto-share failed: {}",
                                error
                            );
                            let text = format_capsule_create_result_text(
                                &response,
                                None,
                                Some(warning.as_str()),
                            );
                            let structured = capsule_create_structured(
                                &response,
                                Some(false),
                                None,
                                Some(&error),
                            );
                            return Ok(ToolResult::with_structured(text, structured));
                        }
                    }
                }

                let text = format_capsule_create_result_text(&response, None, None);
                Ok(ToolResult::with_structured(
                    text,
                    serde_json::to_value(response).unwrap_or_default(),
                ))
            }
            "share" => {
                let capsule_id = Self::require_capsule_id(&input)?;
                Self::validate_share_audience(&input.audience)?;
                let params = apply_safe_share_defaults(CapsuleShareParams {
                    name: input.name.clone(),
                    audience: input.audience.clone(),
                    include_personal: input.include_personal,
                    include_code: input.include_code.clone(),
                    redaction_level: input.redaction_level.clone(),
                    permissions: input.permissions.clone(),
                    expires_in_days: input.expires_in_days,
                    expires_at: None,
                    multi_use: input.multi_use,
                    max_uses: input.max_uses,
                    allow_risky_policy: input.allow_risky_policy,
                    auto_install_skill: input.auto_install_skill,
                    require_unlock_key: input.require_unlock_key,
                    unlock_destinations: input.unlock_destinations.clone(),
                });
                let response = self.client.capsule_share(&capsule_id, params).await?;
                let text = format_share_result_text(&capsule_id, &response);
                Ok(ToolResult::with_structured(
                    text,
                    serde_json::to_value(response).unwrap_or_default(),
                ))
            }
            "chunk" => {
                let response = self
                    .client
                    .capsule_chunk(CapsuleChunkParams {
                        capsule_id: input.capsule_id.clone(),
                        share_token: input.share_token.clone(),
                        url: input.url.clone(),
                        chunk_id: Self::require_chunk_id(&input)?,
                    })
                    .await?;
                Ok(ToolResult::with_structured(
                    format_chunk_summary(&response),
                    serde_json::to_value(response).unwrap_or_default(),
                ))
            }
            "stream" => {
                let response = self
                    .client
                    .capsule_stream(CapsuleStreamParams {
                        capsule_id: input.capsule_id.clone(),
                        share_token: input.share_token.clone(),
                        url: input.url.clone(),
                        cursor_chunk_id: input.cursor_chunk_id.clone(),
                    })
                    .await?;
                let text = format_stream_summary(&response);
                Ok(ToolResult::with_structured(
                    text,
                    serde_json::json!({ "ndjson": response }),
                ))
            }
            "context_doc" => {
                let response = self
                    .client
                    .capsule_context_doc(CapsuleContextDocParams {
                        capsule_id: input.capsule_id.clone(),
                        share_token: input.share_token.clone(),
                        url: input.url.clone(),
                        format: input.format.clone(),
                    })
                    .await?;
                Ok(ToolResult::text(response))
            }
            "bootstrap_prompt" => {
                let response = self
                    .client
                    .open_capsule(CapsuleOpenParams {
                        capsule_id: input.capsule_id.clone(),
                        share_token: input.share_token.clone(),
                        url: input.url.clone(),
                        format: None,
                        hydrate: None,
                    })
                    .await?;
                let prompt = render_bootstrap_prompt(&response);
                let headline = format_bootstrap_prompt_headline(&response, &prompt);
                let text = format!("{}\n\n{}", headline, prompt);
                let char_count = prompt.chars().count();
                let token_estimate = char_count / 4;
                Ok(ToolResult::with_structured(
                    text,
                    serde_json::json!({
                        "capsule_id": response.capsule_id,
                        "bootstrap_prompt": prompt,
                        "char_count": char_count,
                        "token_estimate": token_estimate,
                        "capsule": response,
                    }),
                ))
            }
            "list_shares" => {
                let response = if let Some(capsule_id) = input
                    .capsule_id
                    .clone()
                    .filter(|value| !value.trim().is_empty())
                {
                    self.client.capsule_list_shares(&capsule_id).await?
                } else {
                    self.client
                        .capsule_list_shares_scoped(CapsuleListSharesParams {
                            capsule_id: None,
                            workspace_id: Self::parse_uuid(&input.workspace_id, "workspace_id")?,
                            project_id: Self::parse_uuid(&input.project_id, "project_id")?,
                        })
                        .await?
                };
                let text = format_list_shares_summary(
                    &response,
                    input.capsule_id.as_deref(),
                    input.project_id.as_deref(),
                    input.workspace_id.as_deref(),
                );
                Ok(ToolResult::with_structured(
                    text,
                    serde_json::to_value(response).unwrap_or_default(),
                ))
            }
            "audit" => {
                let response = self
                    .client
                    .capsule_audit(CapsuleAuditParams {
                        capsule_id: input.capsule_id.clone(),
                        workspace_id: Self::parse_uuid(&input.workspace_id, "workspace_id")?,
                        project_id: Self::parse_uuid(&input.project_id, "project_id")?,
                        event_kind: input.event_kind.clone(),
                        access_scope: input.access_scope.clone(),
                        limit: input.limit,
                        offset: input.offset,
                    })
                    .await?;
                let text = format_audit_summary(&response, input.capsule_id.as_deref());
                Ok(ToolResult::with_structured(
                    text,
                    serde_json::to_value(response).unwrap_or_default(),
                ))
            }
            "revoke_share" => {
                let share_id = Self::require_share_id(&input)?;
                let response = self.client.capsule_revoke_share(share_id).await?;
                let text = format_revoke_share_text(&response);
                Ok(ToolResult::with_structured(
                    text,
                    serde_json::to_value(response).unwrap_or_default(),
                ))
            }
            "diff" => {
                let from = input
                    .from_capsule_id
                    .clone()
                    .filter(|s| !s.trim().is_empty())
                    .ok_or_else(|| {
                        Error::Validation(
                            "from_capsule_id is required for action=diff".to_string(),
                        )
                    })?;
                let to = input
                    .to_capsule_id
                    .clone()
                    .filter(|s| !s.trim().is_empty())
                    .ok_or_else(|| {
                        Error::Validation(
                            "to_capsule_id is required for action=diff".to_string(),
                        )
                    })?;
                let markdown = self.client.capsule_diff(&from, &to).await?;
                Ok(ToolResult::text(markdown))
            }
            "schedule" => {
                let capsule_id = Self::require_capsule_id(&input)?;
                let cron = input
                    .cron
                    .clone()
                    .filter(|s| !s.trim().is_empty())
                    .ok_or_else(|| {
                        Error::Validation(
                            "cron is required for action=schedule (e.g. \"0 14 * * 1-5\")"
                                .to_string(),
                        )
                    })?;
                let response = self
                    .client
                    .capsule_schedule(
                        &capsule_id,
                        &cron,
                        input.refresh_shares.unwrap_or(false),
                        input.name.clone(),
                    )
                    .await?;
                let text = format_schedule_create(&response);
                Ok(ToolResult::with_structured(text, response))
            }
            "schedule_list" => {
                let capsule_id = Self::require_capsule_id(&input)?;
                let response = self.client.capsule_schedule_list(&capsule_id).await?;
                let text = format_schedule_list(&response);
                Ok(ToolResult::with_structured(text, response))
            }
            "schedule_delete" => {
                let schedule_id = input
                    .schedule_id
                    .clone()
                    .filter(|s| !s.trim().is_empty())
                    .ok_or_else(|| {
                        Error::Validation(
                            "schedule_id is required for action=schedule_delete".to_string(),
                        )
                    })?;
                let response = self.client.capsule_schedule_delete(&schedule_id).await?;
                let text = format!(
                    "✓ schedule deleted · schedule_id={}\nThe worker stops firing it on the next poll.",
                    schedule_id
                );
                Ok(ToolResult::with_structured(text, response))
            }
            "primer" => {
                // Draft a runbook skeleton for a thin project so the
                // agent can fill in real signal before re-creating the
                // capsule. The backend stores the doc and returns the
                // id + content preview; the agent edits the doc with
                // memory(action="update_doc"|"create_doc") afterwards.
                let response = self
                    .client
                    .capsule_primer(CapsulePrimerParams {
                        workspace_id: Self::parse_uuid(&input.workspace_id, "workspace_id")?,
                        project_id: Self::parse_uuid(&input.project_id, "project_id")?,
                        folder_path: input.folder_path.clone(),
                        title: input.name.clone(),
                    })
                    .await?;
                let text = format_primer_summary(&response);
                Ok(ToolResult::with_structured(text, response))
            }
            "ack" => {
                // Receipt handshake from the consuming agent. Bootstrap_link
                // shares are public, so this works without any
                // Authorization header; team shares still respect the
                // OptionalAuthUser convention server-side.
                let token = input
                    .share_token
                    .clone()
                    .or_else(|| input.url.clone())
                    .filter(|s| !s.trim().is_empty())
                    .ok_or_else(|| {
                        Error::Validation(
                            "share_token (or url) is required for action=ack".to_string(),
                        )
                    })?;
                let response = self
                    .client
                    .capsule_ack_share(CapsuleAckParams {
                        share_token: token.clone(),
                        sections_read: input.sections_read.clone().unwrap_or_default(),
                        notes: input.notes.clone(),
                    })
                    .await?;
                let text = format_ack_summary(&response);
                Ok(ToolResult::with_structured(text, response))
            }
            "explain" => Ok(ToolResult::text(
                "ContextCapsule packages ContextStream context into a portable artifact. Use `context` for normal turn-by-turn retrieval and `capsule` when you need a shareable, renderable, lazy-hydrated handoff or snapshot.".to_string(),
            )),
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
            name: "capsule".to_string(),
            title: "ContextCapsule".to_string(),
            description: "ContextCapsule: portable, shareable, hydrate-on-demand snapshots of project or session context.\n\nUse capsule when:\n- User pastes a /c/<token> link or capsule_<uuid> token → action=open, url=<paste>\n- User explicitly asks for a capsule, portable bundle, share link, team link, or external-agent link → action=create; session creates automatically mint a safe share by default, so call action=share only for a different/additional policy\n- User asks for an end-of-session capsule or shareable summary → action=create, scope=session, session_id=<current session id> (or transcript_id=<uuid>). Return the capsule id plus Agent URL + Dashboard URL links; pass audience=\"self\" only when the user explicitly requests no share link\n- User wants to bootstrap a fresh agent with project state → action=open after action=create\n- User asks for a paste-ready capsule prompt (\"bootstrap prompt\", \"prompt for ChatGPT/Claude\", \"summarize this capsule for a fresh agent\") → action=bootstrap_prompt — returns Markdown the agent can paste verbatim into another LLM\n- User wants files / knowledge / code graphs from a share token → action=graph (share_token or url)\n- User wants to list or audit capsules → action=list, list_shares, audit\n\nHandoff routing:\n- A generic request to create/prepare a handoff, hand work over, or continue in another agent/session MUST create `entity(kind=\"handoff\", action=\"create\", ...)` as the durable handoff.\n- If that handoff also needs a portable bundle, capsule, or share link, create the entity AND this capsule; return both results. Capsule is the artifact, not a replacement for the handoff entity.\n- Never replace either tool call with HANDOFF.md, a scratch prompt, a generic document/event, or prose alone.\n\nDo NOT use capsule for normal turn-by-turn retrieval — use `context` instead.\nDo NOT satisfy a session-capsule request with a prose summary only; default shared results must include the created capsule id plus Agent URL / Dashboard URL links. If audience=\"self\" is passed or auto-share fails, still return the capsule id and explain the missing share link.\n\nSession capsules default to a snapshot handoff with the full transcript available as lazy chunks and owner-only transcript access enforced by the API. Session create results include share links by default; audience=\"self\" opts out. Team share links are authenticated and reusable by default; external_agent/public_link/support shares are token-gated and single-use by default. After action=open with hydrate=false (default), follow up with action=chunk for lazy sections (chunk_ids present, no inline data). For share tokens, action=graph returns JSON (explorer|knowledge|code).".to_string(),
            category: ToolCategory::Utility,
            annotations: ToolAnnotations::destructive(),
            is_pro: false,
            required_tier: None,
        })
    }

    fn input_schema(&self) -> Value {
        SchemaBuilder::new()
            .description("Portable ContextCapsule operations. open|get|list|create|share|chunk|stream|context_doc|graph|audit|list_shares|revoke_share|explain — see tool description for when to use each.")
            .string_enum("action", "Action to perform", VALID_ACTIONS, true)
            .string("capsule_id", "ContextCapsule ID", false)
            .string("share_token", "Existing brain_/capsule_ share token", false)
            .string("share_id", "ContextCapsule share UUID", false)
            .string("url", "ContextCapsule or AI Brain share URL", false)
            .string_enum("format", "Output format", VALID_FORMATS, false)
            .boolean("hydrate", "Whether to fully hydrate the capsule", false)
            .string("chunk_id", "Chunk ID for chunk action", false)
            .string("cursor_chunk_id", "NDJSON stream cursor chunk ID", false)
            .string_enum(
                "audience",
                "Share audience. team creates an authenticated member link; external_agent/public_link/support create token-gated links. self is valid for capsule policy but does not mint share tokens.",
                VALID_AUDIENCES,
                false,
            )
            .integer(
                "expires_in_days",
                "Share expiry in days (defaults: team=7, external_agent/public_link/support=1)",
                false,
            )
            .boolean(
                "multi_use",
                "Allow the share to be opened multiple times until expires_at. Defaults to true for team links and false for external_agent/public_link/support links. Single-use links have a ~120s grace window after first open so parallel summary+markdown reads can finish; reads after the grace return 410 Gone.",
                false,
            )
            .boolean("include_personal", "Include personal artifacts", false)
            .string_enum(
                "include_code",
                "Code inclusion mode",
                VALID_INCLUDE_CODE,
                false,
            )
            .string_enum(
                "redaction_level",
                "Redaction level",
                VALID_REDACTION_LEVELS,
                false,
            )
            .string("permissions", "Permissions for the capsule/share", false)
            .string_enum("scope", "Capsule scope", VALID_SCOPES, false)
            .uuid("workspace_id", "Workspace ID", false)
            .uuid("project_id", "Project ID", false)
            .string(
                "session_id",
                "External AI-tool session id for action=create with scope=session. Selects the owner-visible transcript for this session.",
                false,
            )
            .uuid(
                "transcript_id",
                "Exact transcript UUID for action=create with scope=session. Takes precedence over session_id.",
                false,
            )
            .string_enum("purpose", "Capsule purpose", VALID_PURPOSES, false)
            .string("name", "Capsule/share name", false)
            .string_enum(
                "mode",
                "Capsule mode. `live` (default, recommended): manifest is regenerated on every share open against current data, surfacing fresh decisions/lessons/etc. `snapshot`: frozen point-in-time copy. Snapshots are built synchronously when they complete in ≤8s; very large captures fall back to an async queue and return 202 ACCEPTED with generation_status=\"pending\" — poll the capsule until completed before sharing.",
                VALID_MODES,
                false,
            )
            .array("sections", "Explicit sections to include", "string", false)
            .string("event_kind", "Filter audit events by kind", false)
            .string_enum(
                "access_scope",
                "Filter audit events by access scope",
                VALID_ACCESS_SCOPES,
                false,
            )
            .integer("limit", "Maximum audit events or list caps (list action)", false)
            .integer("offset", "Audit result offset", false)
            .string_enum(
                "graph",
                "Graph kind for action=graph: explorer | knowledge | code",
                VALID_GRAPHS,
                false,
            )
            .integer(
                "max_uses",
                "Burn-after-N-reads cap for action=share. Team links default to no max-use cap; token-gated single-use links default to max_uses=1 with a ~120s grace window after first open.",
                false,
            )
            .integer(
                "max_inline_tokens",
                "Cap inline section tokens during action=create",
                false,
            )
            .boolean(
                "refresh_if_stale",
                "Force regenerate manifest if stale (action=create only)",
                false,
            )
            .string(
                "folder_path",
                "Absolute folder path for scope auto-resolution. When set with no project_id, the MCP client looks up indexed-projects.json (same lookup as init) to find the matching project. Prevents empty-capsule failures from guessing the wrong workspace/project id.",
                false,
            )
            .string(
                "project_name",
                "Project name for scope auto-resolution. When set with no project_id, the backend resolves it via fuzzy match within the workspace.",
                false,
            )
            .boolean(
                "allow_risky_policy",
                "On action=share with external_agent/public_link/support audience: explicitly accept the risk of leaving personal artifacts, code, weak redaction, or a >7-day expiry in the share. Default false. Without this flag, the backend enumerates each risky setting and suggests three fixes (tighten policy, switch to bootstrap_link audience, or set this flag).",
                false,
            )
            .boolean(
                "auto_install_skill",
                "On action=share: when true, the recipient's `ack` registers this share as a project-scoped Skill in their workspace so their agent surfaces it via [MATCHED_SKILLS] next time. Default false.",
                false,
            )
            .boolean(
                "require_unlock_key",
                "On action=share: gate the rich Tier-2 destinations (explorer, knowledge_graph, code_graph, chunks, stream) behind a generated revocable key. The Tier-1 overview + navigation map stay open with the link; the key is returned ONCE in the result and sent by recipients as the X-Capsule-Key header. Keyed shares default to multi-use. Default false.",
                false,
            )
            .array(
                "unlock_destinations",
                "On action=share with require_unlock_key=true: which Tier-2 destinations the key unlocks (explorer, knowledge_graph, code_graph, chunks, stream). Omit for all.",
                "string",
                false,
            )
            .array(
                "sections_read",
                "For action=ack: section ids the recipient actually read (e.g. [\"decisions\", \"lessons\"]). Surfaces in list_shares so the sender knows what the recipient focused on.",
                "string",
                false,
            )
            .string(
                "notes",
                "For action=create: free-form handoff notes included in the capsule (especially useful when no transcript exists). For action=ack: free-text note attached to the receipt.",
                false,
            )
            .array(
                "pin_items",
                "Force-include specific items or preserve external handoff references. Each entry is `{\"kind\": \"doc\"|\"event\"|\"task\"|\"decision\"|\"lesson\"|\"todo\"|\"plan\"|\"diagram\"|\"skill\"|\"reminder\"|\"preference\"|\"constraint\"|\"github_run\"|\"commit\"|\"url\", \"id\": \"<uuid-or-external-ref>\", \"title\"?: \"...\", \"url\"?: \"...\"}`. UUID ids appear in their manifest section with `pinned: true`; non-UUID ids are included in the Handoff Notes section instead of failing.",
                "object",
                false,
            )
            .array(
                "external_refs",
                "External artifacts to include in Handoff Notes without requiring ContextStream UUIDs, e.g. `[{\"kind\":\"github_run\",\"id\":\"123456\",\"url\":\"https://github.com/acme/example/actions/runs/123456\",\"title\":\"Deploy run\"}]`.",
                "object",
                false,
            )
            .string(
                "from_capsule_id",
                "For action=diff: the older capsule id to compare against (the \"from\" side of the diff).",
                false,
            )
            .string(
                "to_capsule_id",
                "For action=diff: the newer capsule id (the \"to\" side of the diff).",
                false,
            )
            .string(
                "cron",
                "For action=schedule: cron expression that regenerates the capsule on a schedule (e.g. \"0 14 * * 1-5\" for weekday 14:00 UTC). POSIX cron — 5 or 6 fields.",
                false,
            )
            .string(
                "schedule_id",
                "For action=schedule_delete: the schedule id returned by capsule(action=\"schedule\") or capsule(action=\"schedule_list\").",
                false,
            )
            .boolean(
                "refresh_shares",
                "For action=schedule: when true, the worker also re-shares the refreshed capsule with the existing share audience on each tick. Default false.",
                false,
            )
            .build()
    }
}

fn is_context_doc_format(format: Option<&str>) -> bool {
    matches!(format, Some("markdown" | "text" | "plain" | "txt"))
}

fn is_stream_format(format: Option<&str>) -> bool {
    matches!(format, Some("ndjson"))
}

fn format_share_result_text(capsule_id: &str, response: &ContextCapsuleShareResponse) -> String {
    let audience = response.audience.as_deref().unwrap_or("unknown");
    let mut lines = vec![
        format_share_headline(capsule_id, response),
        format!("Shared ContextCapsule {}.", capsule_id),
        format!("Audience: {}", audience),
    ];

    // Resolve the two URL surfaces:
    //   - share_url: web app shell URL (e.g. /c/<token>) — for humans
    //   - agent_url: API endpoint URL (e.g. /api/v1/capsules/shares/<token>)
    //                — for LLM agents to fetch JSON/markdown directly.
    // The contextstream API may not yet return `agent_url`; fall back to the
    // existing `api_url` field, which has the same semantics today.
    let share_url = response.share_url.as_deref();
    let agent_url = response
        .agent_url
        .as_deref()
        .or(response.api_url.as_deref());

    // For external_agent shares, the agent URL is the *primary* URL — humans
    // creating these shares are handing them off to an LLM, so the URL the
    // agent can fetch goes first. Other audiences (self, team, public_link,
    // support) keep the legacy ordering with the dashboard URL on top.
    if audience == "external_agent" {
        if let Some(url) = agent_url {
            lines.push(format!("Agent URL (paste into LLMs): {}", url));
        }
        if let Some(url) = share_url {
            lines.push(format!("Dashboard URL (open in browser): {}", url));
        }
        if agent_url.is_none() && share_url.is_none() {
            lines.push("URL: unavailable".to_string());
        }
    } else {
        lines.push(format!(
            "URL: {}",
            share_url.or(agent_url).unwrap_or("unavailable")
        ));
        if let Some(url) = agent_url {
            let label = if audience == "team" {
                "Agent URL (requires Authorization)"
            } else {
                "Agent URL (paste into LLMs)"
            };
            lines.push(format!("{}: {}", label, url));
        }
    }

    if let Some(key) = response.unlock_key.as_deref() {
        lines.push(format!(
            "Unlock key (returned once — give it to the trusted agent): {}",
            key
        ));
        let dests = response
            .unlock_destinations
            .as_ref()
            .filter(|d| !d.is_empty())
            .map(|d| d.join(", "))
            .unwrap_or_else(|| "all deep destinations".to_string());
        lines.push(format!(
            "It unlocks {} via the X-Capsule-Key header; revoke it anytime from the dashboard.",
            dests
        ));
    }

    if audience == "team" {
        lines.push(
            "Access: authenticated team link; recipients must sign in and have workspace or project access."
                .to_string(),
        );
    }

    if response.single_use && response.max_uses == Some(1) && response.use_count == 0 {
        lines.push("Share policy: single-use, unread (burn-after-first-open).".to_string());
        lines.push(
            "Note: ~120s grace window starts on first open — within the window any read succeeds (lets the dashboard viewer fetch summary + markdown in parallel). Reads after the grace return 410 Gone."
                .to_string(),
        );
    } else if response.single_use {
        lines.push(format!(
            "Share policy: single-use (max_uses={:?}, use_count={}, consumed_at={:?})",
            response.max_uses, response.use_count, response.consumed_at
        ));
        lines.push(
            "Note: ~120s grace window starts on first open — within the window any read succeeds. Reads after the grace return 410 Gone."
                .to_string(),
        );
    } else {
        let policy_prefix = if audience == "team" {
            "authenticated multi-use"
        } else {
            "multi-use"
        };
        lines.push(format!(
            "Share policy: {} (max_uses={:?}, use_count={})",
            policy_prefix, response.max_uses, response.use_count
        ));
    }
    lines.push(format!(
        "Warnings: {}",
        if response.warnings.is_empty() {
            "none".to_string()
        } else {
            response.warnings.join(", ")
        }
    ));
    lines.join("\n")
}

fn session_create_auto_share_audience(input: &CapsuleInput) -> Option<String> {
    if !matches!(input.scope.as_deref(), Some("session")) {
        return None;
    }
    let audience = input
        .audience
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or("external_agent")
        .to_ascii_lowercase();
    if audience == "self" {
        None
    } else {
        Some(audience)
    }
}

/// Human-facing one-liner + body for the MCP `schedule` action result.
fn format_schedule_create(value: &Value) -> String {
    let sched = value.get("data").unwrap_or(value);
    let schedule_id = sched
        .get("schedule_id")
        .and_then(Value::as_str)
        .unwrap_or("?");
    let capsule_id = sched
        .get("capsule_id")
        .and_then(Value::as_str)
        .unwrap_or("?");
    let cron = sched.get("cron").and_then(Value::as_str).unwrap_or("?");
    let next_run = sched
        .get("next_run_at")
        .and_then(Value::as_str)
        .unwrap_or("(unscheduled)");
    let refresh_shares = sched
        .get("refresh_shares")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let note = sched.get("note").and_then(Value::as_str).unwrap_or("");

    let mut lines = vec![
        format!(
            "✓ schedule registered · id={} · capsule={} · cron={:?}",
            schedule_id, capsule_id, cron
        ),
        format!("Next run: {}", next_run),
    ];
    if refresh_shares {
        lines.push("Refresh shares: yes (worker also re-shares on each tick)".to_string());
    }
    if !note.is_empty() {
        lines.push(format!("Note: {}", note));
    }
    lines.push(
        "Use capsule(action=\"schedule_list\", capsule_id=...) to see all schedules; \
         capsule(action=\"schedule_delete\", schedule_id=...) to cancel."
            .to_string(),
    );
    lines.join("\n")
}

/// Human-facing summary for `action="schedule_list"`.
fn format_schedule_list(value: &Value) -> String {
    let data = value.get("data").unwrap_or(value);
    let schedules = data
        .get("schedules")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let capsule_id = data
        .get("capsule_id")
        .and_then(Value::as_str)
        .unwrap_or("?");
    if schedules.is_empty() {
        return format!(
            "No schedules registered for capsule {}. \
             Register one with capsule(action=\"schedule\", capsule_id=\"...\", cron=\"...\").",
            capsule_id
        );
    }
    let mut lines = vec![format!(
        "Capsule {} · {} schedule(s)",
        capsule_id,
        schedules.len()
    )];
    for s in &schedules {
        let id = s.get("schedule_id").and_then(Value::as_str).unwrap_or("?");
        let cron = s.get("cron").and_then(Value::as_str).unwrap_or("?");
        let next = s.get("next_run_at").and_then(Value::as_str).unwrap_or("—");
        let last = s
            .get("last_run_at")
            .and_then(Value::as_str)
            .unwrap_or("never");
        let enabled = s.get("enabled").and_then(Value::as_bool).unwrap_or(false);
        let runs = s.get("run_count").and_then(Value::as_i64).unwrap_or(0);
        lines.push(format!(
            "  · {} · cron={:?} · enabled={} · runs={} · next={} · last={}",
            id, cron, enabled, runs, next, last
        ));
    }
    lines.join("\n")
}

/// Human-facing summary for the MCP `primer` action result.
fn format_primer_summary(value: &Value) -> String {
    let doc_id = value.get("doc_id").and_then(Value::as_str).unwrap_or("?");
    let title = value
        .get("title")
        .and_then(Value::as_str)
        .unwrap_or("primer");
    let project_id = value
        .get("project_id")
        .and_then(Value::as_str)
        .map(|s| format!("project={}", s))
        .unwrap_or_else(|| "workspace-scoped".to_string());
    let next_steps: Vec<String> = value
        .get("next_steps")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();

    let mut lines = vec![
        format!(
            "✓ primer drafted · doc_id={} · title={:?} · {}",
            doc_id, title, project_id
        ),
        "Next steps:".to_string(),
    ];
    for (i, step) in next_steps.iter().enumerate() {
        lines.push(format!("  {}. {}", i + 1, step));
    }
    lines.push(
        "Edit the doc via memory(action=\"update_doc\", doc_id=\"<above>\", content=\"…\") then re-create the capsule."
            .to_string(),
    );
    lines.join("\n")
}

/// One-line headline + bulleted summary of an ack response. Returns
/// the human-facing text for the MCP `ack` action result.
fn format_ack_summary(value: &Value) -> String {
    let acked_at = value
        .get("acked_at")
        .and_then(Value::as_str)
        .unwrap_or("just now");
    let share_id = value.get("share_id").and_then(Value::as_str).unwrap_or("?");
    let sections: Vec<String> = value
        .get("sections_read")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();
    let notes = value
        .get("notes")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty());

    let mut lines = vec![format!(
        "✓ ack recorded · share={} · {}",
        share_id, acked_at
    )];
    if !sections.is_empty() {
        lines.push(format!("Sections read: {}", sections.join(", ")));
    }
    if let Some(note) = notes {
        lines.push(format!("Notes: {}", note));
    }
    lines.push("Sender will see this in list_shares as the latest_ack for this share.".to_string());
    lines.join("\n")
}

/// Clamp the share policy to safe-by-construction defaults for
/// token-gated audiences so the backend's `enforce_share_policy_guardrails`
/// passes without the caller needing to pass `allow_risky_policy=true`.
///
/// Safe external clamps override risky caller settings unless the caller
/// explicitly passed `allow_risky_policy=true`.
fn clamp_expires_at_to_safe_external_window(value: &str) -> Option<String> {
    let parsed = DateTime::parse_from_rfc3339(value)
        .ok()?
        .with_timezone(&Utc);
    let deadline = Utc::now() + Duration::days(7);
    if parsed > deadline {
        Some(deadline.to_rfc3339())
    } else {
        Some(value.to_string())
    }
}

fn apply_safe_share_defaults(mut params: CapsuleShareParams) -> CapsuleShareParams {
    let audience = params
        .audience
        .clone()
        .unwrap_or_else(|| "external_agent".to_string());
    if params.permissions.as_deref() == Some("read") {
        params.permissions = Some("read_only".to_string());
    }
    if audience == "bootstrap_link" {
        // Public-safe primer audience. Policy is locked server-side
        // (default_policy_for_audience hardcodes redaction=strict,
        // include_personal=false, include_code=none). The share itself
        // gets a longer 14-day expiry and is multi-use by default
        // because there's no sensitive content to limit exposure of.
        if params.expires_in_days.is_none() && params.expires_at.is_none() {
            params.expires_in_days = Some(14);
        }
        if params.multi_use.is_none() {
            params.multi_use = Some(true);
        }
        if params.permissions.is_none() {
            params.permissions = Some("read_only".to_string());
        }
        params.audience = Some(audience);
        return params;
    }
    if matches!(
        audience.as_str(),
        "external_agent" | "public_link" | "support"
    ) {
        let risky_override = params.allow_risky_policy == Some(true);
        // Fill safe defaults only when the caller left a field UNSET. When the
        // caller explicitly sets a risky value, forward it untouched so the
        // backend `enforce_share_policy_guardrails` can block + enumerate it
        // (the documented allow_risky_policy contract). Previously these guards
        // (`!risky_override || …`) force-overrode explicit risky values to safe
        // ones on the default path, silently neutralizing the request and
        // reporting "Warnings: none" — bypassing the guardrail UX entirely.
        if params.include_personal.is_none() {
            params.include_personal = Some(false);
        }
        if params.include_code.is_none() {
            params.include_code = Some("none".to_string());
        }
        if params.redaction_level.is_none() {
            // Strict redaction is the safe default for token-gated
            // external links: capsule_renderer applies all PII/secret
            // regexes plus structural scrubs (emails, phone, internal
            // URLs). Callers who need looser redaction can pass standard
            // explicitly — the resulting guardrail error will tell them
            // exactly what's risky if they go too loose.
            params.redaction_level = Some("strict".to_string());
        }
        if params.permissions.is_none() {
            params.permissions = Some("read_only".to_string());
        }
        if params.expires_in_days.is_none() && params.expires_at.is_none() {
            // 3-day expiry: long enough for a real handoff cycle, short
            // enough that a leaked URL doesn't become a persistent
            // exfiltration vector. The backend's risky-policy threshold
            // is 7 days, so this stays inside the safe envelope.
            params.expires_in_days = Some(3);
        } else if !risky_override {
            if let Some(days) = params.expires_in_days {
                params.expires_in_days = Some(days.min(7));
            }
            if let Some(expires_at) = params.expires_at.as_deref() {
                if let Some(clamped) = clamp_expires_at_to_safe_external_window(expires_at) {
                    params.expires_at = Some(clamped);
                }
            }
        }
        if params.multi_use.is_none() {
            // Token-gated links default to single-use (burn-after-read)
            // with a ~120s grace window for parallel reads. Callers can
            // pass multi_use=true to opt out.
            params.multi_use = Some(false);
        }
        if params.max_uses.is_none() && params.multi_use == Some(false) {
            params.max_uses = Some(1);
        }
    } else if audience == "team" && params.permissions.is_none() {
        // Team links are authenticated and server-default to reusable, 7-day
        // links. Keep caller policy intact, but make read-only explicit.
        params.permissions = Some("read_only".to_string());
    }
    params.audience = Some(audience);
    params
}

/// Extract a friendly scope tag (e.g. `"project"`, `"workspace"`) from the
/// capsule's `scope` field, which the API may surface as either a bare string
/// or an object with `kind` / `project_id` / `workspace_id`.
fn scope_summary(scope: &Option<Value>) -> Option<String> {
    let v = scope.as_ref()?;
    if let Some(s) = v.as_str() {
        let trimmed = s.trim();
        if trimmed.is_empty() {
            return None;
        }
        return Some(trimmed.to_string());
    }
    if let Some(obj) = v.as_object() {
        if let Some(kind) = obj.get("type").and_then(|x| x.as_str()) {
            if !kind.trim().is_empty() {
                return Some(kind.trim().to_string());
            }
        }
        if let Some(kind) = obj.get("kind").and_then(|x| x.as_str()) {
            if !kind.trim().is_empty() {
                return Some(kind.trim().to_string());
            }
        }
        if obj.get("project_id").map(|v| !v.is_null()).unwrap_or(false) {
            return Some("project".to_string());
        }
        if obj
            .get("workspace_id")
            .map(|v| !v.is_null())
            .unwrap_or(false)
        {
            return Some("workspace".to_string());
        }
    }
    None
}

/// Render an `expires_at` RFC3339 timestamp as a friendly relative string
/// like `"in 7d"` / `"in 4h"` / `"expired (2d ago)"`. Falls back to the raw
/// timestamp if parsing fails, and reports `"no expiry"` when absent.
fn format_expires_humanized(expires_at: Option<&str>) -> String {
    let raw = match expires_at {
        Some(s) if !s.trim().is_empty() => s.trim(),
        _ => return "no expiry".to_string(),
    };
    let parsed = DateTime::parse_from_rfc3339(raw)
        .map(|dt| dt.with_timezone(&Utc))
        .ok();
    let Some(when) = parsed else {
        return raw.to_string();
    };
    let delta = when.signed_duration_since(Utc::now());
    let secs = delta.num_seconds();
    if secs <= 0 {
        let abs = (-secs).max(1);
        if abs >= 86_400 {
            return format!("expired ({}d ago)", abs / 86_400);
        }
        if abs >= 3600 {
            return format!("expired ({}h ago)", abs / 3600);
        }
        return format!("expired ({}m ago)", abs / 60);
    }
    if secs >= 86_400 {
        return format!("in {}d", secs / 86_400);
    }
    if secs >= 3600 {
        return format!("in {}h", secs / 3600);
    }
    format!("in {}m", (secs / 60).max(1))
}

fn total_section_items(capsule: &ContextCapsuleResponse) -> usize {
    capsule.sections.iter().filter_map(|s| s.item_count).sum()
}

/// Punchy one-line headline for `action="create"` — matches the docs format
/// `✓ {capsule_id} · scope={scope} · resolved="<name>" (<method>) · {N} sections · {M} items indexed · expires {when}`.
///
/// The `resolved=...` chip only appears when `resolved_scope` was filled
/// (either by MCP-side folder_path resolution or by the backend), and
/// the method is not `explicit` (an agent that passed `project_id`
/// already knows what it picked).
pub(crate) fn format_capsule_create_headline(capsule: &ContextCapsuleResponse) -> String {
    let mut parts = vec![format!("✓ {}", capsule.capsule_id)];
    if let Some(scope) = scope_summary(&capsule.scope) {
        parts.push(format!("scope={}", scope));
    }
    if let Some(resolved) = capsule.resolved_scope.as_ref() {
        if resolved.resolution_method != "explicit" && !resolved.resolution_method.is_empty() {
            let label = resolved
                .project_name
                .as_deref()
                .filter(|s| !s.is_empty())
                .map(|s| format!("\"{}\"", s))
                .or_else(|| resolved.project_id.map(|id| id.to_string()))
                .unwrap_or_else(|| "?".to_string());
            let confidence = if resolved.confidence > 0.0 {
                format!(", confidence={:.2}", resolved.confidence)
            } else {
                String::new()
            };
            parts.push(format!(
                "resolved={} ({}{})",
                label, resolved.resolution_method, confidence
            ));
        }
    }
    if let Some(readiness) = capsule.readiness.as_ref() {
        parts.push(format!(
            "readiness={} ({:.2})",
            readiness.label, readiness.score
        ));
    }
    parts.push(format!("{} sections", capsule.sections.len()));
    let total = total_section_items(capsule);
    if total > 0 {
        parts.push(format!("{} items indexed", total));
    }
    if capsule.expires_at.is_some() {
        parts.push(format!(
            "expires {}",
            format_expires_humanized(capsule.expires_at.as_deref())
        ));
    }
    parts.join(" · ")
}

fn format_capsule_create_result_text(
    capsule: &ContextCapsuleResponse,
    share: Option<&ContextCapsuleShareResponse>,
    auto_share_warning: Option<&str>,
) -> String {
    let mut text = format!(
        "{}\n\n{}",
        format_capsule_create_headline(capsule),
        format_capsule_summary(capsule)
    );
    if let Some(share) = share {
        text.push_str("\n\nSession capsule share links:\n");
        text.push_str(&format_share_result_text(&capsule.capsule_id, share));
    }
    if let Some(warning) = auto_share_warning {
        text.push_str("\n\n");
        text.push_str(warning);
    }
    text
}

fn capsule_create_structured(
    capsule: &ContextCapsuleResponse,
    auto_shared: Option<bool>,
    share: Option<&ContextCapsuleShareResponse>,
    auto_share_error: Option<&str>,
) -> Value {
    let mut structured = serde_json::to_value(capsule).unwrap_or_else(|_| serde_json::json!({}));
    if let Some(obj) = structured.as_object_mut() {
        if let Some(auto_shared) = auto_shared {
            obj.insert("auto_shared".to_string(), serde_json::json!(auto_shared));
        }
        if let Some(share) = share {
            obj.insert(
                "share".to_string(),
                serde_json::to_value(share).unwrap_or_default(),
            );
        }
        if let Some(error) = auto_share_error {
            obj.insert("auto_share_error".to_string(), serde_json::json!(error));
        }
    }
    structured
}

/// Punchy one-line headline for `action="open"` / `action="get"` — matches the
/// docs format `✓ opened · id={capsule_id} · scope={scope} · expires {when} · {N} sections ready`.
pub(crate) fn format_capsule_open_headline(capsule: &ContextCapsuleResponse) -> String {
    let mut parts = vec!["✓ opened".to_string()];
    if !capsule.capsule_id.is_empty() {
        parts.push(format!("id={}", capsule.capsule_id));
    }
    if let Some(scope) = scope_summary(&capsule.scope) {
        parts.push(format!("scope={}", scope));
    }
    if capsule.expires_at.is_some() {
        parts.push(format!(
            "expires {}",
            format_expires_humanized(capsule.expires_at.as_deref())
        ));
    }
    parts.push(format!("{} sections ready", capsule.sections.len()));
    parts.join(" · ")
}

/// Punchy one-line headline for `action="share"` — varies by audience. The
/// `external_agent` form leads with both the agent and dashboard URLs because
/// callers minting these shares are usually pasting them into an LLM.
pub(crate) fn format_share_headline(
    capsule_id: &str,
    response: &ContextCapsuleShareResponse,
) -> String {
    let audience = response.audience.as_deref().unwrap_or("unknown");
    let share_url = response.share_url.as_deref().unwrap_or("");
    let agent_url = response
        .agent_url
        .as_deref()
        .or(response.api_url.as_deref())
        .unwrap_or("");
    let expires = format_expires_humanized(response.expires_at.as_deref());
    match audience {
        "external_agent" => {
            let agent = if agent_url.is_empty() {
                "unavailable".to_string()
            } else {
                agent_url.to_string()
            };
            let dashboard = if share_url.is_empty() {
                "unavailable".to_string()
            } else {
                share_url.to_string()
            };
            format!("✓ Agent URL: {} · Dashboard URL: {}", agent, dashboard)
        }
        "team" => {
            let url = if share_url.is_empty() {
                "unavailable"
            } else {
                share_url
            };
            format!("✓ {} · authenticated team link · expires {}", url, expires)
        }
        _ => {
            let url = if !share_url.is_empty() {
                share_url
            } else if !agent_url.is_empty() {
                agent_url
            } else {
                "unavailable"
            };
            let policy = if response.single_use && response.use_count == 0 {
                "single-use, unread"
            } else if response.single_use {
                "single-use"
            } else {
                "multi-use"
            };
            let label = if audience == "unknown" {
                format!("ContextCapsule {}", capsule_id)
            } else {
                audience.to_string()
            };
            format!("✓ {} · {} · {} · expires {}", url, label, policy, expires)
        }
    }
}

/// Punchy one-line headline for `action="graph"` — matches the docs format
/// `✓ {N} nodes · {M} edges · returned as JSON`.
pub(crate) fn format_graph_headline(value: &Value) -> String {
    let nodes = value
        .get("nodes")
        .and_then(|v| v.as_array())
        .map(|a| a.len())
        .unwrap_or(0);
    let edges = value
        .get("edges")
        .and_then(|v| v.as_array())
        .map(|a| a.len())
        .unwrap_or(0);
    format!("✓ {} nodes · {} edges · returned as JSON", nodes, edges)
}

/// Punchy one-line headline for `action="list_shares"` — matches the docs
/// format `✓ {N} shares · {x} single-use, unread · {y} multi-use · {z} revoked`.
/// When the call wasn't capsule-scoped we add the resolved scope target so the
/// agent doesn't lose context.
pub(crate) fn format_list_shares_headline(
    shares: &[ContextCapsuleShareResponse],
    capsule_id: Option<&str>,
    project_id: Option<&str>,
    workspace_id: Option<&str>,
) -> String {
    let total = shares.len();
    let single_unread = shares
        .iter()
        .filter(|s| s.single_use && s.use_count == 0 && s.revoked_at.is_none())
        .count();
    let multi_use = shares
        .iter()
        .filter(|s| !s.single_use && s.revoked_at.is_none())
        .count();
    let revoked = shares.iter().filter(|s| s.revoked_at.is_some()).count();

    let mut parts = vec![format!("✓ {} shares", total)];
    if capsule_id.is_none() {
        let target = if let Some(project_id) = project_id {
            format!("project {}", project_id)
        } else if let Some(workspace_id) = workspace_id {
            format!("workspace {}", workspace_id)
        } else {
            "the current default scope".to_string()
        };
        parts.push(format!("for {}", target));
    }
    parts.push(format!("{} single-use, unread", single_unread));
    parts.push(format!("{} multi-use", multi_use));
    parts.push(format!("{} revoked", revoked));
    parts.join(" · ")
}

/// Friendly text for `action="revoke_share"` — matches the docs format
/// `✓ revoked · subsequent reads return 410 Gone`.
pub(crate) fn format_revoke_share_text(response: &ContextCapsuleShareResponse) -> String {
    let url = response
        .share_url
        .as_deref()
        .or(response.agent_url.as_deref())
        .or(response.api_url.as_deref())
        .filter(|s| !s.trim().is_empty());
    match url {
        Some(u) => format!(
            "✓ revoked · subsequent reads return 410 Gone\nRevoked share: {}",
            u
        ),
        None => "✓ revoked · subsequent reads return 410 Gone".to_string(),
    }
}

/// Section IDs whose contents are massive index payloads (file paths, graph
/// nodes, code chunks). Rendering thousands of those into a handoff prompt
/// is pure noise — we surface them as a single tail line in "Index summary"
/// instead. Mirrors the dashboard's `NOISE_IDS` set.
const BOOTSTRAP_NOISE_IDS: &[&str] = &["graph", "file_catalog", "code_chunks", "atlas"];

/// Sections where the items are load-bearing handoff content (decisions,
/// lessons, docs, plans, skills, diagrams). Generous per-section render
/// budget. Mirrors the dashboard's `HIGH_BUDGET_SECTIONS`.
const BOOTSTRAP_HIGH_BUDGET_IDS: &[&str] = &[
    "decisions",
    "lessons",
    "docs",
    "plans",
    "skills",
    "diagrams",
];

/// Sections with shorter, more numerous items (tasks, todos, prefs, etc.) —
/// tighter render budget. Mirrors the dashboard's `MEDIUM_BUDGET_SECTIONS`.
const BOOTSTRAP_MEDIUM_BUDGET_IDS: &[&str] = &[
    "tasks",
    "todos",
    "preferences",
    "memory_events",
    "session_snapshots",
    "reminders",
];

/// Per-section render budget for `render_bootstrap_prompt` — caps how many
/// items we materialize and how long each item's body can be.
struct BootstrapItemBudget {
    max_items: usize,
    body_chars: usize,
}

fn bootstrap_item_budget(section_id: &str) -> BootstrapItemBudget {
    if BOOTSTRAP_HIGH_BUDGET_IDS.contains(&section_id) {
        return BootstrapItemBudget {
            max_items: 25,
            body_chars: 600,
        };
    }
    if BOOTSTRAP_MEDIUM_BUDGET_IDS.contains(&section_id) {
        return BootstrapItemBudget {
            max_items: 12,
            body_chars: 350,
        };
    }
    BootstrapItemBudget {
        max_items: 6,
        body_chars: 250,
    }
}

struct RenderedBootstrapItem {
    title: Option<String>,
    body: Option<String>,
}

fn pick_string_field<'a>(
    obj: &'a serde_json::Map<String, Value>,
    keys: &[&str],
) -> Option<&'a str> {
    for key in keys {
        if let Some(s) = obj.get(*key).and_then(|v| v.as_str()) {
            let trimmed = s.trim();
            if !trimmed.is_empty() {
                return Some(trimmed);
            }
        }
    }
    None
}

fn truncate_line(text: &str, max: usize) -> String {
    let single: String = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if single.chars().count() <= max {
        return single;
    }
    let cap = max.saturating_sub(1);
    let truncated: String = single.chars().take(cap).collect();
    format!("{}…", truncated)
}

fn truncate_block(text: &str, max: usize) -> String {
    let trimmed = text.trim();
    if trimmed.chars().count() <= max {
        return trimmed.to_string();
    }
    let cap = max.saturating_sub(1);
    let truncated: String = trimmed.chars().take(cap).collect();
    format!("{}…", truncated)
}

fn render_bootstrap_items(
    data: Option<&Value>,
    budget: &BootstrapItemBudget,
) -> Vec<RenderedBootstrapItem> {
    let Some(obj) = data.and_then(|v| v.as_object()) else {
        return Vec::new();
    };
    let items_arr = obj
        .get("items")
        .and_then(|v| v.as_array())
        .or_else(|| obj.get("preview").and_then(|v| v.as_array()));
    let Some(items_arr) = items_arr else {
        return Vec::new();
    };

    const TITLE_KEYS: &[&str] = &[
        "title",
        "name",
        "summary",
        "headline",
        "objective",
        "question",
    ];
    const BODY_KEYS: &[&str] = &[
        "content",
        "details",
        "description",
        "instruction",
        "body",
        "answer",
        "rationale",
        "preview",
    ];

    let mut out: Vec<RenderedBootstrapItem> = Vec::new();
    for raw in items_arr {
        if out.len() >= budget.max_items {
            break;
        }
        let Some(item_obj) = raw.as_object() else {
            continue;
        };
        let title = pick_string_field(item_obj, TITLE_KEYS).map(|s| truncate_line(s, 200));
        let body =
            pick_string_field(item_obj, BODY_KEYS).map(|s| truncate_block(s, budget.body_chars));
        if title.is_none() && body.is_none() {
            // Last resort: emit a compact JSON dump so the row isn't silently dropped.
            let compact = serde_json::to_string(raw).unwrap_or_default();
            out.push(RenderedBootstrapItem {
                title: None,
                body: Some(truncate_block(&compact, budget.body_chars)),
            });
            continue;
        }
        out.push(RenderedBootstrapItem { title, body });
    }
    out
}

fn bootstrap_summary_text(capsule: &ContextCapsuleResponse) -> Option<String> {
    let bootstrap = capsule.bootstrap.as_ref()?;
    let llm = bootstrap.get("llm_overview");
    let candidate = llm
        .and_then(|o| o.get("summary"))
        .and_then(|v| v.as_str())
        .or_else(|| bootstrap.get("summary").and_then(|v| v.as_str()))?;
    let trimmed = candidate.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

fn bootstrap_recommended_actions(capsule: &ContextCapsuleResponse) -> Vec<String> {
    let Some(bootstrap) = capsule.bootstrap.as_ref() else {
        return Vec::new();
    };
    let llm = bootstrap.get("llm_overview");
    let arr = llm
        .and_then(|o| o.get("recommended_first_actions"))
        .and_then(|v| v.as_array())
        .or_else(|| {
            bootstrap
                .get("recommended_first_actions")
                .and_then(|v| v.as_array())
        });
    let Some(arr) = arr else { return Vec::new() };
    arr.iter()
        .filter_map(|v| v.as_str())
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
        .collect()
}

fn section_display_title(section: &mcp_types::api::ContextCapsuleSection) -> &str {
    let trimmed = section.title.trim();
    if trimmed.is_empty() {
        section.id.as_str()
    } else {
        trimmed
    }
}

/// Render a ContextCapsule into a paste-ready Markdown handoff prompt.
///
/// Mirrors the web dashboard's `renderCapsuleBody` (see
/// `dashboard-context-export-dialog.tsx`): leads with a `# title`, lifts
/// `bootstrap.summary` and `recommended_first_actions` into top-level
/// sections, then walks each narrative section emitting up to a budgeted
/// number of fully-rendered items. Massive index payloads (graph, file
/// catalog, code chunks, atlas) are summarized as one-line counts under a
/// trailing `## Index summary` heading instead of being expanded inline.
pub(crate) fn render_bootstrap_prompt(capsule: &ContextCapsuleResponse) -> String {
    let mut lines: Vec<String> = Vec::new();
    let title = capsule
        .name
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or(capsule.capsule_id.as_str());
    lines.push(format!("# {}", title));
    lines.push(String::new());

    if let Some(summary) = bootstrap_summary_text(capsule) {
        lines.push("## Summary".to_string());
        lines.push(String::new());
        lines.push(summary);
        lines.push(String::new());
    }

    let actions = bootstrap_recommended_actions(capsule);
    if !actions.is_empty() {
        lines.push("## Recommended First Actions".to_string());
        lines.push(String::new());
        for (idx, action) in actions.iter().enumerate() {
            lines.push(format!("{}. {}", idx + 1, action));
        }
        lines.push(String::new());
    }

    let narrative: Vec<&mcp_types::api::ContextCapsuleSection> = capsule
        .sections
        .iter()
        .filter(|s| {
            let count = s.item_count.unwrap_or(0);
            let chunks = s.chunk_ids.len();
            (count > 0 || chunks > 0) && !BOOTSTRAP_NOISE_IDS.contains(&s.id.as_str())
        })
        .collect();
    let noise: Vec<&mcp_types::api::ContextCapsuleSection> = capsule
        .sections
        .iter()
        .filter(|s| {
            BOOTSTRAP_NOISE_IDS.contains(&s.id.as_str())
                && (s.item_count.unwrap_or(0) > 0 || !s.chunk_ids.is_empty())
        })
        .collect();

    if !narrative.is_empty() {
        lines.push("## Sections".to_string());
        lines.push(String::new());
        for section in &narrative {
            lines.push(format!("### {}", section_display_title(section)));
            lines.push(String::new());

            let summary = section.summary.trim();
            if !summary.is_empty() {
                lines.push(summary.to_string());
                lines.push(String::new());
            }

            let mut meta: Vec<String> = Vec::new();
            if let Some(c) = section.item_count {
                meta.push(format!("Items: {}", c));
            }
            if !section.chunk_ids.is_empty() {
                meta.push(format!("Chunks: {}", section.chunk_ids.len()));
            }
            if !meta.is_empty() {
                lines.push(meta.join("  ·  "));
                lines.push(String::new());
            }

            let budget = bootstrap_item_budget(section.id.as_str());
            let items = render_bootstrap_items(section.data.as_ref(), &budget);
            for item in &items {
                if let Some(t) = item.title.as_deref() {
                    lines.push(format!("#### {}", t));
                    lines.push(String::new());
                }
                if let Some(b) = item.body.as_deref() {
                    lines.push(b.to_string());
                    lines.push(String::new());
                }
            }
            if let Some(total) = section.item_count {
                if total > items.len() && !items.is_empty() {
                    lines.push(format!(
                        "_…{} more — fetch the full capsule via the share link or curl to see all items._",
                        total - items.len()
                    ));
                    lines.push(String::new());
                }
            }
        }
    }

    if !noise.is_empty() {
        lines.push("## Index summary".to_string());
        lines.push(String::new());
        for section in &noise {
            let count = section.item_count.unwrap_or(0);
            let chunks = section.chunk_ids.len();
            let mut parts: Vec<String> = Vec::new();
            if count > 0 {
                parts.push(format!("{} items", count));
            }
            if chunks > 0 {
                parts.push(format!("{} chunks", chunks));
            }
            lines.push(format!(
                "- **{}** — {}",
                section_display_title(section),
                parts.join(", ")
            ));
        }
        lines.push(String::new());
    }

    let mut joined = lines.join("\n");
    let trimmed_len = joined.trim_end().len();
    joined.truncate(trimmed_len);
    joined.push('\n');
    joined
}

/// Punchy one-line headline for `action="bootstrap_prompt"` — leads with a
/// paste-ready summary so agents can decide at a glance whether to forward
/// the prompt or hydrate more sections first.
pub(crate) fn format_bootstrap_prompt_headline(
    capsule: &ContextCapsuleResponse,
    prompt: &str,
) -> String {
    let chars = prompt.chars().count();
    let tokens = chars / 4;
    let rendered_sections = capsule
        .sections
        .iter()
        .filter(|s| {
            let count = s.item_count.unwrap_or(0);
            let chunks = s.chunk_ids.len();
            (count > 0 || chunks > 0) && !BOOTSTRAP_NOISE_IDS.contains(&s.id.as_str())
        })
        .count();
    let mut parts = vec!["✓ bootstrap prompt".to_string()];
    if !capsule.capsule_id.is_empty() {
        parts.push(format!("id={}", capsule.capsule_id));
    }
    parts.push(format!("{} sections", rendered_sections));
    parts.push(format!("~{} tokens", tokens));
    parts.push(format!("{} chars", chars));
    parts.join(" · ")
}

fn format_capsule_summary(capsule: &ContextCapsuleResponse) -> String {
    let title = capsule
        .name
        .as_deref()
        .unwrap_or(capsule.capsule_id.as_str());
    let bootstrap = capsule.bootstrap.as_ref();

    let llm = bootstrap.and_then(|b| b.get("llm_overview"));
    let bootstrap_summary = llm
        .and_then(|o| o.get("summary"))
        .and_then(|v| v.as_str())
        .or_else(|| {
            bootstrap
                .and_then(|b| b.get("summary"))
                .and_then(|v| v.as_str())
        })
        .unwrap_or("No bootstrap summary available.");

    let recommended: Vec<&str> = llm
        .and_then(|o| o.get("recommended_first_actions"))
        .and_then(|v| v.as_array())
        .map(|items| items.iter().filter_map(|x| x.as_str()).collect())
        .or_else(|| {
            bootstrap
                .and_then(|b| b.get("recommended_first_actions"))
                .and_then(|v| v.as_array())
                .map(|items| items.iter().filter_map(|x| x.as_str()).collect())
        })
        .unwrap_or_default();

    let section_summaries: std::collections::HashMap<String, String> = llm
        .and_then(|o| o.get("section_summaries"))
        .and_then(|v| v.as_object())
        .map(|obj| {
            obj.iter()
                .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
                .collect()
        })
        .unwrap_or_default();

    let share_url = capsule
        .links
        .share_url
        .as_deref()
        .filter(|s| !s.is_empty())
        .unwrap_or("—");
    let expires = capsule
        .expires_at
        .as_deref()
        .filter(|s| !s.is_empty())
        .unwrap_or("no expiry");

    let redaction_level = capsule
        .policy
        .redaction_level
        .as_deref()
        .unwrap_or("unknown");
    let redaction_counts = capsule
        .redaction_summary
        .as_ref()
        .and_then(|r| r.get("counts"))
        .map(|c| c.to_string())
        .unwrap_or_else(|| "{}".to_string());

    let mut lines = vec![
        format!(
            "Opened ContextCapsule: {} (purpose: {}, mode: {})",
            title,
            capsule.purpose.as_deref().unwrap_or("unknown"),
            capsule.mode.as_deref().unwrap_or("unknown")
        ),
        format!("Share URL: {}", share_url),
        format!("Expires: {}", expires),
        format!(
            "Redaction: {} (counts: {})",
            redaction_level, redaction_counts
        ),
        String::new(),
        "Bootstrap summary:".to_string(),
        bootstrap_summary.to_string(),
    ];

    if !recommended.is_empty() {
        lines.push(String::new());
        lines.push("Recommended first actions:".to_string());
        for (index, item) in recommended.iter().enumerate() {
            lines.push(format!("{}. {}", index + 1, item));
        }
    }

    lines.push(String::new());
    lines.push("Sections:".to_string());
    let mut needs_chunk = false;
    for section in &capsule.sections {
        let has_inline = section
            .data
            .as_ref()
            .map(|d| !d.is_null() && !d.as_object().map(|o| o.is_empty()).unwrap_or(false))
            .unwrap_or(false);
        let lazy = !section.chunk_ids.is_empty() && !has_inline;
        if lazy {
            needs_chunk = true;
        }
        let mode = if lazy {
            "lazy (fetch via action=chunk)"
        } else if has_inline {
            "inline"
        } else {
            "metadata"
        };
        let tok = section
            .inline_tokens
            .map(|t| format!("{} tok", t))
            .unwrap_or_else(|| "—".to_string());
        lines.push(format!(
            "- {}: {} items, {} chunk id(s), {}, {}",
            section.id,
            section.item_count.unwrap_or(0),
            section.chunk_ids.len(),
            mode,
            tok
        ));
        if let Some(sum) = section_summaries.get(&section.id) {
            lines.push(format!("  Section note: {}", sum));
        }
    }

    lines.push(String::new());
    lines.push("Deep dives (dashboard, auth required):".to_string());
    lines.push(format!(
        "  Project explorer: {}",
        capsule.links.project_explorer_url.as_deref().unwrap_or("—")
    ));
    lines.push(format!(
        "  Knowledge graph: {}",
        capsule.links.knowledge_graph_url.as_deref().unwrap_or("—")
    ));
    lines.push(format!(
        "  Code graph: {}",
        capsule.links.code_graph_url.as_deref().unwrap_or("—")
    ));

    lines.push(String::new());
    lines.push("Next steps:".to_string());
    if needs_chunk {
        lines.push("  - Lazy sections: capsule(action=\"chunk\", capsule_id|share_token|url=..., chunk_id=\"<id from section>\")".to_string());
    }
    lines.push("  - Full NDJSON stream: capsule(action=\"stream\", ...)".to_string());
    lines.push("  - Markdown/text body: capsule(action=\"context_doc\", format=\"markdown\"|\"text\", ...)".to_string());
    if share_url != "—" {
        lines.push("  - Share-token JSON graphs: capsule(action=\"graph\", graph=\"explorer|knowledge|code\", share_token=... or url=...)".to_string());
    }
    lines.join("\n")
}

fn format_list_capsules_summary(capsules: &[ContextCapsuleResponse]) -> String {
    if capsules.is_empty() {
        return "Found 0 ContextCapsule(s) in scope.".to_string();
    }
    let mut lines = vec![format!(
        "Found {} ContextCapsule(s) in scope:",
        capsules.len()
    )];
    for c in capsules {
        lines.push(format!(
            "- {} | purpose={} | mode={} | created={} | fingerprint={}",
            c.name.as_deref().unwrap_or(&c.capsule_id),
            c.purpose.as_deref().unwrap_or("?"),
            c.mode.as_deref().unwrap_or("?"),
            c.created_at.as_deref().unwrap_or("?"),
            c.fingerprint.as_deref().unwrap_or("?")
        ));
    }
    lines.join("\n")
}

fn format_graph_summary(value: &Value) -> String {
    let schema = value
        .get("schema")
        .and_then(|v| v.as_str())
        .unwrap_or("unknown");
    let project_id = value
        .get("project_id")
        .map(|v| v.to_string())
        .unwrap_or_else(|| "—".to_string());
    let nodes = value
        .get("nodes")
        .and_then(|v| v.as_array())
        .map(|a| a.len())
        .unwrap_or(0);
    let edges = value
        .get("edges")
        .and_then(|v| v.as_array())
        .map(|a| a.len())
        .unwrap_or(0);

    let mut type_counts: std::collections::BTreeMap<String, usize> =
        std::collections::BTreeMap::new();
    if let Some(arr) = value.get("nodes").and_then(|v| v.as_array()) {
        for n in arr {
            let t = n
                .get("type")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown")
                .to_string();
            *type_counts.entry(t).or_insert(0) += 1;
        }
    }
    let mut edge_type_counts: std::collections::BTreeMap<String, usize> =
        std::collections::BTreeMap::new();
    if let Some(arr) = value.get("edges").and_then(|v| v.as_array()) {
        for e in arr {
            let t = e
                .get("type")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown")
                .to_string();
            *edge_type_counts.entry(t).or_insert(0) += 1;
        }
    }

    let meta = value.get("metadata").cloned().unwrap_or(Value::Null);

    let lines = vec![
        format_graph_headline(value),
        format!("ContextCapsule graph: schema={}", schema),
        format!("project_id: {}", project_id),
        format!("nodes: {} total", nodes),
        format!("edges: {} total", edges),
        format!("node types: {:?}", type_counts),
        format!("edge types: {:?}", edge_type_counts),
        "metadata:".to_string(),
        meta.to_string(),
    ];
    lines.join("\n")
}

fn format_chunk_summary(chunk: &ContextCapsuleChunkResponse) -> String {
    format!(
        "Fetched chunk {} from section {} ({} item(s)).",
        chunk.chunk_id,
        chunk.section_id,
        chunk.item_count.unwrap_or(0)
    )
}

fn format_stream_summary(ndjson: &str) -> String {
    let line_count = ndjson.lines().count();
    let chunk_count = ndjson
        .lines()
        .filter(|line| line.contains(r#""kind":"chunk""#))
        .count();
    format!(
        "Fetched ContextCapsule stream with {} line(s) and {} chunk record(s).",
        line_count, chunk_count
    )
}

fn format_audit_summary(
    events: &[ContextCapsuleAuditEventResponse],
    capsule_id: Option<&str>,
) -> String {
    let mut kinds: Vec<&str> = events.iter().map(|e| e.event_kind.as_str()).collect();
    kinds.sort_unstable();
    kinds.dedup();
    let target = capsule_id.unwrap_or("requested scope");
    if kinds.is_empty() {
        format!("Found 0 ContextCapsule audit event(s) for {}.", target)
    } else {
        format!(
            "Found {} ContextCapsule audit event(s) for {}.\nDistinct event_kind values: {}.\n(Common kinds include chunk, stream, share, consumed, revoke_share, render_markdown, render_text.)",
            events.len(),
            target,
            kinds.join(", ")
        )
    }
}

fn format_list_shares_summary(
    shares: &[ContextCapsuleShareResponse],
    capsule_id: Option<&str>,
    project_id: Option<&str>,
    workspace_id: Option<&str>,
) -> String {
    let target = if let Some(capsule_id) = capsule_id {
        format!("ContextCapsule {}", capsule_id)
    } else if let Some(project_id) = project_id {
        format!("project {}", project_id)
    } else if let Some(workspace_id) = workspace_id {
        format!("workspace {}", workspace_id)
    } else {
        "the current default scope".to_string()
    };

    let mut lines = vec![
        format_list_shares_headline(shares, capsule_id, project_id, workspace_id),
        format!("Found {} share(s) for {}.", shares.len(), target),
    ];
    for s in shares {
        let policy = if s.single_use && s.max_uses == Some(1) && s.use_count == 0 {
            "single-use, unread"
        } else if s.single_use {
            "single-use"
        } else {
            "multi-use"
        };
        lines.push(format!(
            "- id={} | audience={} | policy={} | max_uses={:?} | use_count={} | consumed_at={:?} | expires_at={:?} | token_prefix={}",
            s.id,
            s.audience.as_deref().unwrap_or("?"),
            policy,
            s.max_uses,
            s.use_count,
            s.consumed_at,
            s.expires_at,
            s.token_prefix
        ));
    }
    lines.join("\n")
}

pub fn register_capsule_tools(
    registry: &mut crate::registry::ToolRegistry,
    client: ContextStreamClient,
) {
    let atlas_layer = registry.atlas_layer().clone();
    registry.register(
        "capsule",
        Arc::new(CapsuleTool::with_atlas(client, atlas_layer)),
    );
}

#[cfg(test)]
#[path = "capsule_tests.rs"]
mod tests;
