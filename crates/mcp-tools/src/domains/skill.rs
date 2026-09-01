//! Skill domain tools: list, get, create, update, run, delete, import, export, share.

use async_trait::async_trait;
use mcp_client::{
    ContextStreamClient, CreateSkillParams, ExportSkillParams, ImportSkillParams, RunSkillParams,
    UpdateSkillParams,
};
use mcp_session::SessionManager;
use mcp_types::{
    tool::{ToolAnnotations, ToolCategory, ToolMetadata, ToolResult},
    Error, Result,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{env, path::Path, sync::Arc};
use uuid::Uuid;

// Re-use the string-or-vec deserializer from the session module.
use super::session::deserialize_string_or_vec;

use crate::registry::ToolHandler;
use crate::schema::SchemaBuilder;

/// Register skill tools in the registry.
pub fn register_skill_tools(
    registry: &mut crate::registry::ToolRegistry,
    client: ContextStreamClient,
    session: Arc<SessionManager>,
) {
    let atlas_layer = registry.atlas_layer().clone();
    registry.register(
        "skill",
        Arc::new(SkillTool::with_atlas(
            client.clone(),
            session.clone(),
            atlas_layer,
        )),
    );
}

const VALID_ACTIONS: &[&str] = &[
    "list",
    "get",
    "create",
    "update",
    "supersede",
    "run",
    "delete",
    "import",
    "export",
    "share",
];

/// Input for the skill tool.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkillInput {
    pub action: String,
    // Identity
    pub skill_id: Option<String>,
    pub name: Option<String>,
    pub title: Option<String>,
    pub description: Option<String>,
    pub instruction_body: Option<String>,
    // Activation
    #[serde(default, deserialize_with = "deserialize_string_or_vec")]
    pub trigger_patterns: Option<Vec<String>>,
    pub trigger_regex: Option<String>,
    #[serde(default, deserialize_with = "deserialize_string_or_vec")]
    pub categories: Option<Vec<String>>,
    // Execution
    pub actions: Option<Value>,
    pub capability_manifest: Option<Value>,
    pub params: Option<Value>,
    pub dry_run: Option<bool>,
    pub repair_policy: Option<String>,
    // Scoping
    pub scope: Option<String>,
    pub status: Option<String>,
    pub is_personal: Option<bool>,
    pub priority: Option<i32>,
    // Import/Export
    pub content: Option<String>,
    pub file_path: Option<String>,
    pub format: Option<String>,
    pub source_tool: Option<String>,
    pub source_file: Option<String>,
    #[serde(default, deserialize_with = "deserialize_string_or_vec")]
    pub skill_ids: Option<Vec<String>>,
    // Versioning
    pub change_summary: Option<String>,
    /// Replacement skill (name or id) recorded when superseding a stale skill.
    pub superseded_by: Option<String>,
    // Scoping overrides
    pub workspace_id: Option<String>,
    pub project_id: Option<String>,
    pub target_project: Option<String>,
    // Query
    pub query: Option<String>,
    pub category: Option<String>,
    pub limit: Option<i64>,
    pub folder_path: Option<String>,
}

/// Skill tool handler.
pub struct SkillTool {
    client: ContextStreamClient,
    session: Arc<SessionManager>,
    atlas_layer: mcp_types::atlas_layer::AtlasLayer,
}

impl SkillTool {
    pub fn new(client: ContextStreamClient, session: Arc<SessionManager>) -> Self {
        Self::with_atlas(client, session, mcp_types::atlas_layer::noop_layer())
    }

    pub fn with_atlas(
        client: ContextStreamClient,
        session: Arc<SessionManager>,
        atlas_layer: mcp_types::atlas_layer::AtlasLayer,
    ) -> Self {
        Self {
            client,
            session,
            atlas_layer,
        }
    }

    fn parse_uuid(input: &Option<String>, field: &str) -> Result<Option<Uuid>> {
        match input {
            Some(s) if !s.is_empty() => Uuid::parse_str(s)
                .map(Some)
                .map_err(|_| Error::Validation(format!("Invalid {} UUID: {}", field, s))),
            _ => Ok(None),
        }
    }

    async fn resolve_workspace_id(&self, input: &Option<String>) -> Option<Uuid> {
        if let Some(s) = input {
            if let Ok(id) = Uuid::parse_str(s) {
                return Some(id);
            }
        }
        self.session.state().await.workspace_id
    }

    /// Only parse explicit input — no session fallback.
    /// Used for skills to avoid auto-tagging with session project/workspace.
    fn parse_explicit_uuid(input: &Option<String>) -> Option<Uuid> {
        input
            .as_ref()
            .filter(|s| !s.is_empty())
            .and_then(|s| Uuid::parse_str(s).ok())
    }

    fn normalize_project_selector(value: &str) -> String {
        value
            .chars()
            .filter(|ch| ch.is_ascii_alphanumeric())
            .flat_map(|ch| ch.to_lowercase())
            .collect()
    }

    async fn resolve_target_project_id(&self, input: &SkillInput) -> Result<Option<Uuid>> {
        if let Some(project_id) = Self::parse_explicit_uuid(&input.project_id) {
            return Ok(Some(project_id));
        }

        let Some(target_name) = input
            .target_project
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
        else {
            return Ok(None);
        };

        let normalized_target = Self::normalize_project_selector(target_name);
        let session_state = self.session.state().await;
        if let Some(project_id) = session_state.project_id {
            let current_folder_matches = session_state
                .folder_path
                .as_deref()
                .and_then(|path| Path::new(path).file_name().and_then(|name| name.to_str()))
                .map(Self::normalize_project_selector)
                .map(|folder_name| folder_name == normalized_target)
                .unwrap_or(false);
            if current_folder_matches {
                return Ok(Some(project_id));
            }
        }

        let related_projects = self.session.get_project_relations().await;
        if related_projects.is_empty() {
            let current_folder = session_state
                .folder_path
                .as_deref()
                .and_then(|path| Path::new(path).file_name().and_then(|name| name.to_str()))
                .unwrap_or("current project");
            return Err(Error::Validation(format!(
                "target_project '{}' requires init from a multi-project folder first. If you are already inside '{}', omit target_project and use the current project directly.",
                target_name, current_folder
            )));
        }

        if let Some(related) = related_projects.iter().find_map(|(key, info)| {
            let key_norm = Self::normalize_project_selector(key);
            let name_norm = Self::normalize_project_selector(&info.name);
            if key_norm == normalized_target
                || name_norm == normalized_target
                || key_norm.contains(&normalized_target)
                || name_norm.contains(&normalized_target)
            {
                Some(info.clone())
            } else {
                None
            }
        }) {
            return Uuid::parse_str(&related.project_id).map(Some).map_err(|_| {
                Error::Validation(format!(
                    "Resolved project '{}' has an invalid project_id",
                    target_name
                ))
            });
        }

        let mut available = related_projects
            .iter()
            .map(|(key, info)| format!("{} ({})", key, info.relation.as_str()))
            .collect::<Vec<_>>();
        available.sort();

        Err(Error::Validation(format!(
            "Unknown target_project '{}'. Available related projects: {}",
            target_name,
            if available.is_empty() {
                "none".to_string()
            } else {
                available.join(", ")
            }
        )))
    }

