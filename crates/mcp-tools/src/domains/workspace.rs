//! Workspace domain tools: list, get, create, associate, bootstrap, team_members, index_settings.

use async_trait::async_trait;
use mcp_client::{BootstrapWorkspaceParams, ContextStreamClient, IndexSettingsParams};
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
// Workspaces List Tool
// ============================================================================

/// Input for listing workspaces.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkspacesListInput {
    pub page: Option<i64>,
    pub page_size: Option<i64>,
}

/// Workspaces list tool handler.
pub struct WorkspacesListTool {
    client: ContextStreamClient,
}

impl WorkspacesListTool {
    pub fn new(client: ContextStreamClient) -> Self {
        Self { client }
    }
}

#[async_trait]
impl ToolHandler for WorkspacesListTool {
    async fn execute(&self, input: Value) -> Result<ToolResult> {
        let input: WorkspacesListInput =
            serde_json::from_value(input).map_err(|e| Error::Validation(e.to_string()))?;

        let workspaces = self
            .client
            .list_workspaces(input.page, input.page_size)
            .await?;

        let mut text = String::new();

        if workspaces.is_empty() {
            text.push_str("No workspaces found. Create one with workspaces_create.");
        } else {
            text.push_str(&format!("Found {} workspaces:\n\n", workspaces.len()));

            for (i, ws) in workspaces.iter().enumerate() {
                text.push_str(&format!("{}. **{}** ({})\n", i + 1, ws.name, ws.id));

                if let Some(ref desc) = ws.description {
                    if !desc.is_empty() {
                        text.push_str(&format!("   {}\n", desc));
                    }
                }
            }
        }

        Ok(ToolResult::with_structured(
            text,
            serde_json::to_value(&workspaces).unwrap_or_default(),
        ))
    }

    fn metadata(&self) -> &ToolMetadata {
        static METADATA: std::sync::OnceLock<ToolMetadata> = std::sync::OnceLock::new();
        METADATA.get_or_init(|| ToolMetadata {
            name: "workspaces_list".to_string(),
            title: "List Workspaces".to_string(),
            description: "List all workspaces you have access to.".to_string(),
            category: ToolCategory::Workspace,
            annotations: ToolAnnotations::read_only(),
            is_pro: false,
            required_tier: None,
        })
    }

    fn input_schema(&self) -> Value {
        SchemaBuilder::new()
            .description("List workspaces")
            .integer("page", "Page number", false)
            .integer("page_size", "Results per page", false)
            .build()
    }
}

// ============================================================================
// Workspaces Create Tool
// ============================================================================

/// Input for creating a workspace.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkspacesCreateInput {
    pub name: String,
    pub description: Option<String>,
}

/// Workspaces create tool handler.
pub struct WorkspacesCreateTool {
    client: ContextStreamClient,
}

impl WorkspacesCreateTool {
    pub fn new(client: ContextStreamClient) -> Self {
        Self { client }
    }
}

#[async_trait]
impl ToolHandler for WorkspacesCreateTool {
    async fn execute(&self, input: Value) -> Result<ToolResult> {
        let input: WorkspacesCreateInput =
            serde_json::from_value(input).map_err(|e| Error::Validation(e.to_string()))?;

        if input.name.trim().is_empty() {
            return Err(Error::Validation("name is required".to_string()));
        }

        let workspace = self
            .client
            .create_workspace(&input.name, input.description.as_deref())
            .await?;

        let text = format!(
            "Created workspace: {}\nID: {}",
            workspace.name, workspace.id
        );

        Ok(ToolResult::with_structured(
            text,
            serde_json::to_value(&workspace).unwrap_or_default(),
        ))
    }

    fn metadata(&self) -> &ToolMetadata {
        static METADATA: std::sync::OnceLock<ToolMetadata> = std::sync::OnceLock::new();
        METADATA.get_or_init(|| ToolMetadata {
            name: "workspaces_create".to_string(),
            title: "Create Workspace".to_string(),
            description: "Create a new workspace.".to_string(),
            category: ToolCategory::Workspace,
            annotations: ToolAnnotations::write(),
            is_pro: false,
            required_tier: None,
        })
    }

