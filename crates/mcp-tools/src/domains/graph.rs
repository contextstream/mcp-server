//! Graph domain tools: dependencies, impact, related, call_path, ingest, outbox, path,
//! circular_dependencies, unused_code, complexity_metrics, quality_trends, quality_history,
//! quality_freshness, quality_snapshot, contradictions.

use async_trait::async_trait;
use mcp_client::{
    CallPathParams, ContextStreamClient, GraphDependenciesParams, GraphImpactParams,
    GraphRelatedParams, GraphTarget,
};
use mcp_session::SessionManager;
use mcp_types::{
    config::Config,
    tool::{ToolAnnotations, ToolCategory, ToolMetadata, ToolResult},
    Error, Result,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::sync::Arc;
use uuid::Uuid;

use crate::domains::scope::{resolve_read_scope, ResolvedReadScope};
use crate::registry::ToolHandler;
use crate::schema::SchemaBuilder;

// ============================================================================
// Graph Related Tool
// ============================================================================

/// Input for graph related.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GraphRelatedInput {
    pub node_id: String,
    pub workspace_id: Option<String>,
    pub project_id: Option<String>,
    pub max_depth: Option<i64>,
    pub relation_types: Option<Vec<String>>,
}

/// Graph related tool handler.
pub struct GraphRelatedTool {
    client: ContextStreamClient,
    session: Arc<SessionManager>,
    /// Legacy no-op layer retained for read-through compatibility via
    /// `AtlasWarmCacheKind::RelatedNodes`
    /// (2-min TTL). Same agentic-burst access pattern as
    /// graph_dependencies, lower volume but the cache is essentially
    /// free at this size.
    atlas_layer: mcp_types::atlas_layer::AtlasLayer,
    acceleration_layer: mcp_types::acceleration_layer::AccelerationLayer,
}

impl GraphRelatedTool {
    pub fn new(client: ContextStreamClient) -> Self {
        Self::with_session(
            client.clone(),
            Arc::new(SessionManager::new(client, Config::default())),
        )
    }

    pub fn with_session(client: ContextStreamClient, session: Arc<SessionManager>) -> Self {
        Self::with_session_and_atlas(client, session, mcp_types::atlas_layer::noop_layer())
    }

    pub fn with_session_and_atlas(
        client: ContextStreamClient,
        session: Arc<SessionManager>,
        atlas_layer: mcp_types::atlas_layer::AtlasLayer,
    ) -> Self {
        Self::with_session_atlas_and_acceleration(
            client,
            session,
            atlas_layer,
            mcp_types::acceleration_layer::noop_acceleration_layer(),
        )
    }

    pub fn with_session_atlas_and_acceleration(
        client: ContextStreamClient,
        session: Arc<SessionManager>,
        atlas_layer: mcp_types::atlas_layer::AtlasLayer,
        acceleration_layer: mcp_types::acceleration_layer::AccelerationLayer,
    ) -> Self {
        Self {
            client,
            session,
            atlas_layer,
            acceleration_layer,
        }
    }
}

#[async_trait]
impl ToolHandler for GraphRelatedTool {
    async fn execute(&self, input: Value) -> Result<ToolResult> {
        let input: GraphRelatedInput =
            serde_json::from_value(input).map_err(|e| Error::Validation(e.to_string()))?;

        let node_id = Uuid::parse_str(&input.node_id)
            .map_err(|_| Error::Validation("node_id must be a valid UUID".to_string()))?;

        let scope = resolve_read_scope(
            &self.client,
            self.session.as_ref(),
            input.workspace_id.as_deref(),
            input.project_id.as_deref(),
        )
        .await?;

        let params = GraphRelatedParams {
            node_id,
            workspace_id: scope.workspace_id,
            project_id: scope.project_id,
            max_depth: input.max_depth,
            relation_types: input.relation_types.clone(),
        };

        // Cache (workspace, project, node, depth, relation_types) in
        // RelatedNodes (2-min TTL). Lookup hard-capped at 50ms; miss
        // = primary unchanged.
        let cache_depth = input.max_depth;
        let cache_relations = input.relation_types.clone();
        let cached_related = if let Some(ws) = scope.workspace_id {
            let fed_scope = mcp_types::atlas_layer::AtlasFederationScope {
                workspace_id: ws,
                project_id: scope.project_id,
                scope_hash: crate::domains::atlas_warm_cache::scope_hash_for_related_query(
                    ws,
                    scope.project_id,
                    node_id,
                    cache_depth,
                    cache_relations.as_deref(),
                ),
                user_scope: None,
            };
            crate::domains::atlas_warm_cache::try_lookup_accelerated(
                &self.acceleration_layer,
                &self.atlas_layer,
                mcp_types::atlas_layer::AtlasWarmCacheKind::RelatedNodes,
                fed_scope,
                60, // primary baseline ms — graph_related is fast on a warm graph
            )
            .await
        } else {
            None
        };

        let (mut result, warm_cache_hit, warm_cache_age_ms) = if let Some(bundle) = cached_related {
            let age_ms = bundle.age_ms;
            (bundle.payload, true, age_ms)
        } else {
            let primary = self.client.graph_related(params).await?;
            if let Some(ws) = scope.workspace_id {
                let fed_scope = mcp_types::atlas_layer::AtlasFederationScope {
                    workspace_id: ws,
                    project_id: scope.project_id,
                    scope_hash: crate::domains::atlas_warm_cache::scope_hash_for_related_query(
                        ws,
                        scope.project_id,
                        node_id,
                        cache_depth,
                        cache_relations.as_deref(),
                    ),
                    user_scope: None,
                };
                crate::domains::atlas_warm_cache::put_accelerated_in_background(
                    self.acceleration_layer.clone(),
                    self.atlas_layer.clone(),
                    mcp_types::atlas_layer::AtlasWarmCacheKind::RelatedNodes,
                    fed_scope,
                    primary.clone(),
                );
            }
            (primary, false, None)
        };

        if warm_cache_hit {
            stamp_warm_cache_metadata(&mut result, "graph_related", warm_cache_age_ms);
        }

        // Format results
        let nodes = result
            .get("nodes")
            .and_then(|v| v.as_array())
            .map(|a| a.len())
            .unwrap_or(0);
        let edges = result
            .get("edges")
            .and_then(|v| v.as_array())
            .map(|a| a.len())
            .unwrap_or(0);

        let mut text = format!(
            "Found {} related nodes and {} connections for node {}.\n\n",
            nodes, edges, input.node_id
        );
        if warm_cache_hit {
            text = warm_cache_prefixed_text("graph_related", warm_cache_age_ms, text);
        }

        // List nodes
        if let Some(nodes_arr) = result.get("nodes").and_then(|v| v.as_array()) {
            for (i, node) in nodes_arr.iter().take(10).enumerate() {
                let name = node
                    .get("name")
                    .and_then(|v| v.as_str())
                    .unwrap_or("Unknown");
                let kind = node.get("kind").and_then(|v| v.as_str()).unwrap_or("");
                let path = node.get("path").and_then(|v| v.as_str()).unwrap_or("");

                text.push_str(&format!("{}. {} [{}]\n   {}\n", i + 1, name, kind, path));
            }

            if nodes_arr.len() > 10 {
                text.push_str(&format!("\n... and {} more nodes\n", nodes_arr.len() - 10));
            }
        }

        let mut output = ToolResult::with_structured(text, result);
        if let Some(note) = scope.note.as_deref() {
            output = output.with_prefix(format!("{}\n", note));
        }
        Ok(output)
    }

    fn metadata(&self) -> &ToolMetadata {
        static METADATA: std::sync::OnceLock<ToolMetadata> = std::sync::OnceLock::new();
        METADATA.get_or_init(|| ToolMetadata {
            name: "graph_related".to_string(),
            title: "Find Related Code".to_string(),
            description: "Find code related to a given file, function, or concept.".to_string(),
            category: ToolCategory::Graph,
            annotations: ToolAnnotations::read_only(),
            is_pro: false,
            required_tier: Some("lite".to_string()),
        })
    }

    fn input_schema(&self) -> Value {
        SchemaBuilder::new()
            .description("Find related nodes in the knowledge graph")
            .uuid(
                "node_id",
                "UUID of the node to find related nodes for",
                true,
            )
            .uuid("workspace_id", "Workspace ID", false)
            .uuid("project_id", "Project ID", false)
            .integer("max_depth", "Maximum traversal depth (default: 2)", false)
            .string(
                "relation_types",
                "Filter by relation types (comma-separated)",
                false,
            )
            .build()
    }
}

// ============================================================================
// Graph Dependencies Tool
// ============================================================================

/// Input for graph dependencies.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GraphDependenciesInput {
    /// Target type: module, function, type, or variable
    pub target_type: String,
    /// Target identifier (file path for modules, function/type/variable name otherwise)
    pub target_id: String,
    pub workspace_id: Option<String>,
    pub project_id: Option<String>,
    pub max_depth: Option<i64>,
    pub include_transitive: Option<bool>,
}

/// Graph dependencies tool handler.
pub struct GraphDependenciesTool {
    client: ContextStreamClient,
    session: Arc<SessionManager>,
    /// Legacy no-op layer retained for read-through compatibility via
    /// `AtlasWarmCacheKind::DependencyResult`
    /// (2-min TTL). Highest-volume Neo4j-touching graph endpoint, with
    /// the agentic-burst access pattern: agents revisit the same
    /// (target_type, target_id, depth, transitive) tuple multiple
    /// times in a single tool-use loop.
    atlas_layer: mcp_types::atlas_layer::AtlasLayer,
    acceleration_layer: mcp_types::acceleration_layer::AccelerationLayer,
}

impl GraphDependenciesTool {
    pub fn new(client: ContextStreamClient) -> Self {
        Self::with_session(
            client.clone(),
            Arc::new(SessionManager::new(client, Config::default())),
        )
    }

    pub fn with_session(client: ContextStreamClient, session: Arc<SessionManager>) -> Self {
        Self::with_session_and_atlas(client, session, mcp_types::atlas_layer::noop_layer())
    }

    pub fn with_session_and_atlas(
        client: ContextStreamClient,
        session: Arc<SessionManager>,
        atlas_layer: mcp_types::atlas_layer::AtlasLayer,
    ) -> Self {
        Self::with_session_atlas_and_acceleration(
            client,
            session,
            atlas_layer,
            mcp_types::acceleration_layer::noop_acceleration_layer(),
        )
    }

