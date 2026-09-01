//! Unified VCS domain tool for GitHub, GitLab, and Bitbucket-backed repository data.

use async_trait::async_trait;
use mcp_client::ContextStreamClient;
use mcp_types::{
    tool::{ToolAnnotations, ToolCategory, ToolMetadata, ToolResult},
    Error, Result,
};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::sync::Arc;
use urlencoding::encode;
use uuid::Uuid;

use crate::registry::ToolHandler;
use crate::schema::SchemaBuilder;

const VALID_ACTIONS: &[&str] = &[
    "list_repos",
    "get_repo",
    "sync_repo",
    "update_repo_settings",
    "delete_repo",
    "get_repo_projects",
    "link_repo_project",
    "import_repo_project",
    "ingest_repo",
    "get_ingest_status",
    "list_pulls",
    "get_pull",
    "get_pull_diff",
    "get_pull_comments",
    "get_pull_commits",
    "get_pull_checks",
    "get_pull_summary",
    "summarize_pull",
    "review_pull",
    "comment_pull",
    "merge_pull",
    "list_issues",
    "get_issue",
    "get_issue_comments",
    "create_issue",
    "update_issue",
    "comment_issue",
    "list_commits",
    "get_commit",
    "get_commit_diff",
    "compare_refs",
    "list_branches",
    "list_tags",
    "get_tree",
    "get_blob",
    "search_code",
    "get_activity",
    "search_vcs",
    "list_notifications",
    "mark_notification_read",
    "mark_all_notifications_read",
    "list_links",
    "create_link",
    "delete_link",
    "list_automations",
    "create_automation",
    "update_automation",
    "delete_automation",
    "register_webhook",
    "unregister_webhook",
];

const VALID_PROVIDERS: &[&str] = &["github", "gitlab", "bitbucket"];

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VcsInput {
    pub action: String,
    pub workspace_id: Option<String>,
    pub provider: Option<String>,
    pub owner: Option<String>,
    pub repo: Option<String>,
    pub repo_ref: Option<String>,
    pub number: Option<i64>,
    pub sha: Option<String>,
    pub state: Option<String>,
    pub branch: Option<String>,
    pub query: Option<String>,
    pub scope: Option<String>,
    pub base: Option<String>,
    pub head: Option<String>,
    pub ref_name: Option<String>,
    pub path: Option<String>,
    pub page: Option<i64>,
    pub per_page: Option<i64>,
    pub full: Option<bool>,
    pub project_id: Option<String>,
    pub project_name: Option<String>,
    pub description: Option<String>,
    pub create_project: Option<bool>,
    pub ingest: Option<bool>,
    pub limit: Option<i64>,
    pub sync_config: Option<Value>,
    // PR mutation fields
    pub title: Option<String>,
    pub body: Option<String>,
    pub event: Option<String>,
    pub merge_method: Option<String>,
    pub commit_title: Option<String>,
    pub commit_message: Option<String>,
    pub comments: Option<Value>,
    // Issue mutation fields
    pub labels: Option<Vec<String>>,
    pub assignees: Option<Vec<String>>,
    // Notification fields
    pub notification_id: Option<String>,
    // Link fields
    pub link_id: Option<String>,
    pub source_type: Option<String>,
    pub source_id: Option<String>,
    pub target_type: Option<String>,
    pub target_id: Option<String>,
    // Automation fields
    pub automation_id: Option<String>,
    pub automation_config: Option<Value>,
}

pub struct VcsTool {
    client: ContextStreamClient,
}

impl VcsTool {
    pub fn new(client: ContextStreamClient) -> Self {
        Self { client }
    }

    async fn resolve_workspace_id(&self, input: &Option<String>) -> Result<Uuid> {
        if let Some(value) = input {
            return Uuid::parse_str(value)
                .map_err(|_| Error::Validation("Invalid workspace_id".to_string()));
        }

        let config = self.client.config().await;
        config.default_workspace_id.ok_or_else(|| {
            Error::Validation(
                "workspace_id is required when no default workspace is configured".to_string(),
            )
        })
    }

    fn validate_provider(provider: &Option<String>) -> Result<Option<String>> {
        let provider = provider
            .as_ref()
            .map(|value| value.trim().to_lowercase())
            .filter(|value| !value.is_empty());

        if let Some(value) = provider.as_deref() {
            if !VALID_PROVIDERS.contains(&value) {
                return Err(Error::Validation(format!(
                    "Invalid provider '{}'. Expected one of: {}",
                    value,
                    VALID_PROVIDERS.join(", ")
                )));
            }
        }

        Ok(provider)
    }

    fn repo_ref(input: &VcsInput) -> Result<String> {
        if let Some(repo_ref) = input.repo_ref.as_ref().map(|value| value.trim()) {
            if !repo_ref.is_empty() {
                return Ok(repo_ref.to_string());
            }
        }

        match (
            input.owner.as_ref().map(|value| value.trim()),
            input.repo.as_ref().map(|value| value.trim()),
        ) {
            (Some(owner), Some(repo)) if !owner.is_empty() && !repo.is_empty() => {
                Ok(format!("{}/{}", owner, repo))
            }
            (None, Some(repo)) if repo.contains('/') => Ok(repo.to_string()),
            _ => Err(Error::Validation(
                "repo_ref is required, or provide owner + repo".to_string(),
            )),
        }
    }