    async fn resolve_session_id(&self) -> Option<String> {
        self.session.state().await.session_id.clone()
    }

    async fn resolve_skill_id_for_action(&self, input: &SkillInput, action: &str) -> Result<Uuid> {
        if let Some(id) = Self::parse_uuid(&input.skill_id, "skill_id")? {
            return Ok(id);
        }

        let name = input
            .name
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .ok_or_else(|| {
                Error::Validation(format!(
                    "Either skill_id or name is required for '{}'",
                    action
                ))
            })?;

        let workspace_id = self.resolve_workspace_id(&input.workspace_id).await;
        let project_id = self.resolve_target_project_id(input).await?;
        let result = self
            .client
            .list_skills(
                workspace_id,
                project_id,
                None,
                None,
                None,
                Some(name.to_string()),
                None,
                Some(25),
            )
            .await?;

        let mut ranked = rank_skill_candidates(name, &result);
        if ranked.is_empty() && project_id.is_some() && input.target_project.is_none() {
            let account_result = self
                .client
                .list_skills(
                    workspace_id,
                    None,
                    None,
                    None,
                    None,
                    Some(name.to_string()),
                    None,
                    Some(25),
                )
                .await?;
            ranked = rank_skill_candidates(name, &account_result);
        }
        if ranked.is_empty() {
            if let Some(workspace_id) = workspace_id {
                let matched = self
                    .client
                    .match_skills(workspace_id, name, Some(5))
                    .await
                    .unwrap_or_else(|_| serde_json::json!([]));
                let matched_candidates = skill_candidates_from_items(&matched);
                if matched_candidates.len() == 1 {
                    return Ok(matched_candidates[0].id);
                }
                if !matched_candidates.is_empty() {
                    let mut lines = vec![format!(
                        "Skill name '{}' is ambiguous. Provide skill_id or a more specific name. Matching skills:",
                        name
                    )];
                    for candidate in matched_candidates.into_iter().take(5) {
                        lines.push(format!(
                            "- {} ({}) [{}|{}] id={}",
                            candidate.title,
                            candidate.name,
                            candidate.scope,
                            candidate.status,
                            candidate.id
                        ));
                    }
                    return Err(Error::Validation(lines.join("\n")));
                }
            }
        }
        if ranked.is_empty() {
            return Err(Error::Validation(format!(
                "Skill '{}' not found. Provide a different name or skill_id.",
                name
            )));
        }

        let best_score = ranked
            .first()
            .map(|candidate| candidate.score)
            .unwrap_or(100);
        let best = ranked
            .iter()
            .filter(|candidate| candidate.score == best_score)
            .collect::<Vec<_>>();

        if best.len() > 1 {
            let mut lines = vec![format!(
                "Skill name '{}' is ambiguous. Provide skill_id or use a more specific name. Candidates:",
                name
            )];
            for candidate in best.into_iter().take(5) {
                lines.push(format!(
                    "- {} ({}) [{}|{}] id={}",
                    candidate.title,
                    candidate.name,
                    candidate.scope,
                    candidate.status,
                    candidate.id
                ));
            }
            return Err(Error::Validation(lines.join("\n")));
        }

        Ok(ranked[0].id)
    }

    fn command_exists(name: &str) -> bool {
        let Some(path_var) = env::var_os("PATH") else {
            return false;
        };

        for base in env::split_paths(&path_var) {
            let candidate = base.join(name);
            if candidate.is_file() {
                return true;
            }

            #[cfg(target_os = "windows")]
            for ext in [".exe", ".cmd", ".bat"] {
                let candidate = base.join(format!("{}{}", name, ext));
                if candidate.is_file() {
                    return true;
                }
            }
        }

        false
    }

    fn command_version(name: &str) -> Option<String> {
        let output = std::process::Command::new(name)
            .arg("--version")
            .output()
            .ok()?;
        if !output.status.success() {
            return None;
        }

        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        let text = stdout
            .lines()
            .find(|line| !line.trim().is_empty())
            .or_else(|| stderr.lines().find(|line| !line.trim().is_empty()))?;

        Some(text.trim().to_string())
    }

    fn probe_harness_capabilities() -> Value {
        let mut runtimes = Vec::new();
        let mut tools = Vec::new();
        let mut package_managers = Vec::new();

        for runtime in ["bash", "python3", "python", "node"] {
            if Self::command_exists(runtime) {
                runtimes.push(serde_json::json!({
                    "name": runtime,
                    "version": Self::command_version(runtime),
                }));
            }
        }

        for tool in ["git", "cargo", "pip", "pip3", "npm", "pnpm"] {
            if Self::command_exists(tool) {
                let version = Self::command_version(tool);
                if matches!(tool, "pip" | "pip3" | "npm" | "pnpm") {
                    package_managers.push(tool.to_string());
                }
                tools.push(serde_json::json!({
                    "name": tool,
                    "version": version,
                }));
            }
        }

        let browser = Self::command_exists("xdg-open")
            || Self::command_exists("open")
            || Self::command_exists("start");
        let display = env::var_os("DISPLAY").is_some()
            || env::var_os("WAYLAND_DISPLAY").is_some()
            || cfg!(target_os = "macos")
            || cfg!(target_os = "windows");

        serde_json::json!({
            "runtimes": runtimes,
            "tools": tools,
            "filesystem": {
                "read": true,
                "write": true,
            },
            "network": {
                "enabled": true,
                "unrestricted": true,
                "domains": [],
            },
            "ui": {
                "browser": browser,
                "display": display,
            },
            "repair": {
                "package_managers": package_managers,
                "directory_creation": true,
            }
        })
    }

