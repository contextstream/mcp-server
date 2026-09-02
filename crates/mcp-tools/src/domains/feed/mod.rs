//! Context Feeds: curated workspace, project, and topic activity streams.
//!
//! A single `feed` tool fronts `/api/v1/feeds`. Reads render compact `[FEED]`
//! lines (the structured payload rides alongside), writes are idempotent (an
//! idempotency key is generated when the caller omits one), and agent posts
//! always carry provenance so humans can tell who wrote what.

mod format;
mod schema;
#[cfg(test)]
mod tests;

pub use format::{
    format_feed_grounding, grounding_items, GROUNDING_MAX_CHARS, GROUNDING_MAX_ITEMS,
};

use async_trait::async_trait;
use mcp_client::{
    ContextStreamClient, FeedCreateParams, FeedFollowParams, FeedItemsParams, FeedListParams,
    FeedPostParams, FeedShareParams, FeedSourceParams, FeedUpdateParams, FEED_GROUNDING_MAX_ITEMS,
    FEED_MAX_PAGE_SIZE,
};
use mcp_session::SessionManager;
use mcp_types::{
    tool::{ToolMetadata, ToolResult},
    Error, Result,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::sync::Arc;
use uuid::Uuid;

use crate::domains::session::deserialize_string_or_vec;
use crate::registry::ToolHandler;

const VALID_ACTIONS: &[&str] = &[
    "list", "ensure", "get", "update", "archive", "items", "post", "follow", "unfollow", "read",
    "share", "unshare", "feedback", "curate", "runs", "sources", "ground",
];
const VALID_KINDS: &[&str] = &["workspace", "project", "topic"];
const VALID_INCLUDES: &[&str] = &["owned", "shared", "all"];
const VALID_VIEWS: &[&str] = &["latest", "unread", "posts", "top"];
const VALID_AUTHOR_KINDS: &[&str] = &["human", "agent"];
const VALID_FEEDBACK_TYPES: &[&str] = &["positive", "dismiss", "hard_ignore", "not_relevant"];
const VALID_AUDIENCES: &[&str] = &["agents", "everyone"];
const VALID_ORIGINS: &[&str] = &["explicit", "excluded"];

/// `provenance.product` recorded on every post published through this tool.
pub const POST_PROVENANCE_PRODUCT: &str = "contextstream-mcp";
/// `provenance.source` recorded on every post published through this tool.
pub const POST_PROVENANCE_SOURCE: &str = "feed_tool";

const DEFAULT_PAGE_SIZE: u16 = 20;
const DEFAULT_GROUNDING_ITEMS: u16 = 5;

/// Input accepted by the `feed` tool.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct FeedInput {
    pub action: String,
    pub workspace_id: Option<String>,
    pub project_id: Option<String>,
    pub feed_id: Option<String>,
    pub kind: Option<String>,
    pub name: Option<String>,
    pub description: Option<String>,
    pub topic_spec: Option<Value>,
    pub curation_settings: Option<Value>,
    pub include: Option<String>,
    pub include_archived: Option<bool>,
    pub view: Option<String>,
    pub cursor: Option<i64>,
    pub limit: Option<i64>,
    pub since: Option<String>,
    pub item_id: Option<String>,
    pub title: Option<String>,
    pub content: Option<String>,
    #[serde(default, deserialize_with = "deserialize_string_or_vec")]
    pub tags: Option<Vec<String>>,
    pub author_kind: Option<String>,
    pub feedback_type: Option<String>,
    pub pinned_to_sidebar: Option<bool>,
    pub muted_until: Option<String>,
    pub digest_frequency: Option<String>,
    pub last_read_sequence: Option<i64>,
    pub target_workspace_id: Option<String>,
    pub target_project_id: Option<String>,
    pub audience: Option<String>,
    pub share_id: Option<String>,
    pub source_workspace_id: Option<String>,
    pub source_project_id: Option<String>,
    pub origin: Option<String>,
    pub source_key: Option<String>,
    pub query: Option<String>,
    pub expected_revision: Option<i64>,
    pub idempotency_key: Option<String>,
}

/// The `feed` tool.
pub struct FeedTool {
    client: ContextStreamClient,
    session: Arc<SessionManager>,
}