    fn require_number(input: &VcsInput, field_name: &str) -> Result<i64> {
        input
            .number
            .ok_or_else(|| Error::Validation(format!("{} is required", field_name)))
    }

    fn require_sha(input: &VcsInput) -> Result<String> {
        input
            .sha
            .as_ref()
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty())
            .ok_or_else(|| Error::Validation("sha is required".to_string()))
    }

    fn require_query(input: &VcsInput) -> Result<String> {
        input
            .query
            .as_ref()
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty())
            .ok_or_else(|| Error::Validation("query is required".to_string()))
    }

    fn require_ref_name(input: &VcsInput) -> Result<String> {
        input
            .ref_name
            .as_ref()
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty())
            .ok_or_else(|| Error::Validation("ref_name is required".to_string()))
    }

    fn require_path(input: &VcsInput) -> Result<String> {
        input
            .path
            .as_ref()
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty())
            .ok_or_else(|| Error::Validation("path is required".to_string()))
    }

    fn require_project_id(input: &VcsInput) -> Result<Uuid> {
        let raw = input
            .project_id
            .as_ref()
            .ok_or_else(|| Error::Validation("project_id is required".to_string()))?;

        Uuid::parse_str(raw).map_err(|_| Error::Validation("Invalid project_id".to_string()))
    }

    fn query_path(path: String, params: Vec<(String, String)>) -> String {
        if params.is_empty() {
            return path;
        }

        let query = params
            .into_iter()
            .map(|(key, value)| format!("{}={}", encode(&key), encode(&value)))
            .collect::<Vec<_>>()
            .join("&");

        format!("{}?{}", path, query)
    }

    fn push_query_value(params: &mut Vec<(String, String)>, key: &str, value: Option<String>) {
        if let Some(value) = value.filter(|value| !value.is_empty()) {
            params.push((key.to_string(), value));
        }
    }

    fn count_results(value: &Value) -> usize {
        if let Some(array) = value.as_array() {
            return array.len();
        }
        for key in ["items", "projects", "notifications", "links", "automations"] {
            if let Some(array) = value.get(key).and_then(|v| v.as_array()) {
                return array.len();
            }
        }
        1
    }

    fn repo_base_path(workspace_id: Uuid, repo_ref: &str) -> String {
        format!(
            "/integrations/workspaces/{}/vcs/repos/{}",
            workspace_id,
            encode(repo_ref)
        )
    }

    fn workspace_base_path(workspace_id: Uuid) -> String {
        format!("/integrations/workspaces/{}/vcs", workspace_id)
    }

    fn success_text(action: &str, result: &Value) -> String {
        let count = Self::count_results(result);
        match action {
            "list_repos" => format!("Found {} repositories.", count),
            "get_repo" => "Repository retrieved.".to_string(),
            "sync_repo" => "Repository sync triggered.".to_string(),
            "update_repo_settings" => "Repository settings updated.".to_string(),
            "get_repo_projects" => format!("Found {} linked projects.", count),
            "link_repo_project" => "Repository linked to project.".to_string(),
            "import_repo_project" => "Project created from repository.".to_string(),
            "ingest_repo" => "Repository ingestion triggered.".to_string(),
            "get_ingest_status" => "Repository ingest status retrieved.".to_string(),
            "list_pulls" => format!("Found {} pull requests.", count),
            "get_pull" => "Pull request retrieved.".to_string(),
            "get_pull_diff" => format!("Retrieved {} changed files.", count),
            "get_pull_comments" => format!("Retrieved {} pull request comments.", count),
            "get_pull_commits" => format!("Retrieved {} pull request commits.", count),
            "get_pull_checks" => format!("Retrieved {} status checks.", count),
            "get_pull_summary" => "Pull request summary retrieved.".to_string(),
            "summarize_pull" => "Pull request summary triggered.".to_string(),
            "list_issues" => format!("Found {} issues.", count),
            "get_issue" => "Issue retrieved.".to_string(),
            "get_issue_comments" => format!("Retrieved {} issue comments.", count),
            "list_commits" => format!("Found {} commits.", count),
            "get_commit" => "Commit retrieved.".to_string(),
            "get_commit_diff" => "Commit diff retrieved.".to_string(),
            "compare_refs" => "Ref comparison retrieved.".to_string(),
            "list_branches" => format!("Found {} branches.", count),
            "list_tags" => format!("Found {} tags.", count),
            "get_tree" => format!("Retrieved {} tree entries.", count),
            "get_blob" => "File blob retrieved.".to_string(),
            "search_code" => format!("Found {} repo code matches.", count),
            "get_activity" => format!("Found {} activity items.", count),
            "search_vcs" => format!("Found {} VCS search results.", count),
            "delete_repo" => "Repository disconnected.".to_string(),
            "review_pull" => "Pull request review submitted.".to_string(),
            "comment_pull" => "Pull request comment added.".to_string(),
            "merge_pull" => "Pull request merged.".to_string(),
            "create_issue" => "Issue created.".to_string(),
            "update_issue" => "Issue updated.".to_string(),
            "comment_issue" => "Issue comment added.".to_string(),
            "list_notifications" => format!("Found {} notifications.", count),
            "mark_notification_read" => "Notification marked as read.".to_string(),
            "mark_all_notifications_read" => "All notifications marked as read.".to_string(),
            "list_links" => format!("Found {} links.", count),
            "create_link" => "Link created.".to_string(),
            "delete_link" => "Link deleted.".to_string(),
            "list_automations" => format!("Found {} automations.", count),
            "create_automation" => "Automation created.".to_string(),
            "update_automation" => "Automation updated.".to_string(),
            "delete_automation" => "Automation deleted.".to_string(),
            "register_webhook" => "Webhook registered.".to_string(),
            "unregister_webhook" => "Webhook unregistered.".to_string(),
            _ => "VCS request completed.".to_string(),
        }
    }
}