    async fn handle_list(&self, input: &SkillInput) -> Result<ToolResult> {
        let workspace_id = self.resolve_workspace_id(&input.workspace_id).await;
        let project_id = self.resolve_target_project_id(input).await?;

        // P1 #7 — SkillsHot warm cache. 5 min TTL. Cache key folds
        // every input that affects the assembled result (scope, status,
        // category, query, is_personal, limit, target_project) so
        // distinct filter shapes don't collide. Cache returns from the
        // pre-formatting state — text formatting is cheap and the same
        // cached value can serve multiple format requests.
        if let Some(ws) = workspace_id {
            let filter_str = format!(
                "scope={};status={};category={};query={};personal={};limit={};target={}",
                input.scope.as_deref().unwrap_or(""),
                input.status.as_deref().unwrap_or(""),
                input.category.as_deref().unwrap_or(""),
                input.query.as_deref().unwrap_or(""),
                input.is_personal.map(|b| b.to_string()).unwrap_or_default(),
                input.limit.unwrap_or(0),
                input.target_project.as_deref().unwrap_or(""),
            );
            let user_scope_token = super::atlas_warm_cache::current_user_scope_token();
            let scope = mcp_types::atlas_layer::AtlasFederationScope {
                workspace_id: ws,
                project_id,
                scope_hash: super::atlas_warm_cache::scope_hash_for_list(
                    ws,
                    user_scope_token.as_deref(),
                    project_id,
                    "skills",
                    Some(&filter_str),
                ),
                user_scope: user_scope_token,
            };
            if let Some(bundle) = super::atlas_warm_cache::try_lookup(
                &self.atlas_layer,
                mcp_types::atlas_layer::AtlasWarmCacheKind::SkillsHot,
                scope,
                300,
            )
            .await
            {
                let cached_result = bundle.payload;
                return Ok(format_skills_list_result(&cached_result));
            }
        }

        let mut result = self
            .client
            .list_skills(
                workspace_id,
                project_id,
                input.scope.clone(),
                input.status.clone(),
                input.category.clone(),
                input.query.clone(),
                input.is_personal,
                input.limit,
            )
            .await?;

        // Agents often pass the current project_id as ambient scope, but skills
        // are portable by default. Merge account-level skills so a project-scoped
        // list cannot hide personal/team operational skills.
        if project_id.is_some() && input.target_project.is_none() {
            let account_result = self
                .client
                .list_skills(
                    workspace_id,
                    None,
                    input.scope.clone(),
                    input.status.clone(),
                    input.category.clone(),
                    input.query.clone(),
                    input.is_personal,
                    input.limit,
                )
                .await?;
            merge_skill_items(&mut result, &account_result, input);
        }

        // The list endpoint search is exact-ish and can miss trigger/category
        // phrases like "prod access". The matcher is the source of truth for
        // natural-language skill discovery, so fold its hits into list output.
        if let Some(query) = input
            .query
            .as_deref()
            .map(str::trim)
            .filter(|query| !query.is_empty())
        {
            if let Some(workspace_id) = workspace_id {
                let match_limit = input
                    .limit
                    .map(|limit| limit.clamp(1, 100) as i32)
                    .unwrap_or(10);
                let matched = self
                    .client
                    .match_skills(workspace_id, query, Some(match_limit))
                    .await
                    .unwrap_or_else(|_| serde_json::json!([]));
                merge_skill_items(&mut result, &matched, input);
            }
        }

        let count = result
            .get("items")
            .and_then(|i| i.as_array())
            .map(|a| a.len())
            .unwrap_or(0);

        // P1 #7 — write-back: deposit the assembled result for the
        // next caller. Same scope_hash as the lookup above. The
        // formatting helper is shared so the format on miss matches
        // the format on hit.
        if let Some(ws) = workspace_id {
            let filter_str = format!(
                "scope={};status={};category={};query={};personal={};limit={};target={}",
                input.scope.as_deref().unwrap_or(""),
                input.status.as_deref().unwrap_or(""),
                input.category.as_deref().unwrap_or(""),
                input.query.as_deref().unwrap_or(""),
                input.is_personal.map(|b| b.to_string()).unwrap_or_default(),
                input.limit.unwrap_or(0),
                input.target_project.as_deref().unwrap_or(""),
            );
            let user_scope_token = super::atlas_warm_cache::current_user_scope_token();
            let scope = mcp_types::atlas_layer::AtlasFederationScope {
                workspace_id: ws,
                project_id,
                scope_hash: super::atlas_warm_cache::scope_hash_for_list(
                    ws,
                    user_scope_token.as_deref(),
                    project_id,
                    "skills",
                    Some(&filter_str),
                ),
                user_scope: user_scope_token,
            };
            super::atlas_warm_cache::put_in_background(
                self.atlas_layer.clone(),
                mcp_types::atlas_layer::AtlasWarmCacheKind::SkillsHot,
                scope,
                result.clone(),
            );
        }
        let _ = count; // count consumed by formatter below

        Ok(format_skills_list_result(&result))
    }

    async fn handle_get(&self, input: &SkillInput) -> Result<ToolResult> {
        let skill_id = self.resolve_skill_id_for_action(input, "get").await?;
        let skill_data = self.client.get_skill(skill_id).await?;
        let text = format_skill_detail(&skill_data);
        Ok(ToolResult::with_structured(text, skill_data))
    }

    async fn handle_create(&self, input: &SkillInput) -> Result<ToolResult> {
        let name = input
            .name
            .as_ref()
            .ok_or_else(|| Error::Validation("'name' is required for create".to_string()))?;
        let title = input.title.as_ref().unwrap_or(name);
        let instruction_body = input.instruction_body.as_ref().ok_or_else(|| {
            Error::Validation("'instruction_body' is required for create".to_string())
        })?;
        let trigger_patterns = input.trigger_patterns.clone().unwrap_or_default();
        if trigger_patterns.is_empty()
            && input
                .trigger_regex
                .as_deref()
                .unwrap_or("")
                .trim()
                .is_empty()
        {
            return Err(Error::Validation(
                "Create skill requires at least one trigger pattern or trigger_regex.".to_string(),
            ));
        }
        if instruction_body.trim().len() < 40 {
            return Err(Error::Validation(
                "Create skill requires instruction_body with at least 40 characters.".to_string(),
            ));
        }
        let project_id = self.resolve_target_project_id(input).await?;

        let result = self
            .client
            .create_skill(CreateSkillParams {
                name: name.clone(),
                title: title.clone(),
                instruction_body: instruction_body.clone(),
                description: input.description.clone(),
                trigger_patterns: Some(trigger_patterns),
                trigger_regex: input.trigger_regex.clone(),
                categories: input.categories.clone(),
                actions: input.actions.clone(),
                capability_manifest: input.capability_manifest.clone(),
                scope: input.scope.clone(),
                is_personal: input.is_personal,
                priority: input.priority,
                status: input.status.clone().or_else(|| Some("active".to_string())),
                source_tool: input.source_tool.clone(),
                source_file: input.source_file.clone(),
                // For team skills, resolve workspace from session; for personal, only use explicit
                workspace_id: if input.scope.as_deref() == Some("team") {
                    self.resolve_workspace_id(&input.workspace_id).await
                } else {
                    Self::parse_explicit_uuid(&input.workspace_id)
                },
                project_id,
            })
            .await?;

        let id = result.get("id").and_then(|v| v.as_str()).unwrap_or("?");

        Ok(ToolResult::text(format!(
            "Skill '{}' created (id={}).",
            name, id
        )))
    }

    async fn handle_update(&self, input: &SkillInput) -> Result<ToolResult> {
        let skill_id = self.resolve_skill_id_for_action(input, "update").await?;

        let result = self
            .client
            .update_skill(
                skill_id,
                UpdateSkillParams {
                    title: input.title.clone(),
                    description: input.description.clone(),
                    instruction_body: input.instruction_body.clone(),
                    trigger_patterns: input.trigger_patterns.clone(),
                    trigger_regex: input.trigger_regex.clone(),
                    categories: input.categories.clone(),
                    actions: input.actions.clone(),
                    capability_manifest: input.capability_manifest.clone(),
                    scope: input.scope.clone(),
                    status: input.status.clone(),
                    is_personal: input.is_personal,
                    priority: input.priority,
                    change_summary: input.change_summary.clone(),
                },
            )
            .await?;

        let version = result.get("version").and_then(|v| v.as_i64()).unwrap_or(0);

        Ok(ToolResult::text(format!(
            "Skill {} updated (version={}).",
            skill_id, version
        )))
    }