fn parse_optional_uuid(value: &Option<String>, field: &str) -> Result<Option<Uuid>> {
    match value
        .as_deref()
        .map(str::trim)
        .filter(|raw| !raw.is_empty())
    {
        Some(raw) => Uuid::parse_str(raw)
            .map(Some)
            .map_err(|_| Error::Validation(format!("Invalid {field}: expected a UUID"))),
        None => Ok(None),
    }
}

fn require_uuid(value: &Option<String>, field: &str, action: &str) -> Result<Uuid> {
    parse_optional_uuid(value, field)?
        .ok_or_else(|| Error::Validation(format!("{field} is required for {action}")))
}

fn require_text(value: &Option<String>, field: &str, action: &str) -> Result<String> {
    value
        .as_deref()
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .map(str::to_string)
        .ok_or_else(|| Error::Validation(format!("{field} is required for {action}")))
}

fn validate_choice(value: Option<&str>, field: &str, allowed: &[&str]) -> Result<Option<String>> {
    match value.map(str::trim).filter(|raw| !raw.is_empty()) {
        None => Ok(None),
        Some(raw) => {
            let normalized = raw.to_ascii_lowercase();
            if allowed.contains(&normalized.as_str()) {
                Ok(Some(normalized))
            } else {
                Err(Error::Validation(format!(
                    "Invalid {field}: {raw}. Expected one of: {}",
                    allowed.join(", ")
                )))
            }
        }
    }
}

fn page_limit(limit: Option<i64>) -> u16 {
    limit
        .and_then(|value| u16::try_from(value).ok())
        .map(|value| value.clamp(1, FEED_MAX_PAGE_SIZE))
        .unwrap_or(DEFAULT_PAGE_SIZE)
}

fn ground_limit(limit: Option<i64>) -> u16 {
    limit
        .and_then(|value| u16::try_from(value).ok())
        .map(|value| value.clamp(1, FEED_GROUNDING_MAX_ITEMS))
        .unwrap_or(DEFAULT_GROUNDING_ITEMS)
}

fn cursor(cursor: Option<i64>) -> Option<u32> {
    cursor
        .and_then(|value| u32::try_from(value).ok())
        .filter(|value| *value > 0)
}

fn idempotency_key(explicit: &Option<String>) -> String {
    explicit
        .as_deref()
        .map(str::trim)
        .filter(|key| !key.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| format!("mcp-feed-{}", Uuid::new_v4()))
}

fn workspace_required(workspace_id: Option<Uuid>, action: &str) -> Result<Uuid> {
    workspace_id.ok_or_else(|| {
        Error::Validation(format!(
            "workspace_id is required for {action}. Run init(folder_path=\"...\") or pass workspace_id."
        ))
    })
}

/// Provenance object attached to agent posts.
pub fn post_provenance() -> Value {
    serde_json::json!({
        "product": POST_PROVENANCE_PRODUCT,
        "source": POST_PROVENANCE_SOURCE,
    })
}

impl FeedTool {
    pub fn new(client: ContextStreamClient, session: Arc<SessionManager>) -> Self {
        Self { client, session }
    }

    /// Explicit ids win; otherwise borrow the session's active scope. The
    /// session project is only applied when the workspace is the session's
    /// own, so an explicit foreign workspace never pairs with a stale project.
    async fn resolve_scope(&self, input: &FeedInput) -> Result<(Option<Uuid>, Option<Uuid>)> {
        let explicit_workspace = parse_optional_uuid(&input.workspace_id, "workspace_id")?;
        let explicit_project = parse_optional_uuid(&input.project_id, "project_id")?;
        if explicit_workspace.is_some() && explicit_project.is_some() {
            return Ok((explicit_workspace, explicit_project));
        }
        let state = self.session.state().await;
        let workspace_id = explicit_workspace.or(state.workspace_id);
        let same_workspace = explicit_workspace.is_none()
            || (state.workspace_id.is_some() && explicit_workspace == state.workspace_id);
        let project_id = explicit_project.or(if same_workspace {
            state.project_id
        } else {
            None
        });
        Ok((workspace_id, project_id))
    }