#[async_trait]
impl ToolHandler for VcsTool {
    async fn execute(&self, input: Value) -> Result<ToolResult> {
        let input: VcsInput =
            serde_json::from_value(input).map_err(|e| Error::Validation(e.to_string()))?;

        let action = input.action.trim().to_lowercase();
        if !VALID_ACTIONS.contains(&action.as_str()) {
            return Err(Error::Validation(format!(
                "Invalid action '{}'. Expected one of: {}",
                action,
                VALID_ACTIONS.join(", ")
            )));
        }

        let workspace_id = self.resolve_workspace_id(&input.workspace_id).await?;
        let provider = Self::validate_provider(&input.provider)?;
        let workspace_base = Self::workspace_base_path(workspace_id);

        let result: Value = match action.as_str() {
            "list_repos" => {
                let mut params = Vec::new();
                Self::push_query_value(&mut params, "provider", provider.clone());
                Self::push_query_value(&mut params, "q", input.query.clone());
                Self::push_query_value(&mut params, "page", input.page.map(|v| v.to_string()));
                Self::push_query_value(
                    &mut params,
                    "per_page",
                    input.per_page.map(|v| v.to_string()),
                );
                let path = Self::query_path(format!("{}/repos", workspace_base), params);
                self.client.get(&path).await?
            }
            "get_repo" => {
                let repo_ref = Self::repo_ref(&input)?;
                let mut params = Vec::new();
                Self::push_query_value(&mut params, "provider", provider.clone());
                let path = Self::query_path(Self::repo_base_path(workspace_id, &repo_ref), params);
                self.client.get(&path).await?
            }
            "sync_repo" => {
                let repo_ref = Self::repo_ref(&input)?;
                let mut params = Vec::new();
                Self::push_query_value(&mut params, "provider", provider.clone());
                Self::push_query_value(&mut params, "full", input.full.map(|v| v.to_string()));
                let path = Self::query_path(
                    format!("{}/sync", Self::repo_base_path(workspace_id, &repo_ref)),
                    params,
                );
                self.client.post(&path, serde_json::json!({})).await?
            }
            "update_repo_settings" => {
                let repo_ref = Self::repo_ref(&input)?;
                let sync_config = input.sync_config.clone().ok_or_else(|| {
                    Error::Validation(
                        "sync_config is required for update_repo_settings".to_string(),
                    )
                })?;
                let mut params = Vec::new();
                Self::push_query_value(&mut params, "provider", provider.clone());
                let path = Self::query_path(
                    format!("{}/settings", Self::repo_base_path(workspace_id, &repo_ref)),
                    params,
                );
                self.client
                    .put(&path, serde_json::json!({ "sync_config": sync_config }))
                    .await?
            }
            "get_repo_projects" => {
                let repo_ref = Self::repo_ref(&input)?;
                let mut params = Vec::new();
                Self::push_query_value(&mut params, "provider", provider.clone());
                let path = Self::query_path(
                    format!("{}/project", Self::repo_base_path(workspace_id, &repo_ref)),
                    params,
                );
                self.client.get(&path).await?
            }
            "link_repo_project" => {
                let repo_ref = Self::repo_ref(&input)?;
                let project_id = Self::require_project_id(&input)?;
                let mut params = Vec::new();
                Self::push_query_value(&mut params, "provider", provider.clone());
                let path = Self::query_path(
                    format!("{}/project", Self::repo_base_path(workspace_id, &repo_ref)),
                    params,
                );
                let mut body = Map::new();
                body.insert(
                    "project_id".to_string(),
                    Value::String(project_id.to_string()),
                );
                if let Some(ingest) = input.ingest {
                    body.insert("ingest".to_string(), Value::Bool(ingest));
                }
                self.client.put(&path, Value::Object(body)).await?
            }
            "import_repo_project" => {
                let repo_ref = Self::repo_ref(&input)?;
                let mut params = Vec::new();
                Self::push_query_value(&mut params, "provider", provider.clone());
                let path = Self::query_path(
                    format!("{}/project", Self::repo_base_path(workspace_id, &repo_ref)),
                    params,
                );
                let mut body = Map::new();
                if let Some(project_name) =
                    input.project_name.clone().filter(|v| !v.trim().is_empty())
                {
                    body.insert("project_name".to_string(), Value::String(project_name));
                }
                if let Some(description) = input.description.clone() {
                    body.insert("description".to_string(), Value::String(description));
                }
                if let Some(ingest) = input.ingest {
                    body.insert("ingest".to_string(), Value::Bool(ingest));
                }
                self.client.post(&path, Value::Object(body)).await?
            }
            "ingest_repo" => {
                let repo_ref = Self::repo_ref(&input)?;
                let mut params = Vec::new();
                Self::push_query_value(&mut params, "provider", provider.clone());
                let path = Self::query_path(
                    format!("{}/ingest", Self::repo_base_path(workspace_id, &repo_ref)),
                    params,
                );
                let mut body = Map::new();
                if let Some(project_id) = input.project_id.clone() {
                    let parsed = Uuid::parse_str(&project_id)
                        .map_err(|_| Error::Validation("Invalid project_id".to_string()))?;
                    body.insert("project_id".to_string(), Value::String(parsed.to_string()));
                }
                if let Some(project_name) =
                    input.project_name.clone().filter(|v| !v.trim().is_empty())
                {
                    body.insert("project_name".to_string(), Value::String(project_name));
                }
                if let Some(description) = input.description.clone() {
                    body.insert("description".to_string(), Value::String(description));
                }
                if let Some(create_project) = input.create_project {
                    body.insert("create_project".to_string(), Value::Bool(create_project));
                }
                self.client.post(&path, Value::Object(body)).await?
            }
            "get_ingest_status" => {
                let repo_ref = Self::repo_ref(&input)?;
                let mut params = Vec::new();
                Self::push_query_value(&mut params, "provider", provider.clone());
                let path = Self::query_path(
                    format!(
                        "{}/ingest/status",
                        Self::repo_base_path(workspace_id, &repo_ref)
                    ),
                    params,
                );
                self.client.get(&path).await?
            }
            "list_pulls" => {
                let base_path = if input.repo_ref.is_some() || input.owner.is_some() {
                    format!(
                        "{}/pulls",
                        Self::repo_base_path(workspace_id, &Self::repo_ref(&input)?)
                    )
                } else {
                    format!("{}/pulls", workspace_base)
                };
                let mut params = Vec::new();
                Self::push_query_value(&mut params, "provider", provider.clone());
                Self::push_query_value(&mut params, "state", input.state.clone());
                Self::push_query_value(&mut params, "page", input.page.map(|v| v.to_string()));
                Self::push_query_value(
                    &mut params,
                    "per_page",
                    input.per_page.map(|v| v.to_string()),
                );
                let path = Self::query_path(base_path, params);
                self.client.get(&path).await?
            }
            "get_pull" | "get_pull_diff" | "get_pull_comments" | "get_pull_commits"
            | "get_pull_checks" | "get_pull_summary" | "summarize_pull" => {
                let repo_ref = Self::repo_ref(&input)?;
                let number = Self::require_number(&input, "number")?;
                let suffix = match action.as_str() {
                    "get_pull" => "".to_string(),
                    "get_pull_diff" => "/files".to_string(),
                    "get_pull_comments" => "/comments".to_string(),
                    "get_pull_commits" => "/commits".to_string(),
                    "get_pull_checks" => "/checks".to_string(),
                    "get_pull_summary" | "summarize_pull" => "/summary".to_string(),
                    _ => String::new(),
                };
                let mut params = Vec::new();
                Self::push_query_value(&mut params, "provider", provider.clone());
                let path = Self::query_path(
                    format!(
                        "{}/pulls/{}{}",
                        Self::repo_base_path(workspace_id, &repo_ref),
                        number,
                        suffix
                    ),
                    params,
                );
                if action == "summarize_pull" {
                    self.client.post(&path, serde_json::json!({})).await?
                } else {
                    self.client.get(&path).await?
                }
            }
            "list_issues" => {
                let base_path = if input.repo_ref.is_some() || input.owner.is_some() {
                    format!(
                        "{}/issues",
                        Self::repo_base_path(workspace_id, &Self::repo_ref(&input)?)
                    )
                } else {
                    format!("{}/issues", workspace_base)
                };
                let mut params = Vec::new();
                Self::push_query_value(&mut params, "provider", provider.clone());
                Self::push_query_value(&mut params, "state", input.state.clone());
                Self::push_query_value(&mut params, "page", input.page.map(|v| v.to_string()));
                Self::push_query_value(
                    &mut params,
                    "per_page",
                    input.per_page.map(|v| v.to_string()),
                );
                let path = Self::query_path(base_path, params);
                self.client.get(&path).await?
            }
            "get_issue" | "get_issue_comments" => {
                let repo_ref = Self::repo_ref(&input)?;
                let number = Self::require_number(&input, "number")?;
                let suffix = if action == "get_issue_comments" {
                    "/comments"
                } else {
                    ""
                };
                let mut params = Vec::new();
                Self::push_query_value(&mut params, "provider", provider.clone());
                let path = Self::query_path(
                    format!(
                        "{}/issues/{}{}",
                        Self::repo_base_path(workspace_id, &repo_ref),
                        number,
                        suffix
                    ),
                    params,
                );
                self.client.get(&path).await?
            }
            "list_commits" => {
                let repo_ref = Self::repo_ref(&input)?;
                let mut params = Vec::new();
                Self::push_query_value(&mut params, "provider", provider.clone());
                Self::push_query_value(&mut params, "branch", input.branch.clone());
                Self::push_query_value(&mut params, "page", input.page.map(|v| v.to_string()));
                Self::push_query_value(
                    &mut params,
                    "per_page",
                    input.per_page.map(|v| v.to_string()),
                );
                let path = Self::query_path(
                    format!("{}/commits", Self::repo_base_path(workspace_id, &repo_ref)),
                    params,
                );
                self.client.get(&path).await?
            }
            "get_commit" | "get_commit_diff" => {
                let repo_ref = Self::repo_ref(&input)?;
                let sha = encode(&Self::require_sha(&input)?).into_owned();
                let suffix = if action == "get_commit_diff" {
                    "/diff"
                } else {
                    ""
                };
                let mut params = Vec::new();
                Self::push_query_value(&mut params, "provider", provider.clone());
                let path = Self::query_path(
                    format!(
                        "{}/commits/{}{}",
                        Self::repo_base_path(workspace_id, &repo_ref),
                        sha,
                        suffix
                    ),
                    params,
                );
                self.client.get(&path).await?
            }
            "compare_refs" => {
                let repo_ref = Self::repo_ref(&input)?;
                let base = input
                    .base
                    .as_ref()
                    .map(|value| value.trim().to_string())
                    .filter(|value| !value.is_empty())
                    .ok_or_else(|| Error::Validation("base is required".to_string()))?;
                let head = input
                    .head
                    .as_ref()
                    .map(|value| value.trim().to_string())
                    .filter(|value| !value.is_empty())
                    .ok_or_else(|| Error::Validation("head is required".to_string()))?;
                let spec = encode(&format!("{}...{}", base, head)).into_owned();
                let mut params = Vec::new();
                Self::push_query_value(&mut params, "provider", provider.clone());
                let path = Self::query_path(
                    format!(
                        "{}/compare/{}",
                        Self::repo_base_path(workspace_id, &repo_ref),
                        spec
                    ),
                    params,
                );
                self.client.get(&path).await?
            }
            "list_branches" => {
                let repo_ref = Self::repo_ref(&input)?;
                let mut params = Vec::new();
                Self::push_query_value(&mut params, "provider", provider.clone());
                let path = Self::query_path(
                    format!("{}/branches", Self::repo_base_path(workspace_id, &repo_ref)),
                    params,
                );
                self.client.get(&path).await?
            }
            "list_tags" => {
                let repo_ref = Self::repo_ref(&input)?;
                let mut params = Vec::new();
                Self::push_query_value(&mut params, "provider", provider.clone());
                let path = Self::query_path(
                    format!("{}/tags", Self::repo_base_path(workspace_id, &repo_ref)),
                    params,
                );
                self.client.get(&path).await?
            }
            "get_tree" => {
                let repo_ref = Self::repo_ref(&input)?;
                let ref_name = encode(&Self::require_ref_name(&input)?).into_owned();
                let mut params = Vec::new();
                Self::push_query_value(&mut params, "provider", provider.clone());
                let path = Self::query_path(
                    format!(
                        "{}/tree/{}",
                        Self::repo_base_path(workspace_id, &repo_ref),
                        ref_name
                    ),
                    params,
                );
                self.client.get(&path).await?
            }
            "get_blob" => {
                let repo_ref = Self::repo_ref(&input)?;
                let ref_name = encode(&Self::require_ref_name(&input)?).into_owned();
                let blob_path = encode(&Self::require_path(&input)?).into_owned();
                let mut params = Vec::new();
                Self::push_query_value(&mut params, "provider", provider.clone());
                let path = Self::query_path(
                    format!(
                        "{}/blob/{}/{}",
                        Self::repo_base_path(workspace_id, &repo_ref),
                        ref_name,
                        blob_path
                    ),
                    params,
                );
                self.client.get(&path).await?
            }
            "search_code" => {
                let repo_ref = Self::repo_ref(&input)?;
                let mut params = Vec::new();
                Self::push_query_value(&mut params, "provider", provider.clone());
                Self::push_query_value(&mut params, "q", Some(Self::require_query(&input)?));
                Self::push_query_value(&mut params, "limit", input.limit.map(|v| v.to_string()));
                let path = Self::query_path(
                    format!(
                        "{}/search/code",
                        Self::repo_base_path(workspace_id, &repo_ref)
                    ),
                    params,
                );
                self.client.get(&path).await?
            }
            "get_activity" => {
                let base_path = if input.repo_ref.is_some() || input.owner.is_some() {
                    format!(
                        "{}/activity",
                        Self::repo_base_path(workspace_id, &Self::repo_ref(&input)?)
                    )
                } else {
                    format!("{}/activity", workspace_base)
                };
                let mut params = Vec::new();
                Self::push_query_value(&mut params, "provider", provider.clone());
                Self::push_query_value(&mut params, "page", input.page.map(|v| v.to_string()));
                Self::push_query_value(
                    &mut params,
                    "per_page",
                    input.per_page.map(|v| v.to_string()),
                );
                let path = Self::query_path(base_path, params);
                self.client.get(&path).await?
            }
            "search_vcs" => {
                let mut params = Vec::new();
                Self::push_query_value(&mut params, "provider", provider.clone());
                Self::push_query_value(&mut params, "q", Some(Self::require_query(&input)?));
                Self::push_query_value(&mut params, "scope", input.scope.clone());
                Self::push_query_value(&mut params, "page", input.page.map(|v| v.to_string()));
                Self::push_query_value(
                    &mut params,
                    "per_page",
                    input.per_page.map(|v| v.to_string()),
                );
                let path = Self::query_path(format!("{}/search", workspace_base), params);
                self.client.get(&path).await?
            }
            // -- PR mutations --
            "review_pull" => {
                let repo_ref = Self::repo_ref(&input)?;
                let number = Self::require_number(&input, "number")?;
                let mut params = Vec::new();
                Self::push_query_value(&mut params, "provider", provider.clone());
                let path = Self::query_path(
                    format!(
                        "{}/pulls/{}/review",
                        Self::repo_base_path(workspace_id, &repo_ref),
                        number
                    ),
                    params,
                );
                let mut body = Map::new();
                if let Some(event) = input.event.clone().filter(|v| !v.trim().is_empty()) {
                    body.insert("event".to_string(), Value::String(event));
                }
                if let Some(text) = input.body.clone().filter(|v| !v.trim().is_empty()) {
                    body.insert("body".to_string(), Value::String(text));
                }
                if let Some(comments) = input.comments.clone() {
                    body.insert("comments".to_string(), comments);
                }
                self.client.post(&path, Value::Object(body)).await?
            }
            "comment_pull" => {
                let repo_ref = Self::repo_ref(&input)?;
                let number = Self::require_number(&input, "number")?;
                let text = input
                    .body
                    .as_ref()
                    .map(|v| v.trim().to_string())
                    .filter(|v| !v.is_empty())
                    .ok_or_else(|| Error::Validation("body is required".to_string()))?;
                let mut params = Vec::new();
                Self::push_query_value(&mut params, "provider", provider.clone());
                let path = Self::query_path(
                    format!(
                        "{}/pulls/{}/comment",
                        Self::repo_base_path(workspace_id, &repo_ref),
                        number
                    ),
                    params,
                );
                self.client
                    .post(&path, serde_json::json!({ "body": text }))
                    .await?
            }
            "merge_pull" => {
                let repo_ref = Self::repo_ref(&input)?;
                let number = Self::require_number(&input, "number")?;
                let mut params = Vec::new();
                Self::push_query_value(&mut params, "provider", provider.clone());
                let path = Self::query_path(
                    format!(
                        "{}/pulls/{}/merge",
                        Self::repo_base_path(workspace_id, &repo_ref),
                        number
                    ),
                    params,
                );
                let mut body = Map::new();
                if let Some(method) = input.merge_method.clone().filter(|v| !v.trim().is_empty()) {
                    body.insert("merge_method".to_string(), Value::String(method));
                }
                if let Some(title) = input.commit_title.clone().filter(|v| !v.trim().is_empty()) {
                    body.insert("commit_title".to_string(), Value::String(title));
                }
                if let Some(message) = input
                    .commit_message
                    .clone()
                    .filter(|v| !v.trim().is_empty())
                {
                    body.insert("commit_message".to_string(), Value::String(message));
                }
                self.client.post(&path, Value::Object(body)).await?
            }
            // -- Issue mutations --
            "create_issue" => {
                let repo_ref = Self::repo_ref(&input)?;
                let title = input
                    .title
                    .as_ref()
                    .map(|v| v.trim().to_string())
                    .filter(|v| !v.is_empty())
                    .ok_or_else(|| Error::Validation("title is required".to_string()))?;
                let mut params = Vec::new();
                Self::push_query_value(&mut params, "provider", provider.clone());
                let path = Self::query_path(
                    format!("{}/issues", Self::repo_base_path(workspace_id, &repo_ref)),
                    params,
                );
                let mut body = Map::new();
                body.insert("title".to_string(), Value::String(title));
                if let Some(text) = input.body.clone().filter(|v| !v.trim().is_empty()) {
                    body.insert("body".to_string(), Value::String(text));
                }
                if let Some(labels) = input.labels.clone().filter(|v| !v.is_empty()) {
                    body.insert(
                        "labels".to_string(),
                        Value::Array(labels.into_iter().map(Value::String).collect()),
                    );
                }
                if let Some(assignees) = input.assignees.clone().filter(|v| !v.is_empty()) {
                    body.insert(
                        "assignees".to_string(),
                        Value::Array(assignees.into_iter().map(Value::String).collect()),
                    );
                }
                self.client.post(&path, Value::Object(body)).await?
            }
            "update_issue" => {
                let repo_ref = Self::repo_ref(&input)?;
                let number = Self::require_number(&input, "number")?;
                let mut params = Vec::new();
                Self::push_query_value(&mut params, "provider", provider.clone());
                let path = Self::query_path(
                    format!(
                        "{}/issues/{}",
                        Self::repo_base_path(workspace_id, &repo_ref),
                        number
                    ),
                    params,
                );
                let mut body = Map::new();
                if let Some(title) = input.title.clone().filter(|v| !v.trim().is_empty()) {
                    body.insert("title".to_string(), Value::String(title));
                }
                if let Some(text) = input.body.clone().filter(|v| !v.trim().is_empty()) {
                    body.insert("body".to_string(), Value::String(text));
                }
                if let Some(state) = input.state.clone().filter(|v| !v.trim().is_empty()) {
                    body.insert("state".to_string(), Value::String(state));
                }
                if let Some(labels) = input.labels.clone().filter(|v| !v.is_empty()) {
                    body.insert(
                        "labels".to_string(),
                        Value::Array(labels.into_iter().map(Value::String).collect()),
                    );
                }
                if let Some(assignees) = input.assignees.clone().filter(|v| !v.is_empty()) {
                    body.insert(
                        "assignees".to_string(),
                        Value::Array(assignees.into_iter().map(Value::String).collect()),
                    );
                }
                self.client.patch(&path, Value::Object(body)).await?
            }
            "comment_issue" => {
                let repo_ref = Self::repo_ref(&input)?;
                let number = Self::require_number(&input, "number")?;
                let text = input
                    .body
                    .as_ref()
                    .map(|v| v.trim().to_string())
                    .filter(|v| !v.is_empty())
                    .ok_or_else(|| Error::Validation("body is required".to_string()))?;
                let mut params = Vec::new();
                Self::push_query_value(&mut params, "provider", provider.clone());
                let path = Self::query_path(
                    format!(
                        "{}/issues/{}/comment",
                        Self::repo_base_path(workspace_id, &repo_ref),
                        number
                    ),
                    params,
                );
                self.client
                    .post(&path, serde_json::json!({ "body": text }))
                    .await?
            }
            // -- Repo delete --
            "delete_repo" => {
                let repo_ref = Self::repo_ref(&input)?;
                let mut params = Vec::new();
                Self::push_query_value(&mut params, "provider", provider.clone());
                let path = Self::query_path(Self::repo_base_path(workspace_id, &repo_ref), params);
                self.client.delete(&path).await?
            }
            // -- Notifications --
            "list_notifications" => {
                let mut params = Vec::new();
                Self::push_query_value(&mut params, "provider", provider.clone());
                Self::push_query_value(&mut params, "page", input.page.map(|v| v.to_string()));
                Self::push_query_value(
                    &mut params,
                    "per_page",
                    input.per_page.map(|v| v.to_string()),
                );
                let path = Self::query_path(format!("{}/notifications", workspace_base), params);
                self.client.get(&path).await?
            }
            "mark_notification_read" => {
                let notification_id = input
                    .notification_id
                    .as_ref()
                    .map(|v| v.trim().to_string())
                    .filter(|v| !v.is_empty())
                    .ok_or_else(|| Error::Validation("notification_id is required".to_string()))?;
                let path = format!("{}/notifications/{}/read", workspace_base, notification_id);
                self.client.put(&path, serde_json::json!({})).await?
            }
            "mark_all_notifications_read" => {
                let path = format!("{}/notifications/read-all", workspace_base);
                self.client.put(&path, serde_json::json!({})).await?
            }
            // -- Links --
            "list_links" => {
                let mut params = Vec::new();
                Self::push_query_value(&mut params, "provider", provider.clone());
                Self::push_query_value(&mut params, "page", input.page.map(|v| v.to_string()));
                Self::push_query_value(
                    &mut params,
                    "per_page",
                    input.per_page.map(|v| v.to_string()),
                );
                let path = Self::query_path(format!("{}/links", workspace_base), params);
                self.client.get(&path).await?
            }
            "create_link" => {
                let source_type = input
                    .source_type
                    .as_ref()
                    .map(|v| v.trim().to_string())
                    .filter(|v| !v.is_empty())
                    .ok_or_else(|| Error::Validation("source_type is required".to_string()))?;
                let source_id = input
                    .source_id
                    .as_ref()
                    .map(|v| v.trim().to_string())
                    .filter(|v| !v.is_empty())
                    .ok_or_else(|| Error::Validation("source_id is required".to_string()))?;
                let target_type = input
                    .target_type
                    .as_ref()
                    .map(|v| v.trim().to_string())
                    .filter(|v| !v.is_empty())
                    .ok_or_else(|| Error::Validation("target_type is required".to_string()))?;
                let target_id = input
                    .target_id
                    .as_ref()
                    .map(|v| v.trim().to_string())
                    .filter(|v| !v.is_empty())
                    .ok_or_else(|| Error::Validation("target_id is required".to_string()))?;
                let path = format!("{}/links", workspace_base);
                self.client
                    .post(
                        &path,
                        serde_json::json!({
                            "source_type": source_type,
                            "source_id": source_id,
                            "target_type": target_type,
                            "target_id": target_id
                        }),
                    )
                    .await?
            }
            "delete_link" => {
                let link_id = input
                    .link_id
                    .as_ref()
                    .map(|v| v.trim().to_string())
                    .filter(|v| !v.is_empty())
                    .ok_or_else(|| Error::Validation("link_id is required".to_string()))?;
                let path = format!("{}/links/{}", workspace_base, link_id);
                self.client.delete(&path).await?
            }
            // -- Automations --
            "list_automations" => {
                let mut params = Vec::new();
                Self::push_query_value(&mut params, "provider", provider.clone());
                Self::push_query_value(&mut params, "page", input.page.map(|v| v.to_string()));
                Self::push_query_value(
                    &mut params,
                    "per_page",
                    input.per_page.map(|v| v.to_string()),
                );
                let path = Self::query_path(format!("{}/automations", workspace_base), params);
                self.client.get(&path).await?
            }
            "create_automation" => {
                let config = input.automation_config.clone().ok_or_else(|| {
                    Error::Validation("automation_config is required".to_string())
                })?;
                let path = format!("{}/automations", workspace_base);
                self.client.post(&path, config).await?
            }
            "update_automation" => {
                let automation_id = input
                    .automation_id
                    .as_ref()
                    .map(|v| v.trim().to_string())
                    .filter(|v| !v.is_empty())
                    .ok_or_else(|| Error::Validation("automation_id is required".to_string()))?;
                let config = input.automation_config.clone().ok_or_else(|| {
                    Error::Validation("automation_config is required".to_string())
                })?;
                let path = format!("{}/automations/{}", workspace_base, automation_id);
                self.client.put(&path, config).await?
            }
            "delete_automation" => {
                let automation_id = input
                    .automation_id
                    .as_ref()
                    .map(|v| v.trim().to_string())
                    .filter(|v| !v.is_empty())
                    .ok_or_else(|| Error::Validation("automation_id is required".to_string()))?;
                let path = format!("{}/automations/{}", workspace_base, automation_id);
                self.client.delete(&path).await?
            }
            // -- Webhook management --
            "register_webhook" => {
                let repo_ref = Self::repo_ref(&input)?;
                let mut params = Vec::new();
                Self::push_query_value(&mut params, "provider", provider.clone());
                let path = Self::query_path(
                    format!(
                        "{}/webhooks/register",
                        Self::repo_base_path(workspace_id, &repo_ref)
                    ),
                    params,
                );
                self.client.post(&path, serde_json::json!({})).await?
            }
            "unregister_webhook" => {
                let repo_ref = Self::repo_ref(&input)?;
                let mut params = Vec::new();
                Self::push_query_value(&mut params, "provider", provider.clone());
                let path = Self::query_path(
                    format!("{}/webhooks", Self::repo_base_path(workspace_id, &repo_ref)),
                    params,
                );
                self.client.delete(&path).await?
            }
            _ => return Err(Error::Validation(format!("Unhandled action '{}'", action))),
        };

        Ok(ToolResult::with_structured(
            Self::success_text(&action, &result),
            result,
        ))
    }