    /// Retire a stale skill: set status=archived so it stops surfacing in
    /// matches/context and skill search. Records the replacement in the
    /// version-history change summary — no schema change required.
    async fn handle_supersede(&self, input: &SkillInput) -> Result<ToolResult> {
        let skill_id = self.resolve_skill_id_for_action(input, "supersede").await?;

        let change_summary = input
            .superseded_by
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(|replacement| format!("Superseded by {}", replacement))
            .or_else(|| input.change_summary.clone())
            .unwrap_or_else(|| "Superseded".to_string());

        self.client
            .update_skill(
                skill_id,
                UpdateSkillParams {
                    title: None,
                    description: None,
                    instruction_body: None,
                    trigger_patterns: None,
                    trigger_regex: None,
                    categories: None,
                    actions: None,
                    capability_manifest: None,
                    scope: None,
                    status: Some("archived".to_string()),
                    is_personal: None,
                    priority: None,
                    change_summary: Some(change_summary.clone()),
                },
            )
            .await?;

        Ok(ToolResult::text(format!(
            "Skill {} superseded (status=archived; {}). It will no longer surface in matched skills or skill search.",
            skill_id, change_summary
        )))
    }

    async fn handle_run(&self, input: &SkillInput) -> Result<ToolResult> {
        let resolved_id = self.resolve_skill_id_for_action(input, "run").await?;

        let result = self
            .client
            .run_skill(
                resolved_id,
                RunSkillParams {
                    params: input.params.clone(),
                    session_id: self.resolve_session_id().await,
                    dry_run: input.dry_run,
                    harness_capabilities: Some(Self::probe_harness_capabilities()),
                    repair_policy: input.repair_policy.clone(),
                },
            )
            .await?;

        let text = format_run_result(&result);
        Ok(ToolResult::with_structured(text, result))
    }

    async fn handle_delete(&self, input: &SkillInput) -> Result<ToolResult> {
        let skill_id = self.resolve_skill_id_for_action(input, "delete").await?;

        self.client.delete_skill(skill_id).await?;

        Ok(ToolResult::text(format!("Skill {} deleted.", skill_id)))
    }

    async fn handle_import(&self, input: &SkillInput) -> Result<ToolResult> {
        // Read content from file_path if provided, otherwise use content field
        let content = if let Some(file_path) = &input.file_path {
            tokio::fs::read_to_string(file_path)
                .await
                .map_err(|e| Error::Validation(format!("Failed to read file: {}", e)))?
        } else if let Some(content) = &input.content {
            content.clone()
        } else {
            return Err(Error::Validation(
                "Either 'content' or 'file_path' is required for import".to_string(),
            ));
        };

        let result = self
            .client
            .import_skills(ImportSkillParams {
                content,
                format: input.format.clone(),
                source_tool: input.source_tool.clone(),
                source_file: input.source_file.clone().or(input.file_path.clone()),
                scope: input.scope.clone(),
                workspace_id: self.resolve_workspace_id(&input.workspace_id).await,
            })
            .await?;

        let imported = result.get("imported").and_then(|v| v.as_u64()).unwrap_or(0);
        let skipped = result.get("skipped").and_then(|v| v.as_u64()).unwrap_or(0);

        Ok(ToolResult::text(format!(
            "Import complete: {} imported, {} skipped (duplicates).",
            imported, skipped
        )))
    }

    async fn handle_export(&self, input: &SkillInput) -> Result<ToolResult> {
        let skill_ids = input.skill_ids.as_ref().map(|ids| {
            ids.iter()
                .filter_map(|s| Uuid::parse_str(s).ok())
                .collect::<Vec<_>>()
        });

        let result = self
            .client
            .export_skills(ExportSkillParams {
                skill_ids,
                format: input.format.clone(),
                scope: input.scope.clone(),
                workspace_id: self.resolve_workspace_id(&input.workspace_id).await,
            })
            .await?;

        // Return the exported content directly
        if let Some(content) = result.get("content").and_then(|v| v.as_str()) {
            Ok(ToolResult::text(content.to_string()))
        } else {
            let text = serde_json::to_string_pretty(&result).unwrap_or_default();
            Ok(ToolResult::text(text))
        }
    }

    async fn handle_share(&self, input: &SkillInput) -> Result<ToolResult> {
        let skill_id = self.resolve_skill_id_for_action(input, "share").await?;
        let scope = input
            .scope
            .as_ref()
            .ok_or_else(|| Error::Validation("'scope' is required for share".to_string()))?
            .trim()
            .to_ascii_lowercase();
        if !matches!(scope.as_str(), "team" | "public") {
            return Err(Error::Validation(
                "share only accepts scope='team' or scope='public'. Use update(action='update') for personal/draft scope changes."
                    .to_string(),
            ));
        }
        if scope == "team"
            && self
                .resolve_workspace_id(&input.workspace_id)
                .await
                .is_none()
        {
            return Err(Error::Validation(
                "scope='team' requires an active workspace. Run init(folder_path=\"...\") or pass workspace_id."
                    .to_string(),
            ));
        }

        let result = self.client.share_skill(skill_id, &scope).await?;

        let new_scope = result
            .get("scope")
            .and_then(|v| v.as_str())
            .unwrap_or(scope.as_str());

        Ok(ToolResult::text(format!(
            "Skill {} shared with scope={}.",
            skill_id, new_scope
        )))
    }
}

#[derive(Debug, Clone)]
struct RankedSkillCandidate {
    id: Uuid,
    name: String,
    title: String,
    scope: String,
    status: String,
    score: u8,
}

fn compute_skill_match_score(query: &str, name: &str, title: &str) -> Option<u8> {
    let q = query.trim().to_lowercase();
    if q.is_empty() {
        return None;
    }

    let n = name.trim().to_lowercase();
    let t = title.trim().to_lowercase();

    if n == q {
        return Some(0);
    }
    if t == q {
        return Some(1);
    }
    if n.contains(&q) {
        return Some(2);
    }
    if t.contains(&q) {
        return Some(3);
    }
    None
}

fn rank_skill_candidates(query: &str, result: &Value) -> Vec<RankedSkillCandidate> {
    let mut candidates = result
        .get("items")
        .and_then(|items| items.as_array())
        .into_iter()
        .flatten()
        .filter_map(|item| {
            let id = item
                .get("id")
                .and_then(|value| value.as_str())
                .and_then(|value| Uuid::parse_str(value).ok())?;
            let name = item
                .get("name")
                .and_then(|value| value.as_str())
                .unwrap_or("?")
                .to_string();
            let title = item
                .get("title")
                .and_then(|value| value.as_str())
                .unwrap_or(name.as_str())
                .to_string();
            let score = compute_skill_match_score(query, &name, &title)?;
            let scope = item
                .get("scope")
                .and_then(|value| value.as_str())
                .unwrap_or("?")
                .to_string();
            let status = item
                .get("status")
                .and_then(|value| value.as_str())
                .unwrap_or("?")
                .to_string();
            Some(RankedSkillCandidate {
                id,
                name,
                title,
                scope,
                status,
                score,
            })
        })
        .collect::<Vec<_>>();

    candidates.sort_by(|a, b| {
        a.score
            .cmp(&b.score)
            .then_with(|| a.name.cmp(&b.name))
            .then_with(|| a.title.cmp(&b.title))
            .then_with(|| a.id.cmp(&b.id))
    });
    candidates
}