    /// Resolve `feed_id`, falling back to the canonical feed of the scope for
    /// non-destructive actions.
    async fn resolve_feed_id(
        &self,
        input: &FeedInput,
        workspace_id: Option<Uuid>,
        project_id: Option<Uuid>,
        action: &str,
    ) -> Result<Uuid> {
        if let Some(feed_id) = parse_optional_uuid(&input.feed_id, "feed_id")? {
            return Ok(feed_id);
        }
        let kind = if project_id.is_some() {
            "project"
        } else {
            "workspace"
        };
        let workspace_id = workspace_required(workspace_id, action)?;
        let feed = self
            .client
            .feed_ensure(Some(workspace_id), project_id, kind)
            .await?;
        format::feed_id(&feed).ok_or_else(|| {
            Error::Validation(format!(
                "feed_id is required for {action}; the canonical {kind} feed could not be resolved"
            ))
        })
    }

    async fn list(
        &self,
        input: &FeedInput,
        ws: Option<Uuid>,
        proj: Option<Uuid>,
    ) -> Result<ToolResult> {
        let include = validate_choice(input.include.as_deref(), "include", VALID_INCLUDES)?
            .unwrap_or_else(|| "all".to_string());
        let result = self
            .client
            .feeds_list(FeedListParams {
                workspace_id: ws,
                project_id: proj,
                include: Some(include.clone()),
                include_archived: input.include_archived,
                cursor: cursor(input.cursor),
                limit: Some(page_limit(input.limit)),
            })
            .await?;
        Ok(ToolResult::with_structured(
            format::format_feed_list(&result, &include),
            result,
        ))
    }

    async fn ensure(
        &self,
        input: &FeedInput,
        ws: Option<Uuid>,
        proj: Option<Uuid>,
    ) -> Result<ToolResult> {
        let kind =
            validate_choice(input.kind.as_deref(), "kind", VALID_KINDS)?.unwrap_or_else(|| {
                if proj.is_some() {
                    "project"
                } else {
                    "workspace"
                }
                .to_string()
            });
        let workspace_id = workspace_required(ws, "ensure")?;
        if kind == "topic" {
            let name = require_text(&input.name, "name", "ensure kind=topic")?;
            let result = self
                .client
                .feed_create(FeedCreateParams {
                    workspace_id: Some(workspace_id),
                    project_id: proj,
                    kind,
                    name,
                    description: input.description.clone(),
                    topic_spec: input.topic_spec.clone(),
                    curation_settings: input.curation_settings.clone(),
                    idempotency_key: idempotency_key(&input.idempotency_key),
                })
                .await?;
            return Ok(ToolResult::with_structured(
                format::format_feed(&result, "Topic feed created."),
                result,
            ));
        }
        if kind == "project" && proj.is_none() {
            return Err(Error::Validation(
                "project_id is required for ensure kind=project. Run init(folder_path=\"...\") or pass project_id.".to_string(),
            ));
        }
        let result = self
            .client
            .feed_ensure(Some(workspace_id), proj, &kind)
            .await?;
        Ok(ToolResult::with_structured(
            format::format_feed(&result, &format!("Canonical {kind} feed ready.")),
            result,
        ))
    }

    async fn get(
        &self,
        input: &FeedInput,
        ws: Option<Uuid>,
        proj: Option<Uuid>,
    ) -> Result<ToolResult> {
        let feed_id = self.resolve_feed_id(input, ws, proj, "get").await?;
        if let Some(item_id) = parse_optional_uuid(&input.item_id, "item_id")? {
            let result = self.client.feed_item(feed_id, item_id).await?;
            return Ok(ToolResult::with_structured(
                format::format_item_detail(&result),
                result,
            ));
        }
        let result = self.client.feed_get(feed_id).await?;
        Ok(ToolResult::with_structured(
            format::format_feed(&result, "Feed."),
            result,
        ))
    }