    fn input_schema(&self) -> Value {
        SchemaBuilder::new()
            .description("Create a workspace")
            .string("name", "Workspace name", true)
            .string("description", "Workspace description", false)
            .build()
    }
}

// ============================================================================
// Unified Workspace Tool
// ============================================================================

/// Valid branch policies.
const VALID_BRANCH_POLICIES: &[&str] =
    &["default_branch_wins", "newest_wins", "feature_branch_wins"];

/// Valid conflict resolutions.
const VALID_CONFLICT_RESOLUTIONS: &[&str] = &["newest_timestamp", "default_branch", "manual"];

/// Input for the unified workspace tool.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkspaceInput {
    pub action: String,
    // Common fields
    pub name: Option<String>,
    pub description: Option<String>,
    pub workspace_id: Option<String>,
    pub page: Option<i64>,
    pub page_size: Option<i64>,
    // Associate fields
    pub folder_path: Option<String>,
    // Bootstrap fields
    pub workspace_name: Option<String>,
    pub context_hint: Option<String>,
    pub auto_index: Option<bool>,
    pub generate_editor_rules: Option<bool>,
    // Index settings fields
    pub auto_sync_enabled: Option<bool>,
    pub allowed_machines: Option<Vec<String>>,
    pub max_machines: Option<i64>,
    pub branch_policy: Option<String>,
    pub conflict_resolution: Option<String>,
}

/// Unified workspace tool handler.
pub struct WorkspaceTool {
    client: ContextStreamClient,
}

impl WorkspaceTool {
    pub fn new(client: ContextStreamClient) -> Self {
        Self { client }
    }
}