fn skill_items(value: &Value) -> Vec<Value> {
    if let Some(items) = value.get("items").and_then(|items| items.as_array()) {
        return items.clone();
    }
    value.as_array().cloned().unwrap_or_default()
}

fn skill_item_key(item: &Value) -> Option<String> {
    item.get("id")
        .or_else(|| item.get("skill_id"))
        .and_then(|value| value.as_str())
        .filter(|value| !value.trim().is_empty())
        .map(|value| format!("id:{value}"))
        .or_else(|| {
            item.get("name")
                .and_then(|value| value.as_str())
                .filter(|value| !value.trim().is_empty())
                .map(|value| format!("name:{value}"))
        })
}

fn skill_item_matches_filters(item: &Value, input: &SkillInput) -> bool {
    if let Some(scope) = input
        .scope
        .as_deref()
        .map(str::trim)
        .filter(|scope| !scope.is_empty() && *scope != "all")
    {
        if item.get("scope").and_then(|value| value.as_str()) != Some(scope) {
            return false;
        }
    }

    if let Some(status) = input
        .status
        .as_deref()
        .map(str::trim)
        .filter(|status| !status.is_empty())
    {
        if item.get("status").and_then(|value| value.as_str()) != Some(status) {
            return false;
        }
    }

    if let Some(is_personal) = input.is_personal {
        let item_is_personal = item
            .get("is_personal")
            .and_then(|value| value.as_bool())
            .unwrap_or_else(|| {
                item.get("scope").and_then(|value| value.as_str()) == Some("personal")
            });
        if item_is_personal != is_personal {
            return false;
        }
    }

    if let Some(category) = input
        .category
        .as_deref()
        .map(str::trim)
        .filter(|category| !category.is_empty())
    {
        let matches_category = item
            .get("categories")
            .and_then(|value| value.as_array())
            .map(|categories| {
                categories
                    .iter()
                    .filter_map(|value| value.as_str())
                    .any(|value| value == category)
            })
            .unwrap_or(false);
        if !matches_category {
            return false;
        }
    }

    true
}

fn merge_skill_items(result: &mut Value, extra: &Value, input: &SkillInput) {
    let mut items = result
        .get("items")
        .and_then(|items| items.as_array())
        .cloned()
        .unwrap_or_default();

    let mut seen = items
        .iter()
        .filter_map(skill_item_key)
        .collect::<std::collections::HashSet<_>>();

    for item in skill_items(extra) {
        if !skill_item_matches_filters(&item, input) {
            continue;
        }
        let Some(key) = skill_item_key(&item) else {
            continue;
        };
        if seen.insert(key) {
            items.push(item);
        }
    }

    if let Some(limit) = input.limit.map(|limit| limit.clamp(1, 100) as usize) {
        items.truncate(limit);
    }

    if !result.is_object() {
        *result = serde_json::json!({});
    }
    if let Some(obj) = result.as_object_mut() {
        obj.insert("items".to_string(), Value::Array(items));
    }
}

fn skill_candidates_from_items(value: &Value) -> Vec<RankedSkillCandidate> {
    skill_items(value)
        .into_iter()
        .filter_map(|item| {
            let id = item
                .get("id")
                .or_else(|| item.get("skill_id"))
                .and_then(|value| value.as_str())
                .and_then(|value| Uuid::parse_str(value).ok())?;
            let name = item
                .get("name")
                .and_then(|value| value.as_str())
                .unwrap_or("?")
                .to_string();
            let title = item
                .get("title")
                .and_then(|value| value.as_str())
                .unwrap_or(name.as_str())
                .to_string();
            let scope = item
                .get("scope")
                .and_then(|value| value.as_str())
                .unwrap_or("?")
                .to_string();
            let status = item
                .get("status")
                .and_then(|value| value.as_str())
                .unwrap_or("?")
                .to_string();
            Some(RankedSkillCandidate {
                id,
                name,
                title,
                scope,
                status,
                score: 10,
            })
        })
        .collect()
}

#[async_trait]
impl ToolHandler for SkillTool {
    async fn execute(&self, input: Value) -> Result<ToolResult> {
        let input: SkillInput =
            serde_json::from_value(input).map_err(|e| Error::Validation(e.to_string()))?;

        let action = input.action.to_lowercase();
        if !VALID_ACTIONS.contains(&action.as_str()) {
            return Err(Error::Validation(format!(
                "Invalid action: '{}'. Valid actions: {}",
                action,
                VALID_ACTIONS.join(", ")
            )));
        }

        match action.as_str() {
            "list" => self.handle_list(&input).await,
            "get" => self.handle_get(&input).await,
            "create" => self.handle_create(&input).await,
            "update" => self.handle_update(&input).await,
            "supersede" => self.handle_supersede(&input).await,
            "run" => self.handle_run(&input).await,
            "delete" => self.handle_delete(&input).await,
            "import" => self.handle_import(&input).await,
            "export" => self.handle_export(&input).await,
            "share" => self.handle_share(&input).await,
            _ => unreachable!(),
        }
    }

    fn metadata(&self) -> &ToolMetadata {
        static META: std::sync::OnceLock<ToolMetadata> = std::sync::OnceLock::new();
        META.get_or_init(|| ToolMetadata {
            name: "skill".to_string(),
            title: "Skill".to_string(),
            description: "Manage and execute reusable skills (instruction + action bundles). Skills are portable across projects, sessions, and tools. Reuse the current project_id from init/context for project-scoped skills instead of guessing. Use 'list' to browse, 'create' to define, 'run' to execute, 'import' to bring skills from other tools, 'supersede' to retire a stale skill (archives it so it stops surfacing).".to_string(),
            category: ToolCategory::Memory,
            annotations: ToolAnnotations::destructive(),
            is_pro: false,
            required_tier: None,
        })
    }

