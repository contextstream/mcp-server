//! Integration domain tools: Slack, GitHub, Notion operations.

use async_trait::async_trait;
use mcp_client::{
    ContextStreamClient, IntegrationActivityParams, IntegrationSearchParams,
    NotionCreateDatabaseParams, NotionCreatePageParams, NotionQueryDatabaseParams,
    NotionSearchPagesParams, NotionSort, NotionUpdatePageParams,
};
use mcp_session::SessionManager;
use mcp_types::{
    tool::{ToolAnnotations, ToolCategory, ToolMetadata, ToolResult},
    Error, Result,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::sync::Arc;
use uuid::Uuid;

use crate::registry::ToolHandler;
use crate::schema::SchemaBuilder;

// ============================================================================
// Valid Constants
// ============================================================================

/// Valid providers.
const VALID_PROVIDERS: &[&str] = &[
    "slack", "github", "notion", "linear", "jira", "figma", "all",
];

/// Valid actions.
const VALID_ACTIONS: &[&str] = &[
    "status",
    "search",
    "stats",
    "activity",
    "contributors",
    "knowledge",
    "summary",
    "channels",
    "discussions",
    "sync_users",
    "repos",
    "issues",
    "create_page",
    "create_database",
    "list_databases",
    "search_pages",
    "get_page",
    "query_database",
    "update_page",
    "team_activity",
    "team_search",
    "files",
    "connected",
];

/// Valid Notion event types.
const VALID_NOTION_EVENT_TYPES: &[&str] = &[
    "NotionTask",
    "NotionMeeting",
    "NotionWiki",
    "NotionBugReport",
    "NotionFeature",
    "NotionJournal",
    "NotionPage",
];

// ============================================================================
// Unified Integration Tool
// ============================================================================

/// Input for the unified integration tool.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IntegrationInput {
    pub provider: String,
    pub action: String,
    // Common fields
    pub workspace_id: Option<String>,
    pub project_id: Option<String>,
    pub query: Option<String>,
    pub limit: Option<i64>,
    pub days: Option<i64>,
    // Activity fields
    pub since: Option<String>,
    pub until: Option<String>,
    pub database_id: Option<String>,
    // Knowledge fields
    pub node_type: Option<String>,
    // Summary fields
    pub max_tokens: Option<i64>,
    // Notion page fields
    pub title: Option<String>,
    pub content: Option<String>,
    pub parent_page_id: Option<String>,
    pub parent_database_id: Option<String>,
    pub page_id: Option<String>,
    pub properties: Option<Value>,
    // Linear/Jira filter fields
    pub team_id: Option<String>,
    pub assignee: Option<String>,
    pub project_key: Option<String>,
    pub issue_type: Option<String>,
    pub file_key: Option<String>,
    pub figma_project_id: Option<String>,
    // Notion search_pages fields
    pub event_type: Option<String>,
    pub status: Option<String>,
    pub priority: Option<String>,
    pub has_due_date: Option<bool>,
    pub tags: Option<String>,
    // Notion database fields
    pub description: Option<String>,
    pub filter: Option<Value>,
    pub sorts: Option<Vec<NotionSortInput>>,
}

/// Sort input for Notion.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NotionSortInput {
    pub property: String,
    pub direction: String,
}

/// Unified integration tool handler.
pub struct IntegrationTool {
    client: ContextStreamClient,
    session: Arc<SessionManager>,
}

impl IntegrationTool {
    pub fn new(client: ContextStreamClient, session: Arc<SessionManager>) -> Self {
        Self { client, session }
    }

    fn parse_workspace_id(input: &Option<String>) -> Result<Option<Uuid>> {
        match input {
            Some(s) => {
                Ok(Some(Uuid::parse_str(s).map_err(|_| {
                    Error::Validation("Invalid workspace_id".to_string())
                })?))
            }
            None => Ok(None),
        }
    }
}