    async fn update(&self, input: &FeedInput) -> Result<ToolResult> {
        let feed_id = require_uuid(&input.feed_id, "feed_id", "update")?;
        let expected_revision = input.expected_revision.ok_or_else(|| {
            Error::Validation(
                "expected_revision is required for update (read it from feed(action=\"get\"))"
                    .to_string(),
            )
        })?;
        if input.name.is_none()
            && input.description.is_none()
            && input.topic_spec.is_none()
            && input.curation_settings.is_none()
        {
            return Err(Error::Validation(
                "update needs at least one of name, description, topic_spec, curation_settings"
                    .to_string(),
            ));
        }
        let result = self
            .client
            .feed_update(
                feed_id,
                FeedUpdateParams {
                    name: input.name.clone(),
                    description: input.description.clone(),
                    topic_spec: input.topic_spec.clone(),
                    curation_settings: input.curation_settings.clone(),
                    expected_revision,
                    idempotency_key: idempotency_key(&input.idempotency_key),
                },
            )
            .await?;
        Ok(ToolResult::with_structured(
            format::format_feed(&result, "Feed updated."),
            result,
        ))
    }

    async fn archive(&self, input: &FeedInput) -> Result<ToolResult> {
        let feed_id = require_uuid(&input.feed_id, "feed_id", "archive")?;
        let expected_revision = input.expected_revision.ok_or_else(|| {
            Error::Validation(
                "expected_revision is required for archive (read it from feed(action=\"get\"))"
                    .to_string(),
            )
        })?;
        let result = self
            .client
            .feed_archive(
                feed_id,
                expected_revision,
                &idempotency_key(&input.idempotency_key),
            )
            .await?;
        Ok(ToolResult::with_structured(
            format::format_feed(&result, "Feed archived."),
            result,
        ))
    }

    async fn items(
        &self,
        input: &FeedInput,
        ws: Option<Uuid>,
        proj: Option<Uuid>,
    ) -> Result<ToolResult> {
        let feed_id = self.resolve_feed_id(input, ws, proj, "items").await?;
        let view = validate_choice(input.view.as_deref(), "view", VALID_VIEWS)?
            .unwrap_or_else(|| "latest".to_string());
        let params = FeedItemsParams {
            view: Some(view.clone()),
            cursor: cursor(input.cursor),
            limit: Some(page_limit(input.limit)),
            since: input.since.clone(),
        };
        let (feed, page) = tokio::join!(
            self.client.feed_get(feed_id),
            self.client.feed_items(feed_id, params)
        );
        let page = page?;
        let feed = feed.ok();
        let text = format::format_items(feed.as_ref(), &page, feed_id, &view);
        let structured = serde_json::json!({
            "feed": feed,
            "feed_id": feed_id,
            "view": view,
            "page": page,
        });
        Ok(ToolResult::with_structured(text, structured))
    }

    async fn post(
        &self,
        input: &FeedInput,
        ws: Option<Uuid>,
        proj: Option<Uuid>,
    ) -> Result<ToolResult> {
        let title = require_text(&input.title, "title", "post")?;
        let content = require_text(&input.content, "content", "post")?;
        let author_kind = validate_choice(
            input.author_kind.as_deref(),
            "author_kind",
            VALID_AUTHOR_KINDS,
        )?
        .unwrap_or_else(|| "agent".to_string());
        let feed_id = self.resolve_feed_id(input, ws, proj, "post").await?;
        let result = self
            .client
            .feed_post(
                feed_id,
                FeedPostParams {
                    title,
                    content,
                    tags: input.tags.clone().unwrap_or_default(),
                    // Only an explicit project scopes the post; the session
                    // project may belong to a different feed.
                    project_id: parse_optional_uuid(&input.project_id, "project_id")?,
                    author_kind,
                    idempotency_key: idempotency_key(&input.idempotency_key),
                    provenance: Some(post_provenance()),
                },
            )
            .await?;
        Ok(ToolResult::with_structured(
            format::format_post(&result),
            result,
        ))
    }

    async fn follow(
        &self,
        input: &FeedInput,
        ws: Option<Uuid>,
        proj: Option<Uuid>,
    ) -> Result<ToolResult> {
        let feed_id = self.resolve_feed_id(input, ws, proj, "follow").await?;
        let result = self
            .client
            .feed_follow(
                feed_id,
                FeedFollowParams {
                    pinned_to_sidebar: input.pinned_to_sidebar,
                    muted_until: input.muted_until.clone(),
                    digest_frequency: input.digest_frequency.clone(),
                },
            )
            .await?;
        Ok(ToolResult::with_structured(
            format::format_follow_state(&result, feed_id, "Following"),
            result,
        ))
    }