    fn input_schema(&self) -> Value {
        SchemaBuilder::new()
            .description("Manage reusable skills (instruction + action bundles)")
            .string_enum(
                "action",
                "The action to perform",
                &[
                    "list", "get", "create", "update", "supersede", "run", "delete", "import",
                    "export", "share",
                ],
                true,
            )
            .string("skill_id", "Skill ID (UUID)", false)
            .string("name", "Skill name (slug, e.g. 'deploy-checker')", false)
            .string("title", "Skill display title", false)
            .string("description", "Skill description", false)
            .string(
                "instruction_body",
                "Markdown instruction text (the prompt)",
                false,
            )
            .array(
                "trigger_patterns",
                "Keywords/phrases for auto-activation",
                "string",
                false,
            )
            .string(
                "trigger_regex",
                "Optional regex for advanced trigger matching",
                false,
            )
            .array(
                "categories",
                "Tags for discovery/filtering",
                "string",
                false,
            )
            .object(
                "actions",
                "Action steps array [{type, tool, params, ...}]",
                false,
            )
            .object(
                "capability_manifest",
                "Structured capability manifest describing runtime, tools, filesystem, network, UI, and repair requirements",
                false,
            )
            .object("params", "Parameters passed to skill execution", false)
            .boolean("dry_run", "Preview execution without running", false)
            .string_enum(
                "repair_policy",
                "Override repair behavior for this run",
                &["none", "suggest", "auto"],
                false,
            )
            .string_enum(
                "scope",
                "Visibility scope. For action='share', only team or public are allowed.",
                &["personal", "team", "public", "all"],
                false,
            )
            .string_enum(
                "status",
                "Skill status. Defaults to 'active' on create when omitted; pass 'draft' to save as a draft.",
                &["active", "draft", "archived"],
                false,
            )
            .string(
                "superseded_by",
                "Replacement skill (name or id), recorded when action='supersede'",
                false,
            )
            .boolean("is_personal", "Whether skill is personal", false)
            .integer(
                "priority",
                "Skill priority 0-100 (higher = matched first)",
                false,
            )
            .string("content", "Content string for import", false)
            .string("file_path", "Local file path for import", false)
            .string_enum(
                "format",
                "Import/export format",
                &[
                    "auto",
                    "json",
                    "markdown",
                    "skills_md",
                    "cursorrules",
                    "claude_md",
                    "aider",
                    "zip",
                ],
                false,
            )
            .string(
                "source_tool",
                "Source tool name (for import provenance)",
                false,
            )
            .string(
                "source_file",
                "Source filename (for import provenance)",
                false,
            )
            .array("skill_ids", "Skill IDs for export", "string", false)
            .string(
                "change_summary",
                "Summary of changes (for version history)",
                false,
            )
            .string(
                "workspace_id",
                "Workspace ID (UUID). Reuse the current workspace_id returned by init/context when overriding session scope.",
                false,
            )
            .string(
                "project_id",
                "Project ID (UUID). For project-scoped skills, pass the current project_id returned by init/context instead of guessing.",
                false,
            )
            .string(
                "target_project",
                "Target child project by folder name or project name (e.g. 'contextstream', 'mcp-server'). Use this only after init from a multi-project parent folder.",
                false,
            )
            .string("query", "Search query", false)
            .string("category", "Filter by category tag", false)
            .integer("limit", "Max results to return", false)
            .build()
    }
}

/// Format a skill's detail as human-readable text.
/// Shared formatter for `skill(action="list")` output. Used both on
/// the cold-path (after assembling `result` from list_skills + merge +
/// match calls) AND on the warm-cache hit path (formatting from
/// the cached `result` Value directly). Keeping these in sync
/// matters: a hit and a miss for the same scope_hash should produce
/// indistinguishable user-facing text.
fn format_skills_list_result(result: &Value) -> ToolResult {
    let count = result
        .get("items")
        .and_then(|v| v.as_array())
        .map(|a| a.len())
        .unwrap_or(0);
    let mut text = format!("Found {} skill(s).\n", count);
    if let Some(items) = result.get("items").and_then(|i| i.as_array()) {
        for item in items {
            let name = item.get("name").and_then(|v| v.as_str()).unwrap_or("?");
            let title = item.get("title").and_then(|v| v.as_str()).unwrap_or("?");
            let scope = item.get("scope").and_then(|v| v.as_str()).unwrap_or("?");
            let status = item.get("status").and_then(|v| v.as_str()).unwrap_or("?");
            let id = item.get("id").and_then(|v| v.as_str()).unwrap_or("?");
            let governance = format_skill_governance(item);
            text.push_str(&format!(
                "- {} ({}) [{}|{}] id={}{}\n",
                title, name, scope, status, id, governance
            ));
        }
    }
    text.push_str(
        "Tip: team sharing uses skill(action=\"share\", scope=\"team\"|\"public\"); prefer governance fields when choosing shared skills.\n",
    );
    ToolResult::text(text)
}

fn format_skill_governance(skill: &Value) -> String {
    let owner = skill
        .get("owner_user_id")
        .or_else(|| skill.get("created_by"))
        .or_else(|| skill.get("author_user_id"))
        .and_then(|v| v.as_str())
        .map(|v| format!(" owner={}", v));
    let workspace = skill
        .get("workspace_id")
        .and_then(|v| v.as_str())
        .map(|v| format!(" workspace={}", v));
    let project = skill
        .get("project_id")
        .and_then(|v| v.as_str())
        .map(|v| format!(" project={}", v));
    let visibility = skill
        .get("visibility")
        .or_else(|| skill.get("sharing"))
        .and_then(|v| v.as_str())
        .map(|v| format!(" visibility={}", v));
    let mut parts = Vec::new();
    if let Some(v) = owner {
        parts.push(v);
    }
    if let Some(v) = workspace {
        parts.push(v);
    }
    if let Some(v) = project {
        parts.push(v);
    }
    if let Some(v) = visibility {
        parts.push(v);
    }
    if parts.is_empty() {
        String::new()
    } else {
        format!(" |{}", parts.join(""))
    }
}

fn format_skill_detail(skill: &Value) -> String {
    let name = skill.get("name").and_then(|v| v.as_str()).unwrap_or("?");
    let title = skill.get("title").and_then(|v| v.as_str()).unwrap_or(name);
    let scope = skill.get("scope").and_then(|v| v.as_str()).unwrap_or("?");
    let status = skill.get("status").and_then(|v| v.as_str()).unwrap_or("?");
    let version = skill.get("version").and_then(|v| v.as_i64()).unwrap_or(0);
    let priority = skill.get("priority").and_then(|v| v.as_i64()).unwrap_or(50);
    let instruction = skill
        .get("instruction_body")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let triggers = skill
        .get("trigger_patterns")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        })
        .unwrap_or_default();
    let categories = skill
        .get("categories")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        })
        .unwrap_or_default();
    let has_actions = skill
        .get("actions")
        .and_then(|v| v.as_array())
        .map(|a| !a.is_empty())
        .unwrap_or(false);
    let has_manifest = skill
        .get("capability_manifest")
        .and_then(|v| v.as_object())
        .map(|obj| !obj.is_empty())
        .unwrap_or(false);

    let mut text = format!("**{}** ({})\n", title, name);
    text.push_str(&format!(
        "Scope: {} | Status: {} | Priority: {} | Version: {}\n",
        scope, status, priority, version
    ));
    let governance = format_skill_governance(skill);
    if !governance.is_empty() {
        text.push_str(&format!(
            "Governance:{}\n",
            governance.trim_start_matches(" |")
        ));
    }
    if !triggers.is_empty() {
        text.push_str(&format!("Triggers: {}\n", triggers));
    }
    if !categories.is_empty() {
        text.push_str(&format!("Categories: {}\n", categories));
    }
    if has_actions {
        let count = skill
            .get("actions")
            .and_then(|v| v.as_array())
            .map(|a| a.len())
            .unwrap_or(0);
        text.push_str(&format!("Actions: {} step(s)\n", count));
    }
    if has_manifest {
        text.push_str("Capability manifest: present\n");
    }
    text.push_str(&format!("\n{}\n", instruction));
    text
}