    pub fn with_session_atlas_and_acceleration(
        client: ContextStreamClient,
        session: Arc<SessionManager>,
        atlas_layer: mcp_types::atlas_layer::AtlasLayer,
        acceleration_layer: mcp_types::acceleration_layer::AccelerationLayer,
    ) -> Self {
        Self {
            client,
            session,
            atlas_layer,
            acceleration_layer,
        }
    }
}

#[async_trait]
impl ToolHandler for GraphDependenciesTool {
    async fn execute(&self, input: Value) -> Result<ToolResult> {
        let input: GraphDependenciesInput =
            serde_json::from_value(input).map_err(|e| Error::Validation(e.to_string()))?;

        if input.target_id.trim().is_empty() {
            return Err(Error::Validation("target_id is required".to_string()));
        }

        let scope = resolve_read_scope(
            &self.client,
            self.session.as_ref(),
            input.workspace_id.as_deref(),
            input.project_id.as_deref(),
        )
        .await?;

        // Build target based on type
        let target = GraphTarget {
            target_type: input.target_type.clone(),
            id: if input.target_type.eq_ignore_ascii_case("module")
                || input.target_type.eq_ignore_ascii_case("file")
                || input.target_type.eq_ignore_ascii_case("path")
            {
                None
            } else {
                Some(input.target_id.clone())
            },
            path: if input.target_type.eq_ignore_ascii_case("module")
                || input.target_type.eq_ignore_ascii_case("file")
                || input.target_type.eq_ignore_ascii_case("path")
            {
                Some(input.target_id.clone())
            } else {
                None
            },
        };

        let params = GraphDependenciesParams {
            target,
            workspace_id: scope.workspace_id,
            project_id: scope.project_id,
            max_depth: input.max_depth,
            include_transitive: input.include_transitive,
        };

        // Highest-volume Neo4j endpoint (~17.5K calls/day vs the rest
        // combined). Cache the (workspace, project, target_type,
        // target, depth, transitive) tuple in the regional warm cache
        // (DependencyResult kind, 2-min TTL — covers the agentic-burst
        // window without serving wildly stale data once the agent
        // moves to a different target). Lookup hard-capped at 50ms;
        // miss = primary unchanged.
        let cache_target_type = input.target_type.clone();
        let cache_target_id = input.target_id.clone();
        let cache_depth = input.max_depth;
        let cache_transitive = input.include_transitive;
        let cached_deps = if let Some(ws) = scope.workspace_id {
            let fed_scope = mcp_types::atlas_layer::AtlasFederationScope {
                workspace_id: ws,
                project_id: scope.project_id,
                scope_hash: crate::domains::atlas_warm_cache::scope_hash_for_dependency_query(
                    ws,
                    scope.project_id,
                    &cache_target_type,
                    &cache_target_id,
                    cache_depth,
                    cache_transitive,
                ),
                user_scope: None,
            };
            crate::domains::atlas_warm_cache::try_lookup_accelerated(
                &self.acceleration_layer,
                &self.atlas_layer,
                mcp_types::atlas_layer::AtlasWarmCacheKind::DependencyResult,
                fed_scope,
                80, // primary baseline ms — graph_dependencies is hot but well-indexed
            )
            .await
        } else {
            None
        };

        let (mut result, warm_cache_hit, warm_cache_age_ms) = if let Some(bundle) = cached_deps {
            let age_ms = bundle.age_ms;
            (bundle.payload, true, age_ms)
        } else {
            let primary = self.client.graph_dependencies(params).await?;
            // Best-effort write-back so the next pod (or this pod's
            // next agent loop iteration) hits.
            if let Some(ws) = scope.workspace_id {
                let fed_scope = mcp_types::atlas_layer::AtlasFederationScope {
                    workspace_id: ws,
                    project_id: scope.project_id,
                    scope_hash: crate::domains::atlas_warm_cache::scope_hash_for_dependency_query(
                        ws,
                        scope.project_id,
                        &cache_target_type,
                        &cache_target_id,
                        cache_depth,
                        cache_transitive,
                    ),
                    user_scope: None,
                };
                crate::domains::atlas_warm_cache::put_accelerated_in_background(
                    self.acceleration_layer.clone(),
                    self.atlas_layer.clone(),
                    mcp_types::atlas_layer::AtlasWarmCacheKind::DependencyResult,
                    fed_scope,
                    primary.clone(),
                );
            }
            (primary, false, None)
        };

        let deps = result
            .get("dependencies")
            .and_then(|v| v.as_array())
            .map(|a| a.len())
            .unwrap_or(0);
        let reverse = result
            .get("reverse_dependencies")
            .and_then(|v| v.as_array())
            .map(|a| a.len())
            .unwrap_or(0);
        let agent_workflow = build_graph_agent_workflow(
            &self.client,
            &scope,
            "dependencies",
            deps + reverse,
            Some(&input.target_id),
        )
        .await;
        let recommendations = dependency_recommendations(deps, reverse);
        if let Some(obj) = result.as_object_mut() {
            obj.insert("agent_workflow".to_string(), agent_workflow.clone());
        }
        attach_recommendations(&mut result, &recommendations);
        if warm_cache_hit {
            stamp_warm_cache_metadata(&mut result, "graph_dependencies", warm_cache_age_ms);
        }

        let mut text = format!(
            "Found {} dependencies and {} reverse dependencies for {} \"{}\".\n\n",
            deps, reverse, input.target_type, input.target_id
        );
        if warm_cache_hit {
            text = warm_cache_prefixed_text("graph_dependencies", warm_cache_age_ms, text);
        }
        text.push_str(&graph_agent_workflow_summary(&agent_workflow));
        text.push_str("\n\n");
        push_recommendations(&mut text, &recommendations);
        text.push('\n');

        if let Some(deps_arr) = result.get("dependencies").and_then(|v| v.as_array()) {
            for (i, dep) in deps_arr.iter().take(15).enumerate() {
                let node = dep.get("node");
                let name = node
                    .and_then(|n| n.get("name"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("Unknown");
                let kind = node
                    .and_then(|n| n.get("type"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                let path = node
                    .and_then(|n| n.get("path").or_else(|| n.get("file_path")))
                    .and_then(|v| v.as_str())
                    .unwrap_or("");

                text.push_str(&format!("{}. {} [{}]\n   {}\n", i + 1, name, kind, path));
            }

            if deps_arr.len() > 15 {
                text.push_str(&format!(
                    "\n... and {} more dependencies\n",
                    deps_arr.len() - 15
                ));
            }
        }

        let mut output = ToolResult::with_structured(text, result);
        if let Some(note) = scope.note.as_deref() {
            output = output.with_prefix(format!("{}\n", note));
        }
        Ok(output)
    }

    fn metadata(&self) -> &ToolMetadata {
        static METADATA: std::sync::OnceLock<ToolMetadata> = std::sync::OnceLock::new();
        METADATA.get_or_init(|| ToolMetadata {
            name: "graph_dependencies".to_string(),
            title: "Analyze Dependencies".to_string(),
            description: "Analyze dependencies of a file or function.".to_string(),
            category: ToolCategory::Graph,
            annotations: ToolAnnotations::read_only(),
            is_pro: false,
            required_tier: Some("lite".to_string()),
        })
    }

    fn input_schema(&self) -> Value {
        SchemaBuilder::new()
            .description("Analyze dependencies")
            .string(
                "target_type",
                "Target type: module, function, type, or variable",
                true,
            )
            .string(
                "target_id",
                "Target identifier (file path for modules, name otherwise)",
                true,
            )
            .integer("max_depth", "Maximum traversal depth", false)
            .boolean(
                "include_transitive",
                "Include transitive dependencies",
                false,
            )
            .build()
    }
}

// ============================================================================
// Graph Impact Tool
// ============================================================================

/// Input for graph impact.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GraphImpactInput {
    /// Target ID (e.g., function or file identifier)
    pub target_id: String,
    /// Target type: module, function, type, or variable
    pub target_type: Option<String>,
    /// Element name for the target
    pub element_name: Option<String>,
    pub workspace_id: Option<String>,
    pub project_id: Option<String>,
    /// Change type: modify_signature, delete, rename, etc.
    pub change_type: Option<String>,
}

/// Graph impact tool handler.
pub struct GraphImpactTool {
    client: ContextStreamClient,
    session: Arc<SessionManager>,
    /// Legacy no-op layer retained while graph warm-cache reads move to the
    /// public acceleration layer.
    atlas_layer: mcp_types::atlas_layer::AtlasLayer,
    acceleration_layer: mcp_types::acceleration_layer::AccelerationLayer,
}

impl GraphImpactTool {
    pub fn new(client: ContextStreamClient) -> Self {
        Self::with_session(
            client.clone(),
            Arc::new(SessionManager::new(client, Config::default())),
        )
    }

    pub fn with_session(client: ContextStreamClient, session: Arc<SessionManager>) -> Self {
        Self::with_session_and_atlas(client, session, mcp_types::atlas_layer::noop_layer())
    }

    pub fn with_session_and_atlas(
        client: ContextStreamClient,
        session: Arc<SessionManager>,
        atlas_layer: mcp_types::atlas_layer::AtlasLayer,
    ) -> Self {
        Self::with_session_atlas_and_acceleration(
            client,
            session,
            atlas_layer,
            mcp_types::acceleration_layer::noop_acceleration_layer(),
        )
    }

    pub fn with_session_atlas_and_acceleration(
        client: ContextStreamClient,
        session: Arc<SessionManager>,
        atlas_layer: mcp_types::atlas_layer::AtlasLayer,
        acceleration_layer: mcp_types::acceleration_layer::AccelerationLayer,
    ) -> Self {
        Self {
            client,
            session,
            atlas_layer,
            acceleration_layer,
        }
    }
}

#[async_trait]
impl ToolHandler for GraphImpactTool {
    async fn execute(&self, input: Value) -> Result<ToolResult> {
        let input: GraphImpactInput =
            serde_json::from_value(input).map_err(|e| Error::Validation(e.to_string()))?;

        if input.target_id.trim().is_empty() {
            return Err(Error::Validation("target_id is required".to_string()));
        }

        let scope = resolve_read_scope(
            &self.client,
            self.session.as_ref(),
            input.workspace_id.as_deref(),
            input.project_id.as_deref(),
        )
        .await?;

        let element_name = input
            .element_name
            .clone()
            .unwrap_or_else(|| input.target_id.clone());
        let target_type = normalize_graph_target_type(input.target_type.clone(), &input.target_id);

        let params = GraphImpactParams {
            target_id: input.target_id.clone(),
            element_name,
            target_type,
            workspace_id: scope.workspace_id,
            project_id: scope.project_id,
            change_type: input.change_type.clone(),
        };

        // A8b: try the regional warm cache for this exact (workspace,
        // project, target_id) tuple. graph_impact is variable-length
        // Neo4j; cache hit serves <30ms vs primary's variable spike.
        // Lookup hard-capped at 50ms; cache miss = primary unchanged.
        let cache_target = input.target_id.clone();
        let cached_impact = if let Some(ws) = scope.workspace_id {
            let fed_scope = mcp_types::atlas_layer::AtlasFederationScope {
                workspace_id: ws,
                project_id: scope.project_id,
                scope_hash: crate::domains::atlas_warm_cache::scope_hash_for_graph_query(
                    ws,
                    scope.project_id,
                    "impact",
                    &cache_target,
                ),
                user_scope: None,
            };
            crate::domains::atlas_warm_cache::try_lookup_accelerated(
                &self.acceleration_layer,
                &self.atlas_layer,
                mcp_types::atlas_layer::AtlasWarmCacheKind::SubgraphSnapshot,
                fed_scope,
                100, // primary baseline ms — graph_impact 60ms p50 with spikes
            )
            .await
        } else {
            None
        };

        let (result, cache_hit, cache_age_ms) = if let Some(bundle) = cached_impact {
            let age = bundle.age_ms;
            (bundle.payload, true, age)
        } else {
            let primary = self.client.graph_impact(params).await?;
            // Best-effort write-back so the next pod hits.
            if let Some(ws) = scope.workspace_id {
                let fed_scope = mcp_types::atlas_layer::AtlasFederationScope {
                    workspace_id: ws,
                    project_id: scope.project_id,
                    scope_hash: crate::domains::atlas_warm_cache::scope_hash_for_graph_query(
                        ws,
                        scope.project_id,
                        "impact",
                        &cache_target,
                    ),
                    user_scope: None,
                };
                crate::domains::atlas_warm_cache::put_accelerated_in_background(
                    self.acceleration_layer.clone(),
                    self.atlas_layer.clone(),
                    mcp_types::atlas_layer::AtlasWarmCacheKind::SubgraphSnapshot,
                    fed_scope,
                    primary.clone(),
                );
            }
            (primary, false, None)
        };
        let _ = (cache_hit, cache_age_ms); // markers stamped into envelope below

        let directly_affected = result
            .get("directly_affected")
            .and_then(|v| v.as_u64())
            .unwrap_or(0);
        let affected_functions = result
            .get("affected_functions")
            .and_then(|v| v.as_array())
            .map(|a| a.len())
            .unwrap_or(0);
        let affected_modules = result
            .get("affected_modules")
            .and_then(|v| v.as_array())
            .map(|a| a.len())
            .unwrap_or(0);

        let risk_level = result
            .get("risk_level")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown");

        let mut text = format!(
            "Impact analysis for \"{}\":\n\
             Risk level: {}\n\
             directly affected: {}\n\
             affected modules: {}\n\
             affected functions: {}\n\n",
            input.target_id, risk_level, directly_affected, affected_modules, affected_functions
        );

        if let Some(modules) = result.get("affected_modules").and_then(|v| v.as_array()) {
            if !modules.is_empty() {
                text.push_str("Affected modules:\n");
                for (i, item) in modules.iter().take(10).enumerate() {
                    let path = item.as_str().unwrap_or("Unknown");
                    text.push_str(&format!("{}. {}\n", i + 1, path));
                }
            }
        }
        if let Some(functions) = result.get("affected_functions").and_then(|v| v.as_array()) {
            if !functions.is_empty() {
                text.push_str("\nAffected functions:\n");
                for (i, item) in functions.iter().take(10).enumerate() {
                    let name = item.as_str().unwrap_or("Unknown");
                    text.push_str(&format!("{}. {}\n", i + 1, name));
                }
            }
        }

        // A8b: stamp warm-cache provenance on the structured envelope
        // without changing the response shape clients consume.
        let mut result = result;
        if let Some(obj) = result.as_object_mut() {
            obj.insert(
                "served_from".to_string(),
                serde_json::Value::String(if cache_hit {
                    "regional_warm_cache".to_string()
                } else {
                    "primary_server".to_string()
                }),
            );
            obj.insert("cache_hit".to_string(), serde_json::Value::Bool(cache_hit));
            if let Some(age) = cache_age_ms {
                obj.insert(
                    "cache_age_ms".to_string(),
                    serde_json::Value::Number(age.into()),
                );
            }
        }
        let text = if cache_hit {
            format!(
                "[WARM_CACHE] graph_impact served from regional cache (age {}ms)\n{}",
                cache_age_ms.unwrap_or(0),
                text
            )
        } else {
            text
        };

        let mut output = ToolResult::with_structured(text, result);
        if let Some(note) = scope.note.as_deref() {
            output = output.with_prefix(format!("{}\n", note));
        }
        Ok(output)
    }

    fn metadata(&self) -> &ToolMetadata {
        static METADATA: std::sync::OnceLock<ToolMetadata> = std::sync::OnceLock::new();
        METADATA.get_or_init(|| ToolMetadata {
            name: "graph_impact".to_string(),
            title: "Impact Analysis".to_string(),
            description: "Analyze the impact of changing a file or function.".to_string(),
            category: ToolCategory::Graph,
            annotations: ToolAnnotations::read_only(),
            is_pro: false,
            required_tier: Some("lite".to_string()),
        })
    }

    fn input_schema(&self) -> Value {
        SchemaBuilder::new()
            .description("Analyze change impact")
            .string(
                "target_id",
                "Target identifier (file path or function name)",
                true,
            )
            .string(
                "target_type",
                "Target type: module, function, type, or variable",
                false,
            )
            .string(
                "element_name",
                "Element name (defaults to target_id)",
                false,
            )
            .string(
                "change_type",
                "Change type: modify_signature, delete, rename",
                false,
            )
            .build()
    }
}

// ============================================================================
// Unified Graph Tool
// ============================================================================

/// Input for the unified graph tool.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GraphInput {
    pub action: String,
    // Common fields
    pub workspace_id: Option<String>,
    pub project_id: Option<String>,
    pub max_depth: Option<i64>,
    pub include_transitive: Option<bool>,
    pub limit: Option<i64>,
    pub offset: Option<i64>,
    // Ingest fields
    pub wait: Option<bool>,
    pub requested_tier: Option<String>,
    pub idempotency_scope: Option<String>,
    // Dashboard quality fields
    pub element_type: Option<String>,
    pub start_date: Option<String>,
    pub end_date: Option<String>,
    // Dependencies/Impact target fields
    pub target_type: Option<String>,
    pub target_id: Option<String>,
    // Impact specific
    pub element_name: Option<String>,
    pub change_type: Option<String>,
    // Related fields
    pub node_id: Option<String>,
    pub relation_types: Option<Vec<String>>,
    // Call path fields (source/target)
    pub source_type: Option<String>,
    pub source_id: Option<String>,
    // Path fields (node IDs)
    pub source_node_id: Option<String>,
    pub target_node_id: Option<String>,
    // Code-drift: recompute findings on demand before returning.
    pub refresh: Option<bool>,
}

/// Unified graph tool handler.
pub struct GraphTool {
    client: ContextStreamClient,
    session: Arc<SessionManager>,
    /// Legacy no-op layer retained while graph warm-cache reads move to the
    /// public acceleration layer.
    atlas_layer: mcp_types::atlas_layer::AtlasLayer,
    acceleration_layer: mcp_types::acceleration_layer::AccelerationLayer,
}

impl GraphTool {
    pub fn new(client: ContextStreamClient) -> Self {
        Self::with_session(
            client.clone(),
            Arc::new(SessionManager::new(client, Config::default())),
        )
    }

    pub fn with_session(client: ContextStreamClient, session: Arc<SessionManager>) -> Self {
        Self::with_session_and_atlas(client, session, mcp_types::atlas_layer::noop_layer())
    }

    pub fn with_session_and_atlas(
        client: ContextStreamClient,
        session: Arc<SessionManager>,
        atlas_layer: mcp_types::atlas_layer::AtlasLayer,
    ) -> Self {
        Self::with_session_atlas_and_acceleration(
            client,
            session,
            atlas_layer,
            mcp_types::acceleration_layer::noop_acceleration_layer(),
        )
    }

    pub fn with_session_atlas_and_acceleration(
        client: ContextStreamClient,
        session: Arc<SessionManager>,
        atlas_layer: mcp_types::atlas_layer::AtlasLayer,
        acceleration_layer: mcp_types::acceleration_layer::AccelerationLayer,
    ) -> Self {
        Self {
            client,
            session,
            atlas_layer,
            acceleration_layer,
        }
    }
}

fn call_path_node_count(result: &Value) -> usize {
    if let Some(path) = result.get("path").and_then(|v| v.as_array()) {
        return path.len();
    }

    if let Some(first_path) = result
        .get("paths")
        .and_then(|v| v.as_array())
        .and_then(|paths| paths.first())
    {
        if let Some(functions) = first_path.get("functions").and_then(|v| v.as_array()) {
            return functions.len();
        }

        if let Some(length) = first_path.get("length").and_then(|v| v.as_u64()) {
            return length.saturating_add(1) as usize;
        }
    }

    if let Some(shortest_path_length) = result.get("shortest_path_length").and_then(|v| v.as_u64())
    {
        return shortest_path_length.saturating_add(1) as usize;
    }

    0
}

fn normalize_call_path_result(mut result: Value) -> Value {
    let first_functions = result
        .get("paths")
        .and_then(|v| v.as_array())
        .and_then(|paths| paths.first())
        .and_then(|path| path.get("functions"))
        .cloned();

    if let Some(obj) = result.as_object_mut() {
        if !obj.contains_key("path") {
            if let Some(functions) = first_functions {
                obj.insert("path".to_string(), functions);
            }
        }
    }

    result
}

fn looks_like_type_identifier(target_id: &str) -> bool {
    let mut chars = target_id.chars();
    matches!(chars.next(), Some(ch) if ch.is_ascii_uppercase())
}

fn normalize_graph_target_type(target_type: Option<String>, target_id: &str) -> Option<String> {
    let normalized = target_type
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(|value| value.to_ascii_lowercase());

    match normalized.as_deref() {
        Some(
            "service" | "interface" | "enum" | "type_alias" | "typealias" | "model" | "provider"
            | "class" | "struct" | "trait",
        ) => Some("type".to_string()),
        Some("method" | "hook") => Some("function".to_string()),
        Some("file" | "path") => Some("module".to_string()),
        Some("data" | "const" | "constant") => Some("variable".to_string()),
        Some(value) => Some(value.to_string()),
        None if looks_like_type_identifier(target_id) => Some("type".to_string()),
        None => None,
    }
}

fn normalize_usages_target_type(target_type: Option<String>, target_id: &str) -> String {
    normalize_graph_target_type(target_type, target_id).unwrap_or_else(|| "component".to_string())
}

fn array_len_for_any_key(result: &Value, keys: &[&str]) -> usize {
    keys.iter()
        .find_map(|key| {
            result
                .get(*key)
                .and_then(|value| value.as_array())
                .map(|items| items.len())
        })
        .unwrap_or(0)
}

fn count_for_any_key(result: &Value, keys: &[&str], fallback: usize) -> usize {
    keys.iter()
        .find_map(|key| {
            result
                .get(*key)
                .and_then(|value| value.as_u64())
                .map(|count| count as usize)
        })
        .unwrap_or(fallback)
}

fn stamp_warm_cache_metadata(result: &mut Value, tool: &str, age_ms: Option<u64>) {
    if let Some(obj) = result.as_object_mut() {
        obj.insert(
            "served_from".to_string(),
            Value::String("regional_warm_cache".to_string()),
        );
        obj.insert("cache_hit".to_string(), Value::Bool(true));
        obj.insert("cache_tool".to_string(), Value::String(tool.to_string()));
        obj.insert(
            "cache_marker".to_string(),
            Value::String("[WARM_CACHE]".to_string()),
        );
        if let Some(age) = age_ms {
            obj.insert("cache_age_ms".to_string(), json!(age));
        }
    }
}

fn warm_cache_prefixed_text(tool: &str, age_ms: Option<u64>, text: String) -> String {
    format!(
        "[WARM_CACHE] {} served from regional cache (age {}ms)\n{}",
        tool,
        age_ms.unwrap_or(0),
        text
    )
}

async fn build_graph_agent_workflow(
    client: &ContextStreamClient,
    scope: &ResolvedReadScope,
    analysis_kind: &str,
    finding_count: usize,
    focus: Option<&str>,
) -> Value {
    let team_plan = client.is_team_plan().await.unwrap_or(false);
    let workspace_visibility = match scope.workspace_id {
        Some(workspace_id) => client
            .get_workspace(workspace_id)
            .await
            .ok()
            .and_then(|workspace| workspace.visibility),
        None => None,
    };
    let shared_project =
        team_plan && matches!(workspace_visibility.as_deref(), Some("team" | "org"));
    let tracking_visibility = if shared_project { "team" } else { "personal" };
    let related_project_ids: Vec<String> = scope
        .related_project_ids
        .iter()
        .map(|project_id| project_id.to_string())
        .collect();

    let prompt_suggestions = match analysis_kind {
        "unused_code" => vec![
            "Verify false positives before deleting unused symbols",
            "Create a staged cleanup plan with tests before removals",
            "Open follow-up tickets for public API or generated-code exceptions",
        ],
        "circular_dependencies" => vec![
            "Find the safest dependency-cycle breakpoints",
            "Draft a small-batch migration plan for reviewers",
            "Create tickets for shared boundaries that need owner review",
        ],
        "dependencies" => vec![
            "Summarize direct imports and reverse-dependency blast radius",
            "Identify owner-facing review notes before refactoring",
            "Create a plan that separates safe local edits from boundary changes",
        ],
        _ => vec![
            "Summarize the graph findings",
            "Create a tracked plan before changing code",
            "Ask the user before launching an editing agent",
        ],
    };

    serde_json::json!({
        "analysis_kind": analysis_kind,
        "finding_count": finding_count,
        "focus": focus,
        "account_scope": {
            "team_plan": team_plan,
            "workspace_visibility": workspace_visibility,
            "project_shared_for_team": shared_project,
            "tracking_visibility": tracking_visibility,
            "related_project_ids": related_project_ids,
        },
        "agent_guidance": {
            "contextcode_opt_in": true,
            "summary": if shared_project {
                "Team/shared scope detected. Prefer team-visible tickets, todos, and plans for validated findings so other members can track ownership. Ask before using ContextCode to edit."
            } else {
                "Personal scope detected. Prefer personal tickets, todos, and plans unless the user chooses to share the project. Ask before using ContextCode to edit."
            },
            "recommended_tracking": ["plan", "ticket", "todo"],
            "prompt_suggestions": prompt_suggestions,
        }
    })
}

fn graph_agent_workflow_summary(workflow: &Value) -> String {
    let tracking_visibility = workflow
        .pointer("/account_scope/tracking_visibility")
        .and_then(|value| value.as_str())
        .unwrap_or("personal");
    let shared_project = workflow
        .pointer("/account_scope/project_shared_for_team")
        .and_then(|value| value.as_bool())
        .unwrap_or(false);
    let prompt = workflow
        .pointer("/agent_guidance/prompt_suggestions/0")
        .and_then(|value| value.as_str())
        .unwrap_or("Create a tracked plan before changing code");

    format!(
        "Agent workflow: ContextCode is opt-in. Suggested tracking visibility: {}{}. First prompt: {}.",
        tracking_visibility,
        if shared_project { " (shared team project)" } else { "" },
        prompt
    )
}

fn value_i64_at(result: &Value, pointer: &str) -> Option<i64> {
    result.pointer(pointer).and_then(|value| {
        value
            .as_i64()
            .or_else(|| value.as_u64().map(|count| count as i64))
    })
}

fn value_f64_at(result: &Value, pointer: &str) -> Option<f64> {
    result.pointer(pointer).and_then(|value| value.as_f64())
}

fn value_bool_at(result: &Value, pointer: &str) -> Option<bool> {
    result.pointer(pointer).and_then(|value| value.as_bool())
}

fn latest_data_point(result: &Value) -> Option<&Value> {
    result
        .get("data_points")
        .and_then(|value| value.as_array())
        .and_then(|points| points.last())
}

fn push_recommendations(text: &mut String, recommendations: &[String]) {
    if recommendations.is_empty() {
        return;
    }

    text.push_str("\nRecommendations:\n");
    for recommendation in recommendations.iter().take(5) {
        text.push_str("- ");
        text.push_str(recommendation);
        text.push('\n');
    }
}

fn attach_recommendations(result: &mut Value, recommendations: &[String]) {
    if let Some(obj) = result.as_object_mut() {
        obj.insert(
            "recommendations".to_string(),
            serde_json::json!(recommendations),
        );
    }
}

fn dependency_recommendations(deps: usize, reverse: usize) -> Vec<String> {
    let mut recommendations = Vec::new();
    if reverse > 0 {
        recommendations.push(
            "Review reverse dependencies before refactoring; they define the change blast radius."
                .to_string(),
        );
    }
    if deps + reverse > 20 {
        recommendations.push(
            "Split changes into a small plan before editing because this target has broad graph reach."
                .to_string(),
        );
    }
    if reverse > 5 {
        recommendations.push(
            "Run graph(action=\"impact\") or graph(action=\"usages\") on high-risk symbols before changing public APIs."
                .to_string(),
        );
    }
    if recommendations.is_empty() {
        recommendations.push(
            "Dependency surface is small; local edits are likely low-risk after normal tests pass."
                .to_string(),
        );
    }
    recommendations
}

fn circular_dependency_recommendations(total_cycles: usize) -> Vec<String> {
    if total_cycles == 0 {
        return vec![
            "No dependency cycles surfaced; check graph(action=\"quality_trends\") to see whether this stayed clean over time."
                .to_string(),
        ];
    }

    vec![
        "Start with the shortest or most repeated cycle and extract a stable boundary module."
            .to_string(),
        "Create a tracked plan before editing cycles that cross package or owner boundaries."
            .to_string(),
        "After a fix, rerun graph(action=\"circular_dependencies\") and graph(action=\"quality_snapshot\") to capture the improvement."
            .to_string(),
    ]
}

fn unused_code_recommendations(total_unused: usize) -> Vec<String> {
    if total_unused == 0 {
        return vec![
            "No unused code surfaced; use graph(action=\"quality_history\") to confirm this is stable across saved scans."
                .to_string(),
        ];
    }

    vec![
        "Verify public API, generated-code, and dynamic-entrypoint false positives before deleting anything."
            .to_string(),
        "Remove unused code in small batches with focused tests for each affected module."
            .to_string(),
        "After cleanup, rerun graph(action=\"unused_code\") and graph(action=\"quality_snapshot\") to update dashboard history."
            .to_string(),
    ]
}

fn complexity_recommendations(result: &Value) -> Vec<String> {
    let complex = value_i64_at(result, "/summary/complex_function_count")
        .or_else(|| value_i64_at(result, "/total_complex"))
        .unwrap_or_else(|| array_len_for_any_key(result, &["high_complexity_functions"]) as i64);
    let long = value_i64_at(result, "/summary/long_function_count")
        .or_else(|| value_i64_at(result, "/total_long"))
        .unwrap_or_else(|| array_len_for_any_key(result, &["long_functions"]) as i64);

    let mut recommendations = Vec::new();
    if complex > 0 {
        recommendations.push(
            "Prioritize the highest-complexity functions; extract decision branches behind named helpers and add characterization tests first."
                .to_string(),
        );
    }
    if long > 0 {
        recommendations.push(
            "For long functions, split orchestration from pure calculations or IO adapters before changing behavior."
                .to_string(),
        );
    }
    if value_bool_at(result, "/needs_full_tier_reindex").unwrap_or(false) {
        recommendations.push(
            "Re-run graph(action=\"ingest\", wait=true) to refresh function-level graph data before acting on complexity results."
                .to_string(),
        );
    }
    if recommendations.is_empty() {
        recommendations.push(
            "Complexity looks healthy; keep this as a baseline with graph(action=\"quality_snapshot\")."
                .to_string(),
        );
    }
    recommendations
}

fn trend_recommendations(result: &Value) -> Vec<String> {
    let Some(latest) = latest_data_point(result) else {
        return vec![
            "No trend points are available yet; run graph(action=\"quality_snapshot\") after quality scans complete."
                .to_string(),
        ];
    };

    let circular = value_i64_at(latest, "/circular_deps").unwrap_or(0);
    let unused = value_i64_at(latest, "/unused_code").unwrap_or(0);
    let complex = value_i64_at(latest, "/complex_functions").unwrap_or(0);
    let long = value_i64_at(latest, "/long_functions").unwrap_or(0);
    let historical = value_bool_at(result, "/has_historical_data").unwrap_or(false);
    let mut recommendations = Vec::new();

    if !historical {
        recommendations.push(
            "This is a live point, not historical data; run graph(action=\"quality_snapshot\") to start persisted trend history."
                .to_string(),
        );
    }
    if circular > 0 {
        recommendations.push(
            "Circular dependency count is non-zero; run graph(action=\"circular_dependencies\") and plan the safest cycle breakpoints."
                .to_string(),
        );
    }
    if unused > 0 {
        recommendations.push(
            "Unused-code count is non-zero; run graph(action=\"unused_code\") and verify false positives before deleting."
                .to_string(),
        );
    }
    if complex > 0 || long > 0 {
        recommendations.push(
            "Complex or long functions are present; run graph(action=\"complexity_metrics\") and refactor highest-risk functions first."
                .to_string(),
        );
    }
    if recommendations.is_empty() {
        recommendations.push(
            "Latest trend point is clean; preserve it with periodic graph(action=\"quality_snapshot\") runs."
                .to_string(),
        );
    }
    recommendations
}

fn quality_history_recommendations(result: &Value) -> Vec<String> {
    let Some(runs) = result.get("runs").and_then(|value| value.as_array()) else {
        return vec!["No saved quality runs were returned; record a snapshot after running Code Health scans.".to_string()];
    };
    let Some(latest) = runs.first() else {
        return vec![
            "Run graph(action=\"circular_dependencies\"), graph(action=\"unused_code\"), graph(action=\"complexity_metrics\"), then graph(action=\"quality_snapshot\") to create history."
                .to_string(),
        ];
    };

    let open = value_i64_at(latest, "/open_count").unwrap_or(0);
    let resolved = value_i64_at(latest, "/resolved_count").unwrap_or(0);
    let regressed = value_i64_at(latest, "/regressed_count").unwrap_or(0);
    let mut recommendations = Vec::new();
    if regressed > 0 {
        recommendations.push(
            "Regressions are present; inspect the latest run and create follow-up tickets before new refactors."
                .to_string(),
        );
    }
    if open > 0 {
        recommendations.push(
            "Open findings remain; group them by risk and create a small remediation plan before editing."
                .to_string(),
        );
    }
    if resolved > 0 && regressed == 0 {
        recommendations.push(
            "Recent run resolved findings without regressions; record the remediation pattern as a decision or lesson if it should be reused."
                .to_string(),
        );
    }
    if recommendations.is_empty() {
        recommendations.push(
            "No open lifecycle movement surfaced; run fresh quality actions when preparing a refactor."
                .to_string(),
        );
    }
    recommendations
}

fn freshness_recommendations(result: &Value) -> Vec<String> {
    let analyses = result
        .get("analyses")
        .and_then(|value| value.as_array())
        .cloned()
        .unwrap_or_default();
    let mut recommendations = Vec::new();

    for analysis in analyses.iter() {
        let name = analysis
            .get("analysis")
            .and_then(|value| value.as_str())
            .unwrap_or("unknown");
        let has_cached_payload = analysis
            .get("has_cached_payload")
            .and_then(|value| value.as_bool())
            .unwrap_or(false);
        let job_state = analysis
            .get("job_state")
            .and_then(|value| value.as_str())
            .unwrap_or("idle");

        if job_state == "in_progress" {
            recommendations.push(format!(
                "{} is still running; retry freshness or the matching graph action before making final recommendations.",
                name
            ));
        } else if !has_cached_payload {
            let action = match name {
                "circular_dependencies" => "circular_dependencies",
                "unused_code" => "unused_code",
                "complexity_metrics" => "complexity_metrics",
                _ => "quality_snapshot",
            };
            recommendations.push(format!(
                "{} has no cached payload; run graph(action=\"{}\") to populate dashboard-quality data.",
                name, action
            ));
        }
    }

    if recommendations.is_empty() {
        recommendations.push(
            "Quality caches look populated; use graph(action=\"quality_trends\") and graph(action=\"quality_history\") for recommendations."
                .to_string(),
        );
    }
    recommendations
}

#[async_trait]
impl ToolHandler for GraphTool {
    async fn execute(&self, input: Value) -> Result<ToolResult> {
        let input: GraphInput =
            serde_json::from_value(input).map_err(|e| Error::Validation(e.to_string()))?;

        match input.action.to_lowercase().as_str() {
            "related" => {
                let node_id = input.node_id.ok_or_else(|| Error::Validation("node_id is required for related action".to_string()))?;
                let related_input = GraphRelatedInput {
                    node_id,
                    workspace_id: input.workspace_id.clone(),
                    project_id: input.project_id.clone(),
                    max_depth: input.max_depth,
                    relation_types: input.relation_types,
                };
                let tool = GraphRelatedTool::with_session_atlas_and_acceleration(
                    self.client.clone(),
                    self.session.clone(),
                    self.atlas_layer.clone(),
                    self.acceleration_layer.clone(),
                );
                tool.execute(serde_json::to_value(&related_input).unwrap()).await
            }
            "dependencies" | "deps" => {
                let target_type = input.target_type.unwrap_or_else(|| "function".to_string());
                let target_id = input.target_id.ok_or_else(|| Error::Validation("target_id is required for dependencies action".to_string()))?;
                let deps_input = GraphDependenciesInput {
                    target_type,
                    target_id,
                    workspace_id: input.workspace_id.clone(),
                    project_id: input.project_id.clone(),
                    max_depth: input.max_depth,
                    include_transitive: input.include_transitive,
                };
                let tool = GraphDependenciesTool::with_session_atlas_and_acceleration(
                    self.client.clone(),
                    self.session.clone(),
                    self.atlas_layer.clone(),
                    self.acceleration_layer.clone(),
                );
                tool.execute(serde_json::to_value(&deps_input).unwrap()).await
            }
            "impact" => {
                let target_id = input.target_id.ok_or_else(|| Error::Validation("target_id is required for impact action".to_string()))?;
                let impact_input = GraphImpactInput {
                    target_id: target_id.clone(),
                    target_type: input.target_type,
                    element_name: input.element_name.or(Some(target_id)),
                    workspace_id: input.workspace_id.clone(),
                    project_id: input.project_id.clone(),
                    change_type: input.change_type,
                };
                // A8b: forward our atlas_layer so the delegated
                // GraphImpactTool sees the same regional warm cache.
                let tool = GraphImpactTool::with_session_atlas_and_acceleration(
                    self.client.clone(),
                    self.session.clone(),
                    self.atlas_layer.clone(),
                    self.acceleration_layer.clone(),
                );
                tool.execute(serde_json::to_value(&impact_input).unwrap()).await
            }
            "ingest" => {
                let scope = resolve_read_scope(
                    &self.client,
                    self.session.as_ref(),
                    input.workspace_id.as_deref(),
                    input.project_id.as_deref(),
                )
                .await?;
                let result = self
                    .client
                    .graph_ingest(scope.workspace_id, scope.project_id, input.wait)
                    .await?;
                let status = result.get("status").and_then(|v| v.as_str()).unwrap_or("started");
                let text = format!("Graph ingest {}.", status);
                let mut output = ToolResult::with_structured(text, result);
                if let Some(note) = scope.note.as_deref() {
                    output = output.with_prefix(format!("{}\n", note));
                }
                Ok(output)
            }
            "outbox_status" => {
                let scope = resolve_read_scope(
                    &self.client,
                    self.session.as_ref(),
                    input.workspace_id.as_deref(),
                    input.project_id.as_deref(),
                )
                .await?;
                let result = self
                    .client
                    .graph_outbox_status(scope.workspace_id, scope.project_id)
                    .await?;
                let pending = result
                    .get("pending_count")
                    .and_then(|v| v.as_i64())
                    .unwrap_or(0);
                let open = result
                    .get("open_count")
                    .and_then(|v| v.as_i64())
                    .unwrap_or(0);
                let deadletter = result
                    .get("deadletter_count")
                    .and_then(|v| v.as_i64())
                    .unwrap_or(0);
                let text = format!(
                    "Graph outbox status: {} open, {} pending, {} deadletter.",
                    open, pending, deadletter
                );
                let mut output = ToolResult::with_structured(text, result);
                if let Some(note) = scope.note.as_deref() {
                    output = output.with_prefix(format!("{}\n", note));
                }
                Ok(output)
            }
            "outbox_canary" => {
                let scope = resolve_read_scope(
                    &self.client,
                    self.session.as_ref(),
                    input.workspace_id.as_deref(),
                    input.project_id.as_deref(),
                )
                .await?;
                let result = self
                    .client
                    .graph_outbox_canary(
                        scope.workspace_id,
                        scope.project_id,
                        input.requested_tier.clone(),
                        input.idempotency_scope.clone(),
                    )
                    .await?;
                let lsn = result
                    .get("mutation_lsn")
                    .and_then(|v| v.as_i64())
                    .map(|value| value.to_string())
                    .unwrap_or_else(|| "unknown".to_string());
                let status = result
                    .get("status")
                    .and_then(|v| v.as_str())
                    .unwrap_or("queued");
                let text = format!(
                    "Graph outbox canary {} with mutation LSN {}.",
                    status, lsn
                );
                let mut output = ToolResult::with_structured(text, result);
                if let Some(note) = scope.note.as_deref() {
                    output = output.with_prefix(format!("{}\n", note));
                }
                Ok(output)
            }
            "call_path" => {
                let source_type = input.source_type.ok_or_else(|| Error::Validation("source_type is required (e.g., 'function')".to_string()))?;
                let source_id = input.source_id.ok_or_else(|| Error::Validation("source_id is required (function identifier)".to_string()))?;
                let scope = resolve_read_scope(
                    &self.client,
                    self.session.as_ref(),
                    input.workspace_id.as_deref(),
                    input.project_id.as_deref(),
                )
                .await?;
                let params = CallPathParams {
                    source_type,
                    source_id: source_id.clone(),
                    target_type: input.target_type,
                    target_id: input.target_id.clone(),
                    workspace_id: scope.workspace_id,
                    project_id: scope.project_id,
                    max_depth: input.max_depth,
                };

                // A8b: warm-cache lookup keyed by source→target pair
                let cache_target = format!(
                    "{}->{}",
                    source_id,
                    input.target_id.as_deref().unwrap_or("_any")
                );
                let cached = if let Some(ws) = scope.workspace_id {
                    let fed_scope = mcp_types::atlas_layer::AtlasFederationScope {
                        workspace_id: ws,
                        project_id: scope.project_id,
                        scope_hash: crate::domains::atlas_warm_cache::scope_hash_for_graph_query(
                            ws,
                            scope.project_id,
                            "call_path",
                            &cache_target,
                        ),
                user_scope: None,
                    };
                    crate::domains::atlas_warm_cache::try_lookup_accelerated(
                        &self.acceleration_layer,
                        &self.atlas_layer,
                        mcp_types::atlas_layer::AtlasWarmCacheKind::SubgraphSnapshot,
                        fed_scope,
                        100,
                    )
                    .await
                } else {
                    None
                };
                let (result, cache_hit, cache_age_ms) = if let Some(bundle) = cached {
                    let age = bundle.age_ms;
                    (bundle.payload, true, age)
                } else {
                    let primary = self.client.graph_call_path(params).await?;
                    let primary = normalize_call_path_result(primary);
                    if let Some(ws) = scope.workspace_id {
                        let fed_scope = mcp_types::atlas_layer::AtlasFederationScope {
                            workspace_id: ws,
                            project_id: scope.project_id,
                            scope_hash:
                                crate::domains::atlas_warm_cache::scope_hash_for_graph_query(
                                    ws,
                                    scope.project_id,
                                    "call_path",
                                    &cache_target,
                                ),
                user_scope: None,
                        };
                        crate::domains::atlas_warm_cache::put_accelerated_in_background(
                            self.acceleration_layer.clone(),
                            self.atlas_layer.clone(),
                            mcp_types::atlas_layer::AtlasWarmCacheKind::SubgraphSnapshot,
                            fed_scope,
                            primary.clone(),
                        );
                    }
                    (primary, false, None)
                };

                let path_len = call_path_node_count(&result);
                let mut result = result;
                if let Some(obj) = result.as_object_mut() {
                    obj.insert(
                        "served_from".to_string(),
                        serde_json::Value::String(if cache_hit {
                            "regional_warm_cache".to_string()
                        } else {
                            "primary_server".to_string()
                        }),
                    );
                    obj.insert(
                        "cache_hit".to_string(),
                        serde_json::Value::Bool(cache_hit),
                    );
                    if let Some(age) = cache_age_ms {
                        obj.insert(
                            "cache_age_ms".to_string(),
                            serde_json::Value::Number(age.into()),
                        );
                    }
                }
                let text = if cache_hit {
                    format!(
                        "[WARM_CACHE] graph_call_path served from regional cache (age {}ms)\nFound call path with {} nodes.",
                        cache_age_ms.unwrap_or(0),
                        path_len
                    )
                } else {
                    format!("Found call path with {} nodes.", path_len)
                };
                let mut output = ToolResult::with_structured(text, result);
                if let Some(note) = scope.note.as_deref() {
                    output = output.with_prefix(format!("{}\n", note));
                }
                Ok(output)
            }
            "path" => {
                let source_id = input.source_node_id.ok_or_else(|| Error::Validation("source_node_id is required".to_string()))?;
                let target_id = input.target_node_id.ok_or_else(|| Error::Validation("target_node_id is required".to_string()))?;
                let source_uuid = Uuid::parse_str(&source_id).map_err(|_| Error::Validation("Invalid source_node_id UUID".to_string()))?;
                let target_uuid = Uuid::parse_str(&target_id).map_err(|_| Error::Validation("Invalid target_node_id UUID".to_string()))?;
                let scope = resolve_read_scope(
                    &self.client,
                    self.session.as_ref(),
                    input.workspace_id.as_deref(),
                    input.project_id.as_deref(),
                )
                .await?;
                let result = self
                    .client
                    .graph_path(
                        source_uuid,
                        target_uuid,
                        scope.workspace_id,
                        scope.project_id,
                        input.max_depth,
                    )
                    .await?;
                let path_len = result.get("path").and_then(|v| v.as_array()).map(|a| a.len()).unwrap_or(0);
                let text = if path_len > 0 {
                    format!("Found path with {} nodes.", path_len)
                } else {
                    "No path found between the nodes.".to_string()
                };
                let mut output = ToolResult::with_structured(text, result);
                if let Some(note) = scope.note.as_deref() {
                    output = output.with_prefix(format!("{}\n", note));
                }
                Ok(output)
            }
            "circular_dependencies" => {
                let scope = resolve_read_scope(
                    &self.client,
                    self.session.as_ref(),
                    input.workspace_id.as_deref(),
                    input.project_id.as_deref(),
                )
                .await?;

                // A8b: workspace-wide query — cache key uses the
                // `_workspace` sentinel target.
                let cached = if let Some(ws) = scope.workspace_id {
                    let fed_scope = mcp_types::atlas_layer::AtlasFederationScope {
                        workspace_id: ws,
                        project_id: scope.project_id,
                        scope_hash: crate::domains::atlas_warm_cache::scope_hash_for_graph_query(
                            ws,
                            scope.project_id,
                            "circular_dependencies",
                            "_workspace",
                        ),
                user_scope: None,
                    };
                    crate::domains::atlas_warm_cache::try_lookup_accelerated(
                        &self.acceleration_layer,
                        &self.atlas_layer,
                        mcp_types::atlas_layer::AtlasWarmCacheKind::SubgraphSnapshot,
                        fed_scope,
                        100,
                    )
                    .await
                } else {
                    None
                };
                let (result, cache_hit, cache_age_ms) = if let Some(bundle) = cached {
                    let age = bundle.age_ms;
                    (bundle.payload, true, age)
                } else {
                    let primary = self
                        .client
                        .graph_circular_dependencies(
                            scope.workspace_id,
                            scope.project_id,
                            input.limit,
                            input.offset,
                        )
                        .await?;
                    if let Some(ws) = scope.workspace_id {
                        let fed_scope = mcp_types::atlas_layer::AtlasFederationScope {
                            workspace_id: ws,
                            project_id: scope.project_id,
                            scope_hash:
                                crate::domains::atlas_warm_cache::scope_hash_for_graph_query(
                                    ws,
                                    scope.project_id,
                                    "circular_dependencies",
                                    "_workspace",
                                ),
                user_scope: None,
                        };
                        crate::domains::atlas_warm_cache::put_accelerated_in_background(
                            self.acceleration_layer.clone(),
                            self.atlas_layer.clone(),
                            mcp_types::atlas_layer::AtlasWarmCacheKind::SubgraphSnapshot,
                            fed_scope,
                            primary.clone(),
                        );
                    }
                    (primary, false, None)
                };

                let cycles = array_len_for_any_key(&result, &["cycles"]);
                let total_cycles = count_for_any_key(&result, &["total_count", "count"], cycles);
                let agent_workflow = build_graph_agent_workflow(
                    &self.client,
                    &scope,
                    "circular_dependencies",
                    total_cycles,
                    None,
                )
                .await;
                let mut result = result;
                if let Some(obj) = result.as_object_mut() {
                    obj.insert(
                        "served_from".to_string(),
                        serde_json::Value::String(if cache_hit {
                            "regional_warm_cache".to_string()
                        } else {
                            "primary_server".to_string()
                        }),
                    );
                    obj.insert(
                        "cache_hit".to_string(),
                        serde_json::Value::Bool(cache_hit),
                    );
                    if let Some(age) = cache_age_ms {
                        obj.insert(
                            "cache_age_ms".to_string(),
                            serde_json::Value::Number(age.into()),
                        );
                    }
                    obj.insert("agent_workflow".to_string(), agent_workflow.clone());
                }
                let recommendations = circular_dependency_recommendations(total_cycles);
                attach_recommendations(&mut result, &recommendations);
                let body = if total_cycles > 0 {
                    format!("Found {} circular dependency cycles.", total_cycles)
                } else {
                    "No circular dependencies found.".to_string()
                };
                let mut text = if cache_hit {
                    format!(
                        "[WARM_CACHE] graph_circular_dependencies served from regional cache (age {}ms)\n{}",
                        cache_age_ms.unwrap_or(0),
                        body
                    )
                } else {
                    body
                };
                text = format!("{}\n{}", text, graph_agent_workflow_summary(&agent_workflow));
                push_recommendations(&mut text, &recommendations);
                let mut output = ToolResult::with_structured(text, result);
                if let Some(note) = scope.note.as_deref() {
                    output = output.with_prefix(format!("{}\n", note));
                }
                Ok(output)
            }
            "unused_code" => {
                let scope = resolve_read_scope(
                    &self.client,
                    self.session.as_ref(),
                    input.workspace_id.as_deref(),
                    input.project_id.as_deref(),
                )
                .await?;

                // A8b: workspace-wide query — cache key uses the
                // `_workspace` sentinel target.
                let cached = if let Some(ws) = scope.workspace_id {
                    let fed_scope = mcp_types::atlas_layer::AtlasFederationScope {
                        workspace_id: ws,
                        project_id: scope.project_id,
                        scope_hash: crate::domains::atlas_warm_cache::scope_hash_for_graph_query(
                            ws,
                            scope.project_id,
                            "unused_code",
                            "_workspace",
                        ),
                user_scope: None,
                    };
                    crate::domains::atlas_warm_cache::try_lookup_accelerated(
                        &self.acceleration_layer,
                        &self.atlas_layer,
                        mcp_types::atlas_layer::AtlasWarmCacheKind::SubgraphSnapshot,
                        fed_scope,
                        100,
                    )
                    .await
                } else {
                    None
                };
                let (result, cache_hit, cache_age_ms) = if let Some(bundle) = cached {
                    let age = bundle.age_ms;
                    (bundle.payload, true, age)
                } else {
                    let primary = self
                        .client
                        .graph_unused_code(
                            scope.workspace_id,
                            scope.project_id,
                            input.limit,
                            input.offset,
                            input.element_type.as_deref(),
                        )
                        .await?;
                    if let Some(ws) = scope.workspace_id {
                        let fed_scope = mcp_types::atlas_layer::AtlasFederationScope {
                            workspace_id: ws,
                            project_id: scope.project_id,
                            scope_hash:
                                crate::domains::atlas_warm_cache::scope_hash_for_graph_query(
                                    ws,
                                    scope.project_id,
                                    "unused_code",
                                    "_workspace",
                                ),
                user_scope: None,
                        };
                        crate::domains::atlas_warm_cache::put_accelerated_in_background(
                            self.acceleration_layer.clone(),
                            self.atlas_layer.clone(),
                            mcp_types::atlas_layer::AtlasWarmCacheKind::SubgraphSnapshot,
                            fed_scope,
                            primary.clone(),
                        );
                    }
                    (primary, false, None)
                };

                let unused = array_len_for_any_key(&result, &["unused_elements", "unused"]);
                let total_unused =
                    count_for_any_key(&result, &["total_count", "filtered_count", "count"], unused);
                let agent_workflow = build_graph_agent_workflow(
                    &self.client,
                    &scope,
                    "unused_code",
                    total_unused,
                    None,
                )
                .await;
                let mut result = result;
                if let Some(obj) = result.as_object_mut() {
                    obj.insert(
                        "served_from".to_string(),
                        serde_json::Value::String(if cache_hit {
                            "regional_warm_cache".to_string()
                        } else {
                            "primary_server".to_string()
                        }),
                    );
                    obj.insert(
                        "cache_hit".to_string(),
                        serde_json::Value::Bool(cache_hit),
                    );
                    if let Some(age) = cache_age_ms {
                        obj.insert(
                            "cache_age_ms".to_string(),
                            serde_json::Value::Number(age.into()),
                        );
                    }
                    obj.insert("agent_workflow".to_string(), agent_workflow.clone());
                }
                let recommendations = unused_code_recommendations(total_unused);
                attach_recommendations(&mut result, &recommendations);
                let body = if total_unused > 0 {
                    format!("Found {} potentially unused code elements.", total_unused)
                } else {
                    "No unused code detected.".to_string()
                };
                let mut text = if cache_hit {
                    format!(
                        "[WARM_CACHE] graph_unused_code served from regional cache (age {}ms)\n{}",
                        cache_age_ms.unwrap_or(0),
                        body
                    )
                } else {
                    body
                };
                text = format!("{}\n{}", text, graph_agent_workflow_summary(&agent_workflow));
                push_recommendations(&mut text, &recommendations);
                let mut output = ToolResult::with_structured(text, result);
                if let Some(note) = scope.note.as_deref() {
                    output = output.with_prefix(format!("{}\n", note));
                }
                Ok(output)
            }
            "complexity_metrics" | "complexity" => {
                let scope = resolve_read_scope(
                    &self.client,
                    self.session.as_ref(),
                    input.workspace_id.as_deref(),
                    input.project_id.as_deref(),
                )
                .await?;

                let mut result = self
                    .client
                    .graph_complexity_metrics(
                        scope.workspace_id,
                        scope.project_id,
                        input.limit,
                        input.offset,
                    )
                    .await?;
                let complex = value_i64_at(&result, "/summary/complex_function_count")
                    .or_else(|| value_i64_at(&result, "/total_complex"))
                    .unwrap_or_else(|| {
                        array_len_for_any_key(&result, &["high_complexity_functions"]) as i64
                    });
                let long = value_i64_at(&result, "/summary/long_function_count")
                    .or_else(|| value_i64_at(&result, "/total_long"))
                    .unwrap_or_else(|| array_len_for_any_key(&result, &["long_functions"]) as i64);
                let avg = value_f64_at(&result, "/summary/avg_function_complexity");
                let recommendations = complexity_recommendations(&result);
                attach_recommendations(&mut result, &recommendations);

                let mut text = if let Some(avg) = avg {
                    format!(
                        "Complexity metrics: {} complex functions, {} long functions, average complexity {:.2}.",
                        complex, long, avg
                    )
                } else {
                    format!(
                        "Complexity metrics: {} complex functions and {} long functions.",
                        complex, long
                    )
                };
                push_recommendations(&mut text, &recommendations);
                let mut output = ToolResult::with_structured(text, result);
                if let Some(note) = scope.note.as_deref() {
                    output = output.with_prefix(format!("{}\n", note));
                }
                Ok(output)
            }
            "quality_trends" | "code_quality_trends" | "trends" => {
                let scope = resolve_read_scope(
                    &self.client,
                    self.session.as_ref(),
                    input.workspace_id.as_deref(),
                    input.project_id.as_deref(),
                )
                .await?;

                let mut result = self
                    .client
                    .graph_quality_trends(
                        scope.workspace_id,
                        scope.project_id,
                        input.limit,
                        input.start_date.as_deref(),
                        input.end_date.as_deref(),
                    )
                    .await?;
                let points = array_len_for_any_key(&result, &["data_points"]);
                let historical = value_bool_at(&result, "/has_historical_data").unwrap_or(false);
                let recommendations = trend_recommendations(&result);
                attach_recommendations(&mut result, &recommendations);

                let mut text = format!(
                    "Code Health trends returned {} data point{} (historical: {}).",
                    points,
                    if points == 1 { "" } else { "s" },
                    historical
                );
                if let Some(latest) = latest_data_point(&result) {
                    let date = latest
                        .get("date")
                        .and_then(|value| value.as_str())
                        .unwrap_or("latest");
                    let circular = value_i64_at(latest, "/circular_deps").unwrap_or(0);
                    let unused = value_i64_at(latest, "/unused_code").unwrap_or(0);
                    let complex = value_i64_at(latest, "/complex_functions").unwrap_or(0);
                    let long = value_i64_at(latest, "/long_functions").unwrap_or(0);
                    text.push_str(&format!(
                        "\nLatest ({date}): {circular} circular cycles, {unused} unused elements, {complex} complex functions, {long} long functions."
                    ));
                }
                push_recommendations(&mut text, &recommendations);
                let mut output = ToolResult::with_structured(text, result);
                if let Some(note) = scope.note.as_deref() {
                    output = output.with_prefix(format!("{}\n", note));
                }
                Ok(output)
            }
            "quality_history" | "code_quality_history" => {
                let scope = resolve_read_scope(
                    &self.client,
                    self.session.as_ref(),
                    input.workspace_id.as_deref(),
                    input.project_id.as_deref(),
                )
                .await?;

                let mut result = self
                    .client
                    .graph_quality_history(scope.workspace_id, scope.project_id, input.limit)
                    .await?;
                let runs = array_len_for_any_key(&result, &["runs"]);
                let recommendations = quality_history_recommendations(&result);
                attach_recommendations(&mut result, &recommendations);

                let mut text = format!(
                    "Code Health history returned {} saved run{}.",
                    runs,
                    if runs == 1 { "" } else { "s" }
                );
                if let Some(latest) = result
                    .get("runs")
                    .and_then(|value| value.as_array())
                    .and_then(|runs| runs.first())
                {
                    let kind = latest
                        .get("analysis_kind")
                        .and_then(|value| value.as_str())
                        .unwrap_or("quality");
                    let open = value_i64_at(latest, "/open_count").unwrap_or(0);
                    let resolved = value_i64_at(latest, "/resolved_count").unwrap_or(0);
                    let regressed = value_i64_at(latest, "/regressed_count").unwrap_or(0);
                    text.push_str(&format!(
                        "\nLatest {kind}: {open} open, {resolved} resolved, {regressed} regressed."
                    ));
                }
                push_recommendations(&mut text, &recommendations);
                let mut output = ToolResult::with_structured(text, result);
                if let Some(note) = scope.note.as_deref() {
                    output = output.with_prefix(format!("{}\n", note));
                }
                Ok(output)
            }
            "quality_freshness" | "freshness" => {
                let scope = resolve_read_scope(
                    &self.client,
                    self.session.as_ref(),
                    input.workspace_id.as_deref(),
                    input.project_id.as_deref(),
                )
                .await?;

                let mut result = self
                    .client
                    .graph_quality_freshness(scope.workspace_id, scope.project_id)
                    .await?;
                let analyses = array_len_for_any_key(&result, &["analyses"]);
                let recommendations = freshness_recommendations(&result);
                attach_recommendations(&mut result, &recommendations);

                let mut text = format!("Code Health freshness returned {} analyses.", analyses);
                if let Some(graph_version) = result.get("graph_version").and_then(|v| v.as_str()) {
                    text.push_str(&format!("\nGraph version: {graph_version}."));
                }
                push_recommendations(&mut text, &recommendations);
                let mut output = ToolResult::with_structured(text, result);
                if let Some(note) = scope.note.as_deref() {
                    output = output.with_prefix(format!("{}\n", note));
                }
                Ok(output)
            }
            "quality_snapshot" | "record_quality_snapshot" => {
                let scope = resolve_read_scope(
                    &self.client,
                    self.session.as_ref(),
                    input.workspace_id.as_deref(),
                    input.project_id.as_deref(),
                )
                .await?;

                let mut result = self
                    .client
                    .graph_quality_snapshot(scope.workspace_id, scope.project_id)
                    .await?;
                let recommendations = vec![
                    "Re-run graph(action=\"quality_trends\") and graph(action=\"quality_history\") to verify the saved snapshot is visible."
                        .to_string(),
                    "Use the saved counts to create a small remediation plan or tickets for non-zero findings."
                        .to_string(),
                ];
                attach_recommendations(&mut result, &recommendations);
                let mut text = "Recorded Code Health quality snapshot.".to_string();
                push_recommendations(&mut text, &recommendations);
                let mut output = ToolResult::with_structured(text, result);
                if let Some(note) = scope.note.as_deref() {
                    output = output.with_prefix(format!("{}\n", note));
                }
                Ok(output)
            }
            "contradictions" => {
                let node_id = input
                    .node_id
                    .as_ref()
                    .and_then(|s| Uuid::parse_str(s).ok());
                let scope = resolve_read_scope(
                    &self.client,
                    self.session.as_ref(),
                    input.workspace_id.as_deref(),
                    input.project_id.as_deref(),
                )
                .await?;
                let result = self
                    .client
                    .graph_contradictions(node_id, scope.workspace_id, input.limit)
                    .await?;
                let contradictions = result.get("contradictions").and_then(|v| v.as_array()).map(|a| a.len()).unwrap_or(0);
                let text = if contradictions > 0 {
                    format!("Found {} potential contradictions.", contradictions)
                } else {
                    "No contradictions found.".to_string()
                };
                let mut output = ToolResult::with_structured(text, result);
                if let Some(note) = scope.note.as_deref() {
                    output = output.with_prefix(format!("{}\n", note));
                }
                Ok(output)
            }
            "code_drift" => {
                // Knowledge<->code drift: code that still references retired knowledge.
                let node_id = input
                    .node_id
                    .as_ref()
                    .and_then(|s| Uuid::parse_str(s).ok());
                let scope = resolve_read_scope(
                    &self.client,
                    self.session.as_ref(),
                    input.workspace_id.as_deref(),
                    input.project_id.as_deref(),
                )
                .await?;
                let result = self
                    .client
                    .graph_code_drift(
                        node_id,
                        scope.workspace_id,
                        scope.project_id,
                        input.refresh.unwrap_or(false),
                        input.limit,
                    )
                    .await?;
                let count = result.get("count").and_then(|v| v.as_u64()).unwrap_or(0);
                let text = if count > 0 {
                    format!(
                        "Found {} stale code reference(s) to retired knowledge.",
                        count
                    )
                } else {
                    "No knowledge-code drift found.".to_string()
                };
                let mut output = ToolResult::with_structured(text, result);
                if let Some(note) = scope.note.as_deref() {
                    output = output.with_prefix(format!("{}\n", note));
                }
                Ok(output)
            }
            "decisions" => {
                // List decisions related to a node
                let node_id = input.node_id
                    .as_ref()
                    .and_then(|s| Uuid::parse_str(s).ok())
                    .ok_or_else(|| Error::Validation("node_id is required for decisions action".to_string()))?;
                let scope = resolve_read_scope(
                    &self.client,
                    self.session.as_ref(),
                    input.workspace_id.as_deref(),
                    input.project_id.as_deref(),
                )
                .await?;
                let params = GraphRelatedParams {
                    node_id,
                    workspace_id: scope.workspace_id,
                    project_id: scope.project_id,
                    max_depth: input.max_depth,
                    relation_types: Some(vec!["decision".to_string()]),
                };
                let result = self.client.graph_related(params).await?;
                let mut output =
                    ToolResult::with_structured("Decisions retrieved.".to_string(), result);
                if let Some(note) = scope.note.as_deref() {
                    output = output.with_prefix(format!("{}\n", note));
                }
                Ok(output)
            }
            "usages" => {
                let target_id = input.target_id.ok_or_else(|| {
                    Error::Validation(
                        "target_id is required for usages (component/type/function name)".to_string(),
                    )
                })?;
                let target_type = normalize_usages_target_type(input.target_type, &target_id);
                let scope = resolve_read_scope(
                    &self.client,
                    self.session.as_ref(),
                    input.workspace_id.as_deref(),
                    input.project_id.as_deref(),
                )
                .await?;

                let result = self
                    .client
                    .graph_usages(&target_id, &target_type, scope.project_id, input.limit)
                    .await?;

                let usages_count = result
                    .get("usages")
                    .and_then(|v| v.as_array())
                    .map(|a| a.len())
                    .unwrap_or(0);

                let mut text = format!(
                    "Found {} usages of {} \"{}\".\n\n",
                    usages_count, target_type, target_id
                );

                if let Some(usages_arr) = result.get("usages").and_then(|v| v.as_array()) {
                    for (i, usage) in usages_arr.iter().take(20).enumerate() {
                        let file = usage
                            .get("file_path")
                            .and_then(|v| v.as_str())
                            .unwrap_or("unknown");
                        let usage_type = usage
                            .get("usage_type")
                            .and_then(|v| v.as_str())
                            .unwrap_or("import");
                        let line = usage
                            .get("line_number")
                            .and_then(|v| v.as_i64())
                            .map(|l| format!(":{}", l))
                            .unwrap_or_default();

                        text.push_str(&format!(
                            "{}. {} [{}]{}\n",
                            i + 1, file, usage_type, line
                        ));
                    }

                    if usages_arr.len() > 20 {
                        text.push_str(&format!(
                            "\n... and {} more usages\n",
                            usages_arr.len() - 20
                        ));
                    }
                }

                let mut output = ToolResult::with_structured(text, result);
                if let Some(note) = scope.note.as_deref() {
                    output = output.with_prefix(format!("{}\n", note));
                }
                Ok(output)
            }
            _ => Err(Error::Validation(format!(
                "Unknown action: {}. Available actions: related, dependencies, impact, ingest, outbox_status, outbox_canary, call_path, path, circular_dependencies, unused_code, complexity_metrics, quality_trends, quality_history, quality_freshness, quality_snapshot, contradictions, code_drift, decisions, usages.",
                input.action
            ))),
        }
    }

    fn metadata(&self) -> &ToolMetadata {
        static METADATA: std::sync::OnceLock<ToolMetadata> = std::sync::OnceLock::new();
        METADATA.get_or_init(|| ToolMetadata {
            name: "graph".to_string(),
            title: "Code Graph Analysis".to_string(),
            description: "Code graph structural analysis and Code Health dashboard retrieval. NOT for searching code by content/keywords (use the 'search' tool for that). Actions: dependencies (module deps), impact (change impact), call_path (function call path), related (related graph nodes), path (path between nodes), decisions (decisions linked to a graph node), ingest (build graph), outbox_status and outbox_canary (Neo4j graph outbox operations), circular_dependencies, unused_code, complexity_metrics, quality_trends, quality_history, quality_freshness, quality_snapshot, contradictions, usages (reverse deps — find all files that use/render a component, type, or function). Use the quality_* actions when the user asks for dashboard Code Health data, scan history, trends, freshness, or recommendations.".to_string(),
            category: ToolCategory::Graph,
            // The unified surface includes ingest, outbox canary, and
            // quality_snapshot actions. Dedicated query-only graph tools remain
            // read-only, but this mixed action router must use the worst-case
            // write annotation.
            annotations: ToolAnnotations::write(),
            is_pro: false,
            required_tier: Some("lite".to_string()),
        })
    }

    fn input_schema(&self) -> Value {
        let all_actions = &[
            "related",
            "dependencies",
            "impact",
            "ingest",
            "outbox_status",
            "outbox_canary",
            "call_path",
            "path",
            "circular_dependencies",
            "unused_code",
            "complexity_metrics",
            "quality_trends",
            "quality_history",
            "quality_freshness",
            "quality_snapshot",
            "contradictions",
            "code_drift",
            "decisions",
            "usages",
        ];

        SchemaBuilder::new()
            .description("Code graph analysis and Code Health dashboard data with recommendation-oriented summaries")
            .string_enum("action", "Operation to perform", all_actions, true)
            // Common fields
            .uuid("workspace_id", "Workspace ID", false)
            .uuid("project_id", "Project ID", false)
            .integer("max_depth", "Maximum traversal depth", false)
            .boolean(
                "include_transitive",
                "Include transitive relationships (for dependencies)",
                false,
            )
            .integer(
                "limit",
                "Maximum results/data points (for circular_dependencies/unused_code/complexity_metrics/quality_trends/quality_history/contradictions)",
                false,
            )
            .integer(
                "offset",
                "Pagination offset (for complexity_metrics and dashboard-quality endpoints that support it)",
                false,
            )
            .string(
                "element_type",
                "Optional unused-code element type filter: Function, Type, Module, or Variable",
                false,
            )
            .string(
                "start_date",
                "Trend start date, YYYY-MM-DD (for quality_trends)",
                false,
            )
            .string(
                "end_date",
                "Trend end date, YYYY-MM-DD (for quality_trends)",
                false,
            )
            // Dependencies/Impact fields
            .string(
                "target_type",
                "Target type: module, function, type, variable (for dependencies/impact/call_path)",
                false,
            )
            .string(
                "target_id",
                "Target identifier (for dependencies/impact/call_path)",
                false,
            )
            // Impact specific
            .string(
                "element_name",
                "Element name (for impact, defaults to target_id)",
                false,
            )
            .string(
                "change_type",
                "Change type: modify_signature, delete, rename (for impact)",
                false,
            )
            // Related fields
            .uuid(
                "node_id",
                "Node UUID (for related/contradictions/decisions/code_drift)",
                false,
            )
            .boolean(
                "refresh",
                "For code_drift: recompute findings on demand before returning (default false)",
                false,
            )
            .string(
                "relation_types",
                "Relation types to filter (comma-separated, for related)",
                false,
            )
            // Ingest fields
            .boolean("wait", "Wait for ingest to complete (for ingest)", false)
            .string(
                "requested_tier",
                "Requested graph tier hint for outbox_canary",
                false,
            )
            .string(
                "idempotency_scope",
                "Optional idempotency scope for outbox_canary",
                false,
            )
            // Call path fields
            .string(
                "source_type",
                "Source type: 'function' (for call_path)",
                false,
            )
            .string(
                "source_id",
                "Source function identifier (for call_path)",
                false,
            )
            // Path fields
            .uuid("source_node_id", "Source node UUID (for path)", false)
            .uuid("target_node_id", "Target node UUID (for path)", false)
            .build()
    }
}

/// Register all graph tools.
pub fn register_graph_tools(
    registry: &mut crate::registry::ToolRegistry,
    client: ContextStreamClient,
    session: Arc<SessionManager>,
) {
    // Snapshot the atlas layer so every cache-aware tool can route
    // through the regional warm cache:
    //   - GraphTool / GraphImpactTool — variable-length Neo4j via
    //     SubgraphSnapshot (15-min TTL)
    //   - GraphDependenciesTool — highest-volume endpoint via
    //     DependencyResult (2-min TTL, agentic-burst pattern)
    //   - GraphRelatedTool — same agentic-burst shape via
    //     RelatedNodes (2-min TTL)
    let atlas_layer = registry.atlas_layer().clone();
    let acceleration_layer = registry.acceleration_layer().clone();
    registry.register(
        "graph",
        Arc::new(GraphTool::with_session_atlas_and_acceleration(
            client.clone(),
            session.clone(),
            atlas_layer.clone(),
            acceleration_layer.clone(),
        )),
    );
    registry.register(
        "graph_related",
        Arc::new(GraphRelatedTool::with_session_atlas_and_acceleration(
            client.clone(),
            session.clone(),
            atlas_layer.clone(),
            acceleration_layer.clone(),
        )),
    );
    registry.register(
        "graph_dependencies",
        Arc::new(GraphDependenciesTool::with_session_atlas_and_acceleration(
            client.clone(),
            session.clone(),
            atlas_layer.clone(),
            acceleration_layer.clone(),
        )),
    );
    registry.register(
        "graph_impact",
        Arc::new(GraphImpactTool::with_session_atlas_and_acceleration(
            client.clone(),
            session,
            atlas_layer,
            acceleration_layer,
        )),
    );
}

#[cfg(test)]
#[path = "graph_tests.rs"]
mod tests;