#[async_trait]
impl ToolHandler for IntegrationTool {
    async fn execute(&self, input: Value) -> Result<ToolResult> {
        let input: IntegrationInput =
            serde_json::from_value(input).map_err(|e| Error::Validation(e.to_string()))?;

        let provider = input.provider.to_lowercase();
        let action = input.action.to_lowercase();
        let workspace_id = Self::parse_workspace_id(&input.workspace_id)?;

        match action.as_str() {
            // Common actions (all providers)
            "status" => {
                let result = self
                    .client
                    .integration_status(&provider, workspace_id)
                    .await?;
                let text = if provider == "all" {
                    let count = result.as_array().map(|items| items.len()).unwrap_or(0);
                    format!("Retrieved {} integration statuses.", count)
                } else {
                    let status = result
                        .get("status")
                        .and_then(|v| v.as_str())
                        .unwrap_or("unknown");
                    format!("{} integration status: {}.", provider, status)
                };
                Ok(ToolResult::with_structured(text, result))
            }

            "search" => {
                let query = input
                    .query
                    .ok_or_else(|| Error::Validation("query is required for search".to_string()))?;
                let params = IntegrationSearchParams {
                    provider: provider.clone(),
                    query,
                    workspace_id,
                    limit: input.limit,
                };
                let result = self.client.integration_search(params).await?;
                let count = result.as_array().map(|a| a.len()).unwrap_or(0);
                Ok(ToolResult::with_structured(
                    format!("Found {} results from {}.", count, provider),
                    result,
                ))
            }

            "stats" => {
                let result = self
                    .client
                    .integration_stats(&provider, workspace_id, input.days)
                    .await?;
                Ok(ToolResult::with_structured(
                    format!("{} stats retrieved.", provider),
                    result,
                ))
            }

            "activity" => {
                // The backend exposes per-provider GET activity routes only;
                // there is no aggregate "all" route, so fan out client-side for
                // "all" and merge the per-provider results best-effort.
                if provider == "all" {
                    let providers = ["slack", "github", "notion", "linear", "jira", "figma"];
                    let mut items: Vec<Value> = Vec::new();
                    let mut errors: Vec<String> = Vec::new();
                    for p in providers {
                        let params = IntegrationActivityParams {
                            provider: p.to_string(),
                            workspace_id,
                            database_id: input.database_id.clone(),
                            since: input.since.clone(),
                            until: input.until.clone(),
                            limit: input.limit,
                        };
                        match self.client.integration_activity(params).await {
                            Ok(result) => {
                                if let Some(arr) = result.as_array() {
                                    items.extend(arr.iter().cloned());
                                } else if !result.is_null() {
                                    items.push(result);
                                }
                            }
                            Err(e) => errors.push(format!("{}: {}", p, e)),
                        }
                    }
                    let text = format!(
                        "Found {} activity item(s) across {} providers{}.",
                        items.len(),
                        providers.len(),
                        if errors.is_empty() {
                            String::new()
                        } else {
                            format!(" ({} not connected/errored)", errors.len())
                        }
                    );
                    Ok(ToolResult::with_structured(
                        text,
                        serde_json::json!({ "items": items, "errors": errors }),
                    ))
                } else {
                    let params = IntegrationActivityParams {
                        provider: provider.clone(),
                        workspace_id,
                        database_id: input.database_id.clone(),
                        since: input.since.clone(),
                        until: input.until.clone(),
                        limit: input.limit,
                    };
                    let result = self.client.integration_activity(params).await?;
                    let count = result.as_array().map(|a| a.len()).unwrap_or(0);
                    Ok(ToolResult::with_structured(
                        format!("Found {} activity items.", count),
                        result,
                    ))
                }
            }

            "contributors" => {
                let result = self
                    .client
                    .integration_contributors(&provider, workspace_id, input.limit)
                    .await?;
                let count = result.as_array().map(|a| a.len()).unwrap_or(0);
                Ok(ToolResult::with_structured(
                    format!("Found {} contributors.", count),
                    result,
                ))
            }

            "knowledge" => {
                let result = self
                    .client
                    .integration_knowledge(
                        &provider,
                        workspace_id,
                        input.node_type.as_deref(),
                        input.limit,
                    )
                    .await?;
                let count = result.as_array().map(|a| a.len()).unwrap_or(0);
                Ok(ToolResult::with_structured(
                    format!("Found {} knowledge items.", count),
                    result,
                ))
            }

            "summary" => {
                let result = self
                    .client
                    .integration_summary(&provider, workspace_id, input.days, input.max_tokens)
                    .await?;
                Ok(ToolResult::with_structured(
                    format!("{} summary retrieved.", provider),
                    result,
                ))
            }

            // Slack-specific actions
            "channels" => {
                if provider != "slack" {
                    return Err(Error::Validation(
                        "channels action is only for Slack".to_string(),
                    ));
                }
                let result = self
                    .client
                    .integration_slack_channels(workspace_id, input.limit)
                    .await?;
                let count = result.as_array().map(|a| a.len()).unwrap_or(0);
                Ok(ToolResult::with_structured(
                    format!("Found {} Slack channels.", count),
                    result,
                ))
            }

            "discussions" => {
                if provider != "slack" {
                    return Err(Error::Validation(
                        "discussions action is only for Slack".to_string(),
                    ));
                }
                let result = self
                    .client
                    .integration_slack_discussions(workspace_id, input.limit)
                    .await?;
                let count = result.as_array().map(|a| a.len()).unwrap_or(0);
                Ok(ToolResult::with_structured(
                    format!("Found {} Slack discussions.", count),
                    result,
                ))
            }

            "sync_users" => {
                if provider != "slack" {
                    return Err(Error::Validation(
                        "sync_users action is only for Slack".to_string(),
                    ));
                }
                let result = self
                    .client
                    .integration_slack_sync_users(workspace_id)
                    .await?;
                Ok(ToolResult::with_structured(
                    "Slack users synced.".to_string(),
                    result,
                ))
            }

            // GitHub-specific actions
            "repos" => {
                if provider != "github" {
                    return Err(Error::Validation(
                        "repos action is only for GitHub".to_string(),
                    ));
                }
                let result = self
                    .client
                    .integration_github_repos(workspace_id, input.limit)
                    .await?;
                let count = result.as_array().map(|a| a.len()).unwrap_or(0);
                Ok(ToolResult::with_structured(
                    format!("Found {} GitHub repos.", count),
                    result,
                ))
            }

            "issues" => match provider.as_str() {
                "github" => {
                    let result = self
                        .client
                        .integration_github_issues(workspace_id, input.limit)
                        .await?;
                    let count = result.as_array().map(|a| a.len()).unwrap_or(0);
                    Ok(ToolResult::with_structured(
                        format!("Found {} GitHub issues.", count),
                        result,
                    ))
                }
                "linear" => {
                    let result = self
                        .client
                        .integration_linear_issues(
                            workspace_id,
                            input.team_id.as_deref(),
                            input.status.as_deref(),
                            input.priority.as_deref(),
                            input.assignee.as_deref(),
                            input.limit,
                        )
                        .await?;
                    let count = result.as_array().map(|a| a.len()).unwrap_or(0);
                    Ok(ToolResult::with_structured(
                        format!("Found {} Linear issues.", count),
                        result,
                    ))
                }
                "jira" => {
                    let result = self
                        .client
                        .integration_jira_issues(
                            workspace_id,
                            input.project_key.as_deref(),
                            input.status.as_deref(),
                            input.priority.as_deref(),
                            input.issue_type.as_deref(),
                            input.assignee.as_deref(),
                            input.limit,
                        )
                        .await?;
                    let count = result.as_array().map(|a| a.len()).unwrap_or(0);
                    Ok(ToolResult::with_structured(
                        format!("Found {} Jira issues.", count),
                        result,
                    ))
                }
                _ => Err(Error::Validation(
                    "issues action is only for GitHub, Linear, or Jira".to_string(),
                )),
            },

            "files" => {
                if provider != "figma" {
                    return Err(Error::Validation(
                        "files action is only for Figma".to_string(),
                    ));
                }
                let result = self
                    .client
                    .integration_figma_files(
                        workspace_id,
                        input.figma_project_id.as_deref(),
                        input.limit,
                    )
                    .await?;
                let count = result.as_array().map(|a| a.len()).unwrap_or(0);
                Ok(ToolResult::with_structured(
                    format!("Found {} Figma files.", count),
                    result,
                ))
            }

            // Notion-specific actions
            "create_page" => {
                if provider != "notion" {
                    return Err(Error::Validation(
                        "create_page action is only for Notion".to_string(),
                    ));
                }
                let title = input.title.ok_or_else(|| {
                    Error::Validation("title is required for create_page".to_string())
                })?;
                let params = NotionCreatePageParams {
                    title,
                    content: input.content,
                    parent_page_id: input.parent_page_id,
                    parent_database_id: input.parent_database_id,
                    workspace_id,
                };
                let result = self.client.integration_notion_create_page(params).await?;
                Ok(ToolResult::with_structured(
                    "Notion page created.".to_string(),
                    result,
                ))
            }

            "get_page" => {
                if provider != "notion" {
                    return Err(Error::Validation(
                        "get_page action is only for Notion".to_string(),
                    ));
                }
                let page_id = input.page_id.ok_or_else(|| {
                    Error::Validation("page_id is required for get_page".to_string())
                })?;
                let result = self
                    .client
                    .integration_notion_get_page(&page_id, workspace_id)
                    .await?;
                Ok(ToolResult::with_structured(
                    format!("Retrieved Notion page: {}", page_id),
                    result,
                ))
            }

            "update_page" => {
                if provider != "notion" {
                    return Err(Error::Validation(
                        "update_page action is only for Notion".to_string(),
                    ));
                }
                let page_id = input.page_id.ok_or_else(|| {
                    Error::Validation("page_id is required for update_page".to_string())
                })?;
                let params = NotionUpdatePageParams {
                    page_id: page_id.clone(),
                    title: input.title,
                    content: input.content,
                    properties: input.properties,
                    workspace_id,
                };
                let result = self.client.integration_notion_update_page(params).await?;
                Ok(ToolResult::with_structured(
                    format!("Updated Notion page: {}", page_id),
                    result,
                ))
            }

            "search_pages" => {
                if provider != "notion" {
                    return Err(Error::Validation(
                        "search_pages action is only for Notion".to_string(),
                    ));
                }
                let params = NotionSearchPagesParams {
                    query: input.query,
                    database_id: input.database_id,
                    event_type: input.event_type,
                    status: input.status,
                    priority: input.priority,
                    has_due_date: input.has_due_date,
                    tags: input.tags,
                    workspace_id,
                    limit: input.limit,
                };
                let result = self.client.integration_notion_search_pages(params).await?;
                let count = result.as_array().map(|a| a.len()).unwrap_or(0);
                Ok(ToolResult::with_structured(
                    format!("Found {} Notion pages.", count),
                    result,
                ))
            }

            "create_database" => {
                if provider != "notion" {
                    return Err(Error::Validation(
                        "create_database action is only for Notion".to_string(),
                    ));
                }
                let title = input.title.ok_or_else(|| {
                    Error::Validation("title is required for create_database".to_string())
                })?;
                let params = NotionCreateDatabaseParams {
                    title,
                    description: input.description,
                    parent_page_id: input.parent_page_id,
                    workspace_id,
                };
                let result = self
                    .client
                    .integration_notion_create_database(params)
                    .await?;
                Ok(ToolResult::with_structured(
                    "Notion database created.".to_string(),
                    result,
                ))
            }

            "list_databases" => {
                if provider != "notion" {
                    return Err(Error::Validation(
                        "list_databases action is only for Notion".to_string(),
                    ));
                }
                let result = self
                    .client
                    .integration_notion_list_databases(workspace_id, input.limit)
                    .await?;
                let count = result.as_array().map(|a| a.len()).unwrap_or(0);
                Ok(ToolResult::with_structured(
                    format!("Found {} Notion databases.", count),
                    result,
                ))
            }

            "query_database" => {
                if provider != "notion" {
                    return Err(Error::Validation(
                        "query_database action is only for Notion".to_string(),
                    ));
                }
                let database_id = input.database_id.ok_or_else(|| {
                    Error::Validation("database_id is required for query_database".to_string())
                })?;
                let sorts = input.sorts.map(|s| {
                    s.into_iter()
                        .map(|sort| NotionSort {
                            property: sort.property,
                            direction: sort.direction,
                        })
                        .collect()
                });
                let params = NotionQueryDatabaseParams {
                    database_id,
                    filter: input.filter,
                    sorts,
                    workspace_id,
                    limit: input.limit,
                };
                let result = self
                    .client
                    .integration_notion_query_database(params)
                    .await?;
                let count = result.as_array().map(|a| a.len()).unwrap_or(0);
                Ok(ToolResult::with_structured(
                    format!("Query returned {} results.", count),
                    result,
                ))
            }

            // Team activity (all providers)
            "team_activity" => {
                if !self.session.team_features_enabled().await {
                    return Err(Error::Validation(
                        "team_activity requires team mode with an active team membership. Switch with session(action=\"set_account_mode\", account_mode=\"team\").".to_string(),
                    ));
                }
                let result = self
                    .client
                    .integration_team_activity(
                        &provider,
                        workspace_id,
                        input.since.as_deref(),
                        input.until.as_deref(),
                        input.limit,
                    )
                    .await?;
                let count = result.as_array().map(|a| a.len()).unwrap_or(0);
                Ok(ToolResult::with_structured(
                    format!("Found {} team activity items.", count),
                    result,
                ))
            }

            "team_search" => {
                if !self.session.team_features_enabled().await {
                    return Err(Error::Validation(
                        "team_search requires team mode. Cross-provider search aggregates results from all connected integrations (Linear + Jira + GitHub + Slack + Notion + Figma).".to_string(),
                    ));
                }
                let query = input.query.ok_or_else(|| {
                    Error::Validation("query is required for team_search".to_string())
                })?;
                let params = IntegrationSearchParams {
                    provider: "all".to_string(),
                    query,
                    workspace_id,
                    limit: input.limit,
                };
                let result = self.client.integration_search(params).await?;
                let count = result.as_array().map(|a| a.len()).unwrap_or(0);
                Ok(ToolResult::with_structured(
                    format!(
                        "Cross-provider team search: {} results from all connected integrations.",
                        count
                    ),
                    result,
                ))
            }

            "connected" => {
                let result = self.client.integration_status("all", workspace_id).await?;
                let connected: Vec<&str> = result
                    .as_array()
                    .map(|items| {
                        items
                            .iter()
                            .filter(|item| {
                                item.get("status")
                                    .and_then(|v| v.as_str())
                                    .map(|s| s == "connected" || s == "syncing")
                                    .unwrap_or(false)
                            })
                            .filter_map(|item| item.get("provider").and_then(|v| v.as_str()))
                            .collect()
                    })
                    .unwrap_or_default();
                let is_team = self.session.team_features_enabled().await;
                let text = if connected.is_empty() {
                    "No integrations connected. Use the dashboard to connect GitHub, Linear, Jira, Slack, Notion, or Figma.".to_string()
                } else {
                    let mut msg = format!("Connected integrations: {}.", connected.join(", "));
                    if is_team {
                        msg.push_str(" Team features available: team_activity, team_search (cross-provider).");
                    }
                    msg
                };
                Ok(ToolResult::with_structured(text, result))
            }

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
            name: "integration".to_string(),
            title: "Integration Operations".to_string(),
            description: "Integration operations for Slack, GitHub, Notion, Linear, Jira, and Figma. Provider: slack, github, notion, linear, jira, figma, all. Actions: status, search, stats, activity, contributors, knowledge, summary, connected (list connected integrations), channels (slack), discussions (slack), repos (github), issues (github/linear/jira), files (figma), create_page (notion), create_database (notion), list_databases (notion), search_pages (notion), get_page (notion), query_database (notion), update_page (notion), team_activity (team-only), team_search (team-only cross-provider search). Linear filters: team_id, status, priority, assignee. Jira filters: project_key, status, priority, issue_type, assignee. Figma filters: figma_project_id.".to_string(),
            category: ToolCategory::Integrations,
            annotations: ToolAnnotations::destructive(),
            is_pro: false,
            required_tier: None,
        })
    }

    fn input_schema(&self) -> Value {
        SchemaBuilder::new()
            .description("Integration operations")
            .string_enum("provider", "Integration provider", VALID_PROVIDERS, true)
            .string_enum("action", "Action to perform", VALID_ACTIONS, true)
            // Common fields
            .uuid("workspace_id", "Workspace ID", false)
            .uuid("project_id", "Project ID", false)
            .string("query", "Search query", false)
            .integer("limit", "Maximum results", false)
            .integer("days", "Number of days for stats/summary", false)
            // Activity fields
            .string("since", "Start date (ISO 8601)", false)
            .string("until", "End date (ISO 8601)", false)
            .string("database_id", "Notion database ID", false)
            // Knowledge fields
            .string("node_type", "Node type filter", false)
            // Summary fields
            .integer("max_tokens", "Maximum tokens for summary", false)
            // Notion page fields
            .string("title", "Page/database title", false)
            .string("content", "Page content (Markdown)", false)
            .string("parent_page_id", "Parent page ID", false)
            .string("parent_database_id", "Parent database ID", false)
            .string("page_id", "Page ID", false)
            .object("properties", "Page properties", false)
            // Notion search_pages fields
            .string_enum(
                "event_type",
                "Notion content type",
                VALID_NOTION_EVENT_TYPES,
                false,
            )
            .string(
                "status",
                "Status filter (e.g., 'Done', 'In Progress')",
                false,
            )
            .string(
                "priority",
                "Priority filter (e.g., 'High', 'Medium', 'Low')",
                false,
            )
            .boolean("has_due_date", "Filter by due date presence", false)
            .string("tags", "Tags filter (comma-separated)", false)
            // Notion database fields
            .string("description", "Database description", false)
            .object("filter", "Query filter", false)
            .array("sorts", "Sort order", "object", false)
            // Linear/Jira/Figma fields
            .string("team_id", "Linear team ID filter", false)
            .string("assignee", "Assignee filter (Linear/Jira)", false)
            .string("project_key", "Jira project key (e.g. 'ENG')", false)
            .string(
                "issue_type",
                "Jira issue type (Bug, Story, Task, Epic)",
                false,
            )
            .string("file_key", "Figma file key", false)
            .string("figma_project_id", "Figma project ID filter", false)
            .build()
    }
}

/// Register all integration tools.
pub fn register_integration_tools(
    registry: &mut crate::registry::ToolRegistry,
    client: ContextStreamClient,
    session: Arc<SessionManager>,
) {
    registry.register(
        "integration",
        Arc::new(IntegrationTool::new(client, session)),
    );
}

#[cfg(test)]
#[path = "integrations_tests.rs"]
mod tests;