    async fn unfollow(&self, input: &FeedInput) -> Result<ToolResult> {
        let feed_id = require_uuid(&input.feed_id, "feed_id", "unfollow")?;
        let result = self.client.feed_unfollow(feed_id).await?;
        Ok(ToolResult::with_structured(
            format::format_follow_state(&result, feed_id, "Unfollowed"),
            result,
        ))
    }

    async fn read(
        &self,
        input: &FeedInput,
        ws: Option<Uuid>,
        proj: Option<Uuid>,
    ) -> Result<ToolResult> {
        let feed_id = self.resolve_feed_id(input, ws, proj, "read").await?;
        let last_read_sequence = match input.last_read_sequence {
            Some(sequence) if sequence >= 0 => sequence,
            Some(_) => {
                return Err(Error::Validation(
                    "last_read_sequence must be zero or positive".to_string(),
                ))
            }
            None => {
                let feed = self.client.feed_get(feed_id).await?;
                format::i64_field(format::payload(&feed), "latest_sequence").unwrap_or(0)
            }
        };
        let result = self.client.feed_read(feed_id, last_read_sequence).await?;
        Ok(ToolResult::with_structured(
            format::format_follow_state(&result, feed_id, "Marked read"),
            result,
        ))
    }

    async fn share(&self, input: &FeedInput) -> Result<ToolResult> {
        let feed_id = require_uuid(&input.feed_id, "feed_id", "share")?;
        let Some(target_workspace_id) =
            parse_optional_uuid(&input.target_workspace_id, "target_workspace_id")?
        else {
            let result = self.client.feed_shares(feed_id).await?;
            return Ok(ToolResult::with_structured(
                format::format_shares(&result, feed_id),
                result,
            ));
        };
        let audience = validate_choice(input.audience.as_deref(), "audience", VALID_AUDIENCES)?;
        let result = self
            .client
            .feed_share(
                feed_id,
                FeedShareParams {
                    target_workspace_id,
                    target_project_id: parse_optional_uuid(
                        &input.target_project_id,
                        "target_project_id",
                    )?,
                    audience,
                    idempotency_key: idempotency_key(&input.idempotency_key),
                },
            )
            .await?;
        Ok(ToolResult::with_structured(
            format::format_share(&result, "Shared"),
            result,
        ))
    }

    async fn unshare(&self, input: &FeedInput) -> Result<ToolResult> {
        let feed_id = require_uuid(&input.feed_id, "feed_id", "unshare")?;
        let share_id = require_uuid(&input.share_id, "share_id", "unshare")?;
        let result = self.client.feed_unshare(feed_id, share_id).await?;
        Ok(ToolResult::with_structured(
            format::format_share(&result, "Revoked"),
            result,
        ))
    }

    async fn feedback(&self, input: &FeedInput) -> Result<ToolResult> {
        let feed_id = require_uuid(&input.feed_id, "feed_id", "feedback")?;
        let item_id = require_uuid(&input.item_id, "item_id", "feedback")?;
        let feedback_type = validate_choice(
            input.feedback_type.as_deref(),
            "feedback_type",
            VALID_FEEDBACK_TYPES,
        )?
        .ok_or_else(|| Error::Validation("feedback_type is required for feedback".to_string()))?;
        let result = self
            .client
            .feed_feedback(feed_id, item_id, &feedback_type, None)
            .await?;
        Ok(ToolResult::with_structured(
            format::format_feedback(&result, feed_id),
            result,
        ))
    }

    async fn curate(
        &self,
        input: &FeedInput,
        ws: Option<Uuid>,
        proj: Option<Uuid>,
    ) -> Result<ToolResult> {
        let feed_id = self.resolve_feed_id(input, ws, proj, "curate").await?;
        let result = self.client.feed_curate(feed_id).await?;
        Ok(ToolResult::with_structured(
            format::format_curation(&result, feed_id),
            result,
        ))
    }

    async fn runs(
        &self,
        input: &FeedInput,
        ws: Option<Uuid>,
        proj: Option<Uuid>,
    ) -> Result<ToolResult> {
        let feed_id = self.resolve_feed_id(input, ws, proj, "runs").await?;
        let result = self
            .client
            .feed_runs(feed_id, Some(page_limit(input.limit)))
            .await?;
        Ok(ToolResult::with_structured(
            format::format_runs(&result, feed_id),
            result,
        ))
    }