    fn metadata(&self) -> &ToolMetadata {
        static METADATA: std::sync::OnceLock<ToolMetadata> = std::sync::OnceLock::new();
        METADATA.get_or_init(|| ToolMetadata {
            name: "vcs".to_string(),
            title: "VCS".to_string(),
            description: "Unified VCS access for GitHub, GitLab, and Bitbucket. Repos, PRs, issues, commits, branches, tags, tree/blob, search, activity, notifications, links, automations, webhooks, and project linking/ingestion.".to_string(),
            category: ToolCategory::Integrations,
            annotations: ToolAnnotations::destructive(),
            is_pro: false,
            required_tier: None,
        })
    }

    fn input_schema(&self) -> Value {
        SchemaBuilder::new()
            .description("Unified VCS operations across GitHub, GitLab, and Bitbucket. Use repo_ref for full repo paths like group/subgroup/repo.")
            .string_enum("action", "VCS action to execute", VALID_ACTIONS, true)
            .uuid("workspace_id", "Workspace ID (uses default if omitted)", false)
            .string_enum("provider", "Provider filter", VALID_PROVIDERS, false)
            .string("owner", "Repository owner/organization", false)
            .string("repo", "Repository name, or full repo path when owner is omitted", false)
            .string("repo_ref", "Full repository path such as owner/repo or group/subgroup/repo", false)
            .integer("number", "Pull request or issue number", false)
            .string("sha", "Commit SHA", false)
            .string("state", "State filter such as open, closed, merged, or all", false)
            .string("branch", "Branch filter", false)
            .string("query", "Search query or repository search term", false)
            .string("scope", "Cross-repo VCS search scope such as code, issues, or prs", false)
            .string("base", "Compare base ref", false)
            .string("head", "Compare head ref", false)
            .string("ref_name", "Branch, tag, or commit ref", false)
            .string("path", "Repository file path", false)
            .integer("page", "Page number", false)
            .integer("per_page", "Results per page", false)
            .boolean("full", "Run a full sync", false)
            .uuid("project_id", "ContextStream project ID", false)
            .string("project_name", "ContextStream project name", false)
            .string("description", "Project description", false)
            .boolean("create_project", "Create a new project when ingesting", false)
            .boolean("ingest", "Trigger ingestion after linking or import", false)
            .integer("limit", "Limit for code search results", false)
            .object("sync_config", "Repository sync configuration object", false)
            .string("title", "Issue title or merge commit title", false)
            .string("body", "Comment body, review body, or issue body", false)
            .string_enum("event", "PR review event type", &["APPROVE", "REQUEST_CHANGES", "COMMENT"], false)
            .string_enum("merge_method", "Merge strategy", &["merge", "squash", "rebase"], false)
            .string("commit_title", "Custom merge commit title", false)
            .string("commit_message", "Custom merge commit message", false)
            .object("comments", "Inline review comments array for review_pull", false)
            .array("labels", "Issue labels", "string", false)
            .array("assignees", "Issue assignees", "string", false)
            .string("notification_id", "Notification ID for mark_notification_read", false)
            .string("link_id", "Link ID for delete_link", false)
            .string("source_type", "Link source type such as repo, pull_request, issue, or commit", false)
            .string("source_id", "Link source identifier", false)
            .string("target_type", "Link target type such as project, doc, plan, task, todo, decision, or node", false)
            .string("target_id", "Link target identifier", false)
            .string("automation_id", "Automation ID for update or delete", false)
            .object("automation_config", "Automation trigger and action configuration", false)
            .build()
    }
}

pub fn register_vcs_tools(
    registry: &mut crate::registry::ToolRegistry,
    client: ContextStreamClient,
) {
    registry.register("vcs", Arc::new(VcsTool::new(client)));
}