#[async_trait]
impl ToolHandler for WorkspaceTool {
    async fn execute(&self, input: Value) -> Result<ToolResult> {
        let input: WorkspaceInput =
            serde_json::from_value(input).map_err(|e| Error::Validation(e.to_string()))?;

        match input.action.to_lowercase().as_str() {
            "list" => {
                let list_input = WorkspacesListInput {
                    page: input.page,
                    page_size: input.page_size,
                };
                let tool = WorkspacesListTool::new(self.client.clone());
                tool.execute(serde_json::to_value(&list_input).unwrap()).await
            }
            "get" => {
                let workspace_id = input.workspace_id
                    .ok_or_else(|| Error::Validation("workspace_id is required for get".to_string()))?;
                let id = Uuid::parse_str(&workspace_id)
                    .map_err(|_| Error::Validation("Invalid workspace_id".to_string()))?;

                match self.client.get_workspace(id).await {
                    Ok(workspace) => {
                        let text = format!("Workspace: {} ({})", workspace.name, workspace.id);
                        Ok(ToolResult::with_structured(text, serde_json::to_value(&workspace).unwrap_or_default()))
                    }
                    Err(ref e) if matches!(e.code(), mcp_types::ErrorCode::NotFound) => {
                        // Workspace ID returned 404 — try listing workspaces and return
                        // guidance so the caller can re-associate.
                        let workspaces = self.client.list_workspaces(None, Some(50)).await
                            .unwrap_or_default();

                        let mut text = format!(
                            "Workspace {} was not found (404). It may have been deleted or the ID is stale.",
                            workspace_id
                        );

                        if workspaces.is_empty() {
                            text.push_str("\nNo workspaces exist. Create one with workspace(action=\"create\", name=\"...\").");
                        } else {
                            text.push_str(&format!("\n\nAvailable workspaces ({}):", workspaces.len()));
                            for ws in &workspaces {
                                text.push_str(&format!("\n  - {} ({})", ws.name, ws.id));
                            }
                            text.push_str(
                                "\n\nTo fix: call init(folder_path=\"...\") with the correct workspace, or run `contextstream-mcp setup` to reconfigure.",
                            );
                        }

                        let structured = serde_json::json!({
                            "error": "not_found",
                            "requested_workspace_id": workspace_id,
                            "available_workspaces": workspaces.iter().map(|ws| {
                                serde_json::json!({
                                    "id": ws.id.to_string(),
                                    "name": ws.name,
                                })
                            }).collect::<Vec<_>>(),
                        });

                        Ok(ToolResult::with_structured(text, structured))
                    }
                    Err(e) => Err(e),
                }
            }
            "create" => {
                let name = input.name.ok_or_else(|| Error::Validation("name is required for create".to_string()))?;
                let create_input = WorkspacesCreateInput {
                    name,
                    description: input.description,
                };
                let tool = WorkspacesCreateTool::new(self.client.clone());
                tool.execute(serde_json::to_value(&create_input).unwrap()).await
            }
            "delete" => {
                let workspace_id = input.workspace_id
                    .ok_or_else(|| Error::Validation("workspace_id is required for delete".to_string()))?;
                let id = Uuid::parse_str(&workspace_id)
                    .map_err(|_| Error::Validation("Invalid workspace_id".to_string()))?;

                let result = self.client.delete_workspace(id).await?;
                Ok(ToolResult::with_structured(
                    format!("Deleted workspace: {}", workspace_id),
                    result
                ))
            }
            "associate" => {
                let workspace_id = input.workspace_id
                    .ok_or_else(|| Error::Validation("workspace_id is required for associate".to_string()))?;
                let folder_path = input.folder_path
                    .ok_or_else(|| Error::Validation("folder_path is required for associate".to_string()))?;
                let id = Uuid::parse_str(&workspace_id)
                    .map_err(|_| Error::Validation("Invalid workspace_id".to_string()))?;

                match self.client.associate_workspace(id, &folder_path).await {
                    Ok(result) => Ok(ToolResult::with_structured(
                        format!("Associated folder '{}' with workspace {}", folder_path, workspace_id),
                        result
                    )),
                    Err(ref e) if matches!(e.code(), mcp_types::ErrorCode::NotFound) => {
                        let text = format!(
                            "Workspace {} not found (404). The workspace may have been deleted or the ID is stale.\n\
                             Run `contextstream-mcp setup` to reconfigure, or use workspace(action=\"list\") to find valid workspaces.",
                            workspace_id
                        );
                        Ok(ToolResult::with_structured(text, serde_json::json!({
                            "error": "not_found",
                            "requested_workspace_id": workspace_id,
                            "folder_path": folder_path,
                        })))
                    }
                    Err(e) => Err(e),
                }
            }
            "bootstrap" => {
                let workspace_name = input.workspace_name
                    .or(input.name)
                    .ok_or_else(|| Error::Validation("workspace_name is required for bootstrap".to_string()))?;
                let params = BootstrapWorkspaceParams {
                    workspace_name,
                    description: input.description,
                    folder_path: input.folder_path,
                    context_hint: input.context_hint,
                    auto_index: input.auto_index,
                    generate_editor_rules: input.generate_editor_rules,
                };
                let result = self.client.bootstrap_workspace(params).await?;
                Ok(ToolResult::with_structured("Workspace bootstrapped successfully.".to_string(), result))
            }
            "team_members" => {
                let workspace_id = input.workspace_id
                    .ok_or_else(|| Error::Validation("workspace_id is required for team_members".to_string()))?;
                let id = Uuid::parse_str(&workspace_id)
                    .map_err(|_| Error::Validation("Invalid workspace_id".to_string()))?;

                let result = self.client.workspace_team_members(id, input.page, input.page_size).await?;
                let count = result.as_array().map(|a| a.len()).unwrap_or(0);
                Ok(ToolResult::with_structured(format!("Found {} team members.", count), result))
            }
            "index_settings" => {
                let workspace_id = input.workspace_id
                    .ok_or_else(|| Error::Validation("workspace_id is required for index_settings".to_string()))?;
                let id = Uuid::parse_str(&workspace_id)
                    .map_err(|_| Error::Validation("Invalid workspace_id".to_string()))?;

                // If any settings are provided, update; otherwise get
                let has_updates = input.auto_sync_enabled.is_some()
                    || input.allowed_machines.is_some()
                    || input.max_machines.is_some()
                    || input.branch_policy.is_some()
                    || input.conflict_resolution.is_some();

                let settings = if has_updates {
                    Some(IndexSettingsParams {
                        auto_sync_enabled: input.auto_sync_enabled,
                        allowed_machines: input.allowed_machines,
                        max_machines: input.max_machines,
                        branch_policy: input.branch_policy,
                        conflict_resolution: input.conflict_resolution,
                    })
                } else {
                    None
                };

                let result = self.client.workspace_index_settings(id, settings).await?;
                let action_text = if has_updates { "updated" } else { "retrieved" };
                Ok(ToolResult::with_structured(format!("Index settings {}.", action_text), result))
            }
            _ => Err(Error::Validation(format!(
                "Unknown action: {}. Available actions: list, get, create, delete, associate, bootstrap, team_members, index_settings.",
                input.action
            ))),
        }
    }