    async fn sources(&self, input: &FeedInput) -> Result<ToolResult> {
        let feed_id = require_uuid(&input.feed_id, "feed_id", "sources")?;
        if let Some(source_workspace_id) =
            parse_optional_uuid(&input.source_workspace_id, "source_workspace_id")?
        {
            let origin = validate_choice(input.origin.as_deref(), "origin", VALID_ORIGINS)?;
            let result = self
                .client
                .feed_source_add(
                    feed_id,
                    FeedSourceParams {
                        source_workspace_id,
                        source_project_id: parse_optional_uuid(
                            &input.source_project_id,
                            "source_project_id",
                        )?,
                        origin,
                    },
                )
                .await?;
            return Ok(ToolResult::with_structured(
                format::format_source(&result, "Source saved"),
                result,
            ));
        }
        if let Some(source_key) = input
            .source_key
            .as_deref()
            .map(str::trim)
            .filter(|key| !key.is_empty())
        {
            let result = self.client.feed_source_remove(feed_id, source_key).await?;
            return Ok(ToolResult::with_structured(
                format!("[FEED] Removed source {source_key} from feed {feed_id}."),
                result,
            ));
        }
        let result = self.client.feed_sources(feed_id).await?;
        Ok(ToolResult::with_structured(
            format::format_sources(&result, feed_id),
            result,
        ))
    }

    async fn ground(
        &self,
        input: &FeedInput,
        ws: Option<Uuid>,
        proj: Option<Uuid>,
    ) -> Result<ToolResult> {
        let workspace_id = workspace_required(ws, "ground")?;
        let result = self
            .client
            .feed_ground(
                workspace_id,
                proj,
                Some(ground_limit(input.limit)),
                input.query.as_deref(),
            )
            .await?;
        Ok(ToolResult::with_structured(
            format::format_ground(&result, workspace_id),
            result,
        ))
    }
}

#[async_trait]
impl ToolHandler for FeedTool {
    async fn execute(&self, input: Value) -> Result<ToolResult> {
        let input: FeedInput =
            serde_json::from_value(input).map_err(|e| Error::Validation(e.to_string()))?;
        let action = input.action.trim().to_ascii_lowercase();
        if !VALID_ACTIONS.contains(&action.as_str()) {
            return Err(Error::Validation(format!(
                "Unknown action: {}. Available: {}",
                input.action,
                VALID_ACTIONS.join(", ")
            )));
        }
        let (ws, proj) = self.resolve_scope(&input).await?;
        match action.as_str() {
            "list" => self.list(&input, ws, proj).await,
            "ensure" => self.ensure(&input, ws, proj).await,
            "get" => self.get(&input, ws, proj).await,
            "update" => self.update(&input).await,
            "archive" => self.archive(&input).await,
            "items" => self.items(&input, ws, proj).await,
            "post" => self.post(&input, ws, proj).await,
            "follow" => self.follow(&input, ws, proj).await,
            "unfollow" => self.unfollow(&input).await,
            "read" => self.read(&input, ws, proj).await,
            "share" => self.share(&input).await,
            "unshare" => self.unshare(&input).await,
            "feedback" => self.feedback(&input).await,
            "curate" => self.curate(&input, ws, proj).await,
            "runs" => self.runs(&input, ws, proj).await,
            "sources" => self.sources(&input).await,
            "ground" => self.ground(&input, ws, proj).await,
            _ => Err(Error::Validation(format!(
                "Unknown action: {}. Available: {}",
                input.action,
                VALID_ACTIONS.join(", ")
            ))),
        }
    }

    fn metadata(&self) -> &ToolMetadata {
        static METADATA: std::sync::OnceLock<ToolMetadata> = std::sync::OnceLock::new();
        METADATA.get_or_init(schema::metadata)
    }

    fn input_schema(&self) -> Value {
        schema::input_schema()
    }
}

/// Register the `feed` tool.
pub fn register_feed_tools(
    registry: &mut crate::registry::ToolRegistry,
    client: ContextStreamClient,
    session: Arc<SessionManager>,
) {
    registry.register("feed", Arc::new(FeedTool::new(client, session)));
}