/// Format a skill run result as human-readable text.
fn format_run_result(result: &Value) -> String {
    let status = result
        .get("status")
        .and_then(|v| v.as_str())
        .unwrap_or("unknown");
    let exec_id = result
        .get("execution_id")
        .and_then(|v| v.as_str())
        .unwrap_or("dry-run");

    let mut text = format!("Execution: {} (id: {})\n", status, exec_id);

    if let Some(negotiation) = result.get("negotiation") {
        if let Some(mode) = negotiation.get("resolved_mode").and_then(|v| v.as_str()) {
            text.push_str(&format!("Negotiation: {}\n", mode));
        }
        if let Some(summary) = negotiation.get("summary").and_then(|v| v.as_str()) {
            text.push_str(&format!("Summary: {}\n", summary));
        }
        if let Some(missing) = negotiation
            .get("missing_required")
            .and_then(|v| v.as_array())
        {
            if !missing.is_empty() {
                let items = missing
                    .iter()
                    .filter_map(|v| v.as_str())
                    .collect::<Vec<_>>()
                    .join(", ");
                text.push_str(&format!("Missing required: {}\n", items));
            }
        }
    }

    if let Some(steps) = result.get("steps").and_then(|v| v.as_array()) {
        for step in steps {
            let idx = step.get("step_index").and_then(|v| v.as_i64()).unwrap_or(0);
            let action_type = step
                .get("action_type")
                .and_then(|v| v.as_str())
                .unwrap_or("?");
            let step_status = step.get("status").and_then(|v| v.as_str()).unwrap_or("?");

            text.push_str(&format!(
                "  Step {}: {} [{}]",
                idx, action_type, step_status
            ));

            // Show instruction/prompt_inject text content
            if action_type == "instruction" || action_type == "prompt_inject" {
                if let Some(content) = step
                    .get("output")
                    .and_then(|o| o.get("text"))
                    .and_then(|v| v.as_str())
                {
                    text.push_str(&format!("\n    {}", content));
                }
            }
            if let Some(reason) = step.get("reason").and_then(|v| v.as_str()) {
                text.push_str(&format!("\n    {}", reason));
            }
            text.push('\n');
        }
    }

    text
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_valid_actions() {
        for action in VALID_ACTIONS {
            assert!(!action.is_empty());
        }
        assert!(VALID_ACTIONS.contains(&"list"));
        assert!(VALID_ACTIONS.contains(&"get"));
        assert!(VALID_ACTIONS.contains(&"create"));
        assert!(VALID_ACTIONS.contains(&"update"));
        assert!(VALID_ACTIONS.contains(&"run"));
        assert!(VALID_ACTIONS.contains(&"delete"));
        assert!(VALID_ACTIONS.contains(&"import"));
        assert!(VALID_ACTIONS.contains(&"export"));
        assert!(VALID_ACTIONS.contains(&"share"));
    }

    #[test]
    fn test_parse_uuid_valid() {
        let id = "550e8400-e29b-41d4-a716-446655440000".to_string();
        let result = SkillTool::parse_uuid(&Some(id), "test");
        assert!(result.is_ok());
        assert!(result.unwrap().is_some());
    }

    #[test]
    fn test_parse_uuid_invalid() {
        let result = SkillTool::parse_uuid(&Some("not-a-uuid".to_string()), "test");
        assert!(result.is_err());
    }

    #[test]
    fn test_parse_uuid_none() {
        let result = SkillTool::parse_uuid(&None, "test");
        assert!(result.is_ok());
        assert!(result.unwrap().is_none());
    }

    #[test]
    fn test_parse_uuid_empty() {
        let result = SkillTool::parse_uuid(&Some("".to_string()), "test");
        assert!(result.is_ok());
        assert!(result.unwrap().is_none());
    }

    #[test]
    fn test_input_schema_has_action() {
        let config = mcp_types::Config::default();
        let client = mcp_client::ContextStreamClient::new(config.clone());
        let session = Arc::new(mcp_session::SessionManager::new(client.clone(), config));
        let tool = SkillTool::new(client, session);
        let schema = tool.input_schema();

        // Verify action property exists and is required
        let props = schema.get("properties").unwrap();
        assert!(props.get("action").is_some());
        assert!(props.get("target_project").is_some());
        let required = schema.get("required").unwrap().as_array().unwrap();
        assert!(required.iter().any(|v| v.as_str() == Some("action")));
    }

    #[test]
    fn test_metadata() {
        let config = mcp_types::Config::default();
        let client = mcp_client::ContextStreamClient::new(config.clone());
        let session = Arc::new(mcp_session::SessionManager::new(client.clone(), config));
        let tool = SkillTool::new(client, session);
        let meta = tool.metadata();
        assert_eq!(meta.name, "skill");
        assert_eq!(meta.title, "Skill");
        assert!(!meta.description.is_empty());
    }

    #[test]
    fn test_compute_skill_match_score_priority() {
        assert_eq!(
            compute_skill_match_score("aws-ssh", "aws-ssh", "AWS SSH Access"),
            Some(0)
        );
        assert_eq!(
            compute_skill_match_score("aws ssh access", "aws-ssh", "AWS SSH Access"),
            Some(1)
        );
        assert_eq!(
            compute_skill_match_score("ssh", "aws-ssh", "AWS SSH Access"),
            Some(2)
        );
        assert_eq!(
            compute_skill_match_score("access", "aws-ssh", "AWS SSH Access"),
            Some(3)
        );
        assert_eq!(
            compute_skill_match_score("missing", "aws-ssh", "AWS SSH Access"),
            None
        );
    }

    #[test]
    fn test_rank_skill_candidates_deterministic_order() {
        let items = serde_json::json!({
            "items": [
                {
                    "id": "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa",
                    "name": "aws-ssh",
                    "title": "AWS SSH Access",
                    "scope": "team",
                    "status": "active"
                },
                {
                    "id": "bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb",
                    "name": "aws-ssh-east",
                    "title": "AWS SSH East",
                    "scope": "team",
                    "status": "active"
                },
                {
                    "id": "cccccccc-cccc-4ccc-8ccc-cccccccccccc",
                    "name": "linux-shell",
                    "title": "Shell Access",
                    "scope": "personal",
                    "status": "draft"
                }
            ]
        });

        let ranked = rank_skill_candidates("aws-ssh", &items);
        assert_eq!(ranked.len(), 2);
        assert_eq!(ranked[0].name, "aws-ssh");
        assert_eq!(ranked[0].score, 0);
        assert_eq!(ranked[1].name, "aws-ssh-east");
        assert_eq!(ranked[1].score, 2);
    }

    #[test]
    fn test_merge_skill_items_adds_account_level_skills() {
        let input: SkillInput = serde_json::from_value(serde_json::json!({
            "action": "list",
            "project_id": "11111111-1111-4111-8111-111111111111"
        }))
        .unwrap();
        let mut project_result = serde_json::json!({
            "items": [
                {
                    "id": "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa",
                    "name": "code-review",
                    "title": "Code Review Checklist",
                    "scope": "personal",
                    "status": "active"
                }
            ]
        });
        let account_result = serde_json::json!({
            "items": [
                {
                    "id": "bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb",
                    "name": "prod-rds-access",
                    "title": "Production RDS Database Access",
                    "scope": "personal",
                    "status": "active"
                }
            ]
        });

        merge_skill_items(&mut project_result, &account_result, &input);
        let items = project_result
            .get("items")
            .and_then(|items| items.as_array())
            .unwrap();
        assert_eq!(items.len(), 2);
        assert!(items.iter().any(
            |item| item.get("name").and_then(|value| value.as_str()) == Some("prod-rds-access")
        ));
    }

    #[test]
    fn test_merge_skill_items_dedupes_by_id() {
        let input: SkillInput = serde_json::from_value(serde_json::json!({
            "action": "list"
        }))
        .unwrap();
        let mut result = serde_json::json!({
            "items": [
                {
                    "id": "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa",
                    "name": "prod-rds-access",
                    "title": "Production RDS Database Access",
                    "scope": "personal",
                    "status": "active"
                }
            ]
        });
        let extra = serde_json::json!([
            {
                "id": "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa",
                "name": "prod-rds-access",
                "title": "Production RDS Database Access",
                "scope": "personal",
                "status": "active"
            }
        ]);

        merge_skill_items(&mut result, &extra, &input);
        let items = result
            .get("items")
            .and_then(|items| items.as_array())
            .unwrap();
        assert_eq!(items.len(), 1);
    }

    #[test]
    fn test_merge_skill_items_honors_scope_filter_for_matched_skills() {
        let input: SkillInput = serde_json::from_value(serde_json::json!({
            "action": "list",
            "scope": "team"
        }))
        .unwrap();
        let mut result = serde_json::json!({ "items": [] });
        let extra = serde_json::json!([
            {
                "id": "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa",
                "name": "prod-rds-access",
                "title": "Production RDS Database Access",
                "scope": "personal",
                "status": "active"
            },
            {
                "id": "bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb",
                "name": "deploy-checker",
                "title": "Deploy Safety Checker",
                "scope": "team",
                "status": "active"
            }
        ]);

        merge_skill_items(&mut result, &extra, &input);
        let items = result
            .get("items")
            .and_then(|items| items.as_array())
            .unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(
            items[0].get("name").and_then(|value| value.as_str()),
            Some("deploy-checker")
        );
    }

    #[test]
    fn test_skill_candidates_from_match_array_accepts_skill_id() {
        let matched = serde_json::json!([
            {
                "skill_id": "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa",
                "name": "prod-rds-access",
                "title": "Production RDS Database Access",
                "scope": "personal",
                "status": "active"
            }
        ]);

        let candidates = skill_candidates_from_items(&matched);
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].name, "prod-rds-access");
    }

    #[test]
    fn test_format_skill_governance_includes_metadata() {
        let skill = serde_json::json!({
            "owner_user_id": "11111111-1111-4111-8111-111111111111",
            "workspace_id": "22222222-2222-4222-8222-222222222222",
            "project_id": "33333333-3333-4333-8333-333333333333",
            "visibility": "workspace"
        });
        let text = format_skill_governance(&skill);
        assert!(text.contains("owner="));
        assert!(text.contains("workspace="));
        assert!(text.contains("project="));
        assert!(text.contains("visibility="));
    }

    #[test]
    fn test_format_skills_list_result_appends_governance() {
        let result = serde_json::json!({
            "items": [{
                "id": "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa",
                "name": "deploy",
                "title": "Deploy Skill",
                "scope": "team",
                "status": "active",
                "workspace_id": "22222222-2222-4222-8222-222222222222"
            }]
        });
        let rendered = format_skills_list_result(&result)
            .content
            .iter()
            .filter_map(|item| match item {
                mcp_types::tool::ContentItem::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(rendered.contains("workspace="));
    }

    #[tokio::test]
    async fn test_execute_invalid_action() {
        let config = mcp_types::Config::default();
        let client = mcp_client::ContextStreamClient::new(config.clone());
        let session = Arc::new(mcp_session::SessionManager::new(client.clone(), config));
        let tool = SkillTool::new(client, session);
        let input = serde_json::json!({"action": "invalid_action"});
        let result = tool.execute(input).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_execute_create_missing_name() {
        let config = mcp_types::Config::default();
        let client = mcp_client::ContextStreamClient::new(config.clone());
        let session = Arc::new(mcp_session::SessionManager::new(client.clone(), config));
        let tool = SkillTool::new(client, session);
        let input = serde_json::json!({
            "action": "create",
            "instruction_body": "some body"
        });
        let result = tool.execute(input).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_execute_get_missing_id_and_name() {
        let config = mcp_types::Config::default();
        let client = mcp_client::ContextStreamClient::new(config.clone());
        let session = Arc::new(mcp_session::SessionManager::new(client.clone(), config));
        let tool = SkillTool::new(client, session);
        let input = serde_json::json!({"action": "get"});
        let result = tool.execute(input).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_execute_list_rejects_unknown_target_project() {
        let config = mcp_types::Config::default();
        let client = mcp_client::ContextStreamClient::new(config.clone());
        let session = Arc::new(mcp_session::SessionManager::new(client.clone(), config));
        let mut projects = std::collections::HashMap::new();
        projects.insert(
            "contextstream".to_string(),
            mcp_session::ChildProjectInfo {
                project_id: "11111111-1111-4111-8111-111111111111".to_string(),
                name: "ContextStream".to_string(),
                path: "/tmp/contextstream".to_string(),
            },
        );
        session.set_child_projects(projects).await;
        let tool = SkillTool::new(client, session);
        let input = serde_json::json!({
            "action": "list",
            "target_project": "missing-child"
        });
        let result = tool.execute(input).await;
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("Unknown target_project"));
    }

    #[tokio::test]
    async fn test_execute_create_accepts_known_target_project_before_network() {
        let config = mcp_types::Config::default();
        let client = mcp_client::ContextStreamClient::new(config.clone());
        let session = Arc::new(mcp_session::SessionManager::new(client.clone(), config));
        let mut projects = std::collections::HashMap::new();
        projects.insert(
            "contextstream".to_string(),
            mcp_session::ChildProjectInfo {
                project_id: "11111111-1111-4111-8111-111111111111".to_string(),
                name: "ContextStream".to_string(),
                path: "/tmp/contextstream".to_string(),
            },
        );
        session.set_child_projects(projects).await;
        let tool = SkillTool::new(client, session);
        let input = serde_json::json!({
            "action": "create",
            "name": "cross-project-skill",
            "instruction_body": "Do the thing",
            "target_project": "contextstream"
        });
        let result = tool.execute(input).await;
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(!err.contains("target_project"));
    }

    #[tokio::test]
    async fn test_execute_create_accepts_current_session_project_target_without_child_projects() {
        let config = mcp_types::Config::default();
        let client = mcp_client::ContextStreamClient::new(config.clone());
        let session = Arc::new(mcp_session::SessionManager::new(client.clone(), config));
        let workspace_id = Uuid::new_v4();
        let project_id = Uuid::new_v4();
        session
            .initialize(
                Some(workspace_id),
                Some(project_id),
                Some("/tmp/contextstream".to_string()),
                None,
            )
            .await;
        let tool = SkillTool::new(client, session);
        let input = serde_json::json!({
            "action": "create",
            "name": "cross-project-skill",
            "instruction_body": "Do the thing",
            "target_project": "contextstream"
        });
        let result = tool.execute(input).await;
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(!err.contains("requires init from a multi-project parent folder first"));
        assert!(!err.contains("Unknown target_project"));
    }
}