    fn metadata(&self) -> &ToolMetadata {
        static METADATA: std::sync::OnceLock<ToolMetadata> = std::sync::OnceLock::new();
        METADATA.get_or_init(|| ToolMetadata {
            name: "workspace".to_string(),
            title: "Workspace Operations".to_string(),
            description: "Workspace management. Actions: list, get, create, associate (link folder to workspace), bootstrap (create workspace and initialize), team_members (list members with access - team plans only), index_settings (get/update multi-machine sync settings - admin only).".to_string(),
            category: ToolCategory::Workspace,
            annotations: ToolAnnotations::destructive(),
            is_pro: false,
            required_tier: None,
        })
    }

    fn input_schema(&self) -> Value {
        let all_actions = &[
            "list",
            "get",
            "create",
            "delete",
            "associate",
            "bootstrap",
            "team_members",
            "index_settings",
        ];

        SchemaBuilder::new()
            .description("Workspace operations")
            .string_enum("action", "Operation to perform", all_actions, true)
            // Common fields
            .string("name", "Workspace name (for create)", false)
            .string(
                "description",
                "Workspace description (for create/bootstrap)",
                false,
            )
            .uuid("workspace_id", "Workspace ID", false)
            .integer("page", "Page number (for list/team_members)", false)
            .integer(
                "page_size",
                "Results per page (for list/team_members)",
                false,
            )
            // Associate fields
            .string(
                "folder_path",
                "Absolute path to folder (for associate/bootstrap)",
                false,
            )
            // Bootstrap fields
            .string("workspace_name", "Workspace name (for bootstrap)", false)
            .string(
                "context_hint",
                "Context hint for semantic search (for bootstrap)",
                false,
            )
            .boolean(
                "auto_index",
                "Automatically index on creation (for bootstrap)",
                false,
            )
            .boolean(
                "generate_editor_rules",
                "Generate AI editor rules (for bootstrap)",
                false,
            )
            // Index settings fields
            .boolean(
                "auto_sync_enabled",
                "Enable auto-sync from all machines (for index_settings)",
                false,
            )
            .array(
                "allowed_machines",
                "List of allowed machine IDs (for index_settings)",
                "string",
                false,
            )
            .integer(
                "max_machines",
                "Maximum machines allowed to index (for index_settings)",
                false,
            )
            .string_enum(
                "branch_policy",
                "Branch priority policy",
                VALID_BRANCH_POLICIES,
                false,
            )
            .string_enum(
                "conflict_resolution",
                "How to resolve conflicts",
                VALID_CONFLICT_RESOLUTIONS,
                false,
            )
            .build()
    }
}

/// Register all workspace tools.
pub fn register_workspace_tools(
    registry: &mut crate::registry::ToolRegistry,
    client: ContextStreamClient,
) {
    registry.register("workspace", Arc::new(WorkspaceTool::new(client.clone())));
    registry.register(
        "workspaces_list",
        Arc::new(WorkspacesListTool::new(client.clone())),
    );
    registry.register(
        "workspaces_create",
        Arc::new(WorkspacesCreateTool::new(client.clone())),
    );
}

#[cfg(test)]
#[path = "workspace_tests.rs"]
mod tests;
