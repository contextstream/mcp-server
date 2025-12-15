import { randomUUID } from 'node:crypto';
import { z } from 'zod';
import type { Config } from './config.js';
import { request } from './http.js';
import { readFilesFromDirectory, readAllFilesInBatches } from './files.js';
import {
  resolveWorkspace,
  readLocalConfig,
  writeLocalConfig,
  addGlobalMapping,
  type WorkspaceConfig
} from './workspace-config.js';
import { globalCache, CacheKeys, CacheTTL } from './cache.js';

const uuidSchema = z.string().uuid();

export class ContextStreamClient {
  constructor(private config: Config) {}

  /**
   * Update the client's default workspace/project IDs at runtime.
   *
   * This is useful for multi-workspace users: once a session is initialized
   * (via repo mapping or explicit session_init), the MCP server can treat that
   * workspace as the default for subsequent calls that don't explicitly include
   * `workspace_id` in the request payload/path/query.
   */
  setDefaults(input: { workspace_id?: string; project_id?: string }) {
    if (input.workspace_id) {
      try {
        uuidSchema.parse(input.workspace_id);
        this.config.defaultWorkspaceId = input.workspace_id;
      } catch {
        // ignore invalid IDs
      }
    }
    if (input.project_id) {
      try {
        uuidSchema.parse(input.project_id);
        this.config.defaultProjectId = input.project_id;
      } catch {
        // ignore invalid IDs
      }
    }
  }

  private withDefaults<T extends { workspace_id?: string; project_id?: string }>(
    input: T
  ): T {
    const { defaultWorkspaceId, defaultProjectId } = this.config;
    return {
      ...input,
      workspace_id: input.workspace_id || defaultWorkspaceId,
      project_id: input.project_id || defaultProjectId,
    } as T;
  }

  // Auth
  me() {
    return request(this.config, '/auth/me');
  }

  // Workspaces & Projects
  listWorkspaces(params?: { page?: number; page_size?: number }) {
    const query = new URLSearchParams();
    if (params?.page) query.set('page', String(params.page));
    if (params?.page_size) query.set('page_size', String(params.page_size));
    const suffix = query.toString() ? `?${query.toString()}` : '';
    return request(this.config, `/workspaces${suffix}`);
  }

  createWorkspace(input: { name: string; description?: string; visibility?: string }) {
    return request(this.config, '/workspaces', { body: input });
  }

  updateWorkspace(workspaceId: string, input: { name?: string; description?: string; visibility?: string }) {
    uuidSchema.parse(workspaceId);
    return request(this.config, `/workspaces/${workspaceId}`, { method: 'PUT', body: input });
  }

  deleteWorkspace(workspaceId: string) {
    uuidSchema.parse(workspaceId);
    return request(this.config, `/workspaces/${workspaceId}`, { method: 'DELETE' });
  }

  listProjects(params?: { workspace_id?: string; page?: number; page_size?: number }) {
    const withDefaults = this.withDefaults(params || {});
    const query = new URLSearchParams();
    if (withDefaults.workspace_id) query.set('workspace_id', withDefaults.workspace_id);
    if (params?.page) query.set('page', String(params.page));
    if (params?.page_size) query.set('page_size', String(params.page_size));
    const suffix = query.toString() ? `?${query.toString()}` : '';
    return request(this.config, `/projects${suffix}`);
  }

  createProject(input: { name: string; description?: string; workspace_id?: string }) {
    const payload = this.withDefaults(input);
    return request(this.config, '/projects', { body: payload });
  }

  updateProject(projectId: string, input: { name?: string; description?: string }) {
    uuidSchema.parse(projectId);
    return request(this.config, `/projects/${projectId}`, { method: 'PUT', body: input });
  }

  deleteProject(projectId: string) {
    uuidSchema.parse(projectId);
    return request(this.config, `/projects/${projectId}`, { method: 'DELETE' });
  }

  indexProject(projectId: string) {
    uuidSchema.parse(projectId);
    return request(this.config, `/projects/${projectId}/index`, { body: {} });
  }

  // Search - each method adds required search_type and filters fields
  searchSemantic(body: { query: string; workspace_id?: string; project_id?: string; limit?: number }) {
    return request(this.config, '/search/semantic', { 
      body: { 
        ...this.withDefaults(body), 
        search_type: 'semantic',
        filters: body.workspace_id ? {} : { file_types: [], languages: [], file_paths: [], exclude_paths: [], content_types: [], tags: [] }
      } 
    });
  }

  searchHybrid(body: { query: string; workspace_id?: string; project_id?: string; limit?: number }) {
    return request(this.config, '/search/hybrid', { 
      body: { 
        ...this.withDefaults(body), 
        search_type: 'hybrid',
        filters: body.workspace_id ? {} : { file_types: [], languages: [], file_paths: [], exclude_paths: [], content_types: [], tags: [] }
      } 
    });
  }

  searchKeyword(body: { query: string; workspace_id?: string; project_id?: string; limit?: number }) {
    return request(this.config, '/search/keyword', { 
      body: { 
        ...this.withDefaults(body), 
        search_type: 'keyword',
        filters: body.workspace_id ? {} : { file_types: [], languages: [], file_paths: [], exclude_paths: [], content_types: [], tags: [] }
      } 
    });
  }

  searchPattern(body: { query: string; workspace_id?: string; project_id?: string; limit?: number }) {
    return request(this.config, '/search/pattern', { 
      body: { 
        ...this.withDefaults(body), 
        search_type: 'pattern',
        filters: body.workspace_id ? {} : { file_types: [], languages: [], file_paths: [], exclude_paths: [], content_types: [], tags: [] }
      } 
    });
  }

  // Memory / Knowledge
  createMemoryEvent(body: {
    workspace_id?: string;
    project_id?: string;
    event_type: string;
    title: string;
    content: string;
    metadata?: Record<string, unknown>;
  }) {
    const withDefaults = this.withDefaults(body);
    
    // Validate required fields
    if (!withDefaults.workspace_id) {
      throw new Error('workspace_id is required for creating memory events. Set defaultWorkspaceId in config or provide workspace_id.');
    }
    
    // Ensure content is not empty
    if (!body.content || body.content.trim().length === 0) {
      throw new Error('content is required and cannot be empty');
    }
    
    return request(this.config, '/memory/events', { body: withDefaults });
  }

  bulkIngestEvents(body: { workspace_id?: string; project_id?: string; events: any[] }) {
    return request(this.config, '/memory/events/ingest', { body: this.withDefaults(body) });
  }

  listMemoryEvents(params?: { workspace_id?: string; project_id?: string; limit?: number }) {
    const withDefaults = this.withDefaults(params || {});
    if (!withDefaults.workspace_id) {
      throw new Error('workspace_id is required for listing memory events');
    }
    const query = new URLSearchParams();
    if (params?.limit) query.set('limit', String(params.limit));
    if (withDefaults.project_id) query.set('project_id', withDefaults.project_id);
    const suffix = query.toString() ? `?${query.toString()}` : '';
    return request(this.config, `/memory/events/workspace/${withDefaults.workspace_id}${suffix}`, { method: 'GET' });
  }

  createKnowledgeNode(body: {
    workspace_id?: string;
    project_id?: string;
    node_type: string;
    title: string;
    content: string;
    relations?: Array<{ type: string; target_id: string }>;
  }) {
    return request(this.config, '/memory/nodes', { body: this.withDefaults(body) });
  }

  listKnowledgeNodes(params?: { workspace_id?: string; project_id?: string; limit?: number }) {
    const withDefaults = this.withDefaults(params || {});
    if (!withDefaults.workspace_id) {
      throw new Error('workspace_id is required for listing knowledge nodes');
    }
    const query = new URLSearchParams();
    if (params?.limit) query.set('limit', String(params.limit));
    if (withDefaults.project_id) query.set('project_id', withDefaults.project_id);
    const suffix = query.toString() ? `?${query.toString()}` : '';
    return request(this.config, `/memory/nodes/workspace/${withDefaults.workspace_id}${suffix}`, { method: 'GET' });
  }

  memorySearch(body: { query: string; workspace_id?: string; project_id?: string; limit?: number }) {
    return request(this.config, '/memory/search', { body: this.withDefaults(body) });
  }

  memoryDecisions(params?: { workspace_id?: string; project_id?: string; limit?: number }) {
    const query = new URLSearchParams();
    const withDefaults = this.withDefaults(params || {});
    if (withDefaults.workspace_id) query.set('workspace_id', withDefaults.workspace_id);
    if (withDefaults.project_id) query.set('project_id', withDefaults.project_id);
    if (params?.limit) query.set('limit', String(params.limit));
    const suffix = query.toString() ? `?${query.toString()}` : '';
    return request(this.config, `/memory/search/decisions${suffix}`, { method: 'GET' });
  }

  // Graph
  graphRelated(body: { workspace_id?: string; project_id?: string; node_id: string; limit?: number }) {
    return request(this.config, '/graph/knowledge/related', { body: this.withDefaults(body) });
  }

  graphPath(body: { workspace_id?: string; project_id?: string; source_id: string; target_id: string }) {
    return request(this.config, '/graph/knowledge/path', { body: this.withDefaults(body) });
  }

  graphDecisions(body?: { workspace_id?: string; project_id?: string; limit?: number }) {
    return request(this.config, '/graph/knowledge/decisions', { body: this.withDefaults(body || {}) });
  }

  graphDependencies(body: { target: { type: string; id: string }; max_depth?: number; include_transitive?: boolean }) {
    return request(this.config, '/graph/dependencies', { body });
  }

  graphCallPath(body: { source: { type: string; id: string }; target: { type: string; id: string }; max_depth?: number }) {
    return request(this.config, '/graph/call-paths', { body });
  }

  graphImpact(body: { target: { type: string; id: string }; max_depth?: number }) {
    return request(this.config, '/graph/impact-analysis', { body });
  }

  // AI
  aiContext(body: {
    query: string;
    workspace_id?: string;
    project_id?: string;
    include_code?: boolean;
    include_docs?: boolean;
    include_memory?: boolean;
    limit?: number;
  }) {
    return request(this.config, '/ai/context', { body: this.withDefaults(body) });
  }

  aiEmbeddings(body: { text: string }) {
    return request(this.config, '/ai/embeddings', { body });
  }

  aiPlan(body: { description: string; project_id?: string; complexity?: string }) {
    return request(this.config, '/ai/plan/generate', { body: this.withDefaults(body) });
  }

  aiTasks(body: { plan_id?: string; description?: string; project_id?: string; granularity?: string }) {
    return request(this.config, '/ai/tasks/generate', { body: this.withDefaults(body) });
  }

  aiEnhancedContext(body: {
    query: string;
    workspace_id?: string;
    project_id?: string;
    include_code?: boolean;
    include_docs?: boolean;
    include_memory?: boolean;
    limit?: number;
  }) {
    return request(this.config, '/ai/context/enhanced', { body: this.withDefaults(body) });
  }

  // Project extended operations (with caching)
  async getProject(projectId: string) {
    uuidSchema.parse(projectId);
    
    const cacheKey = CacheKeys.project(projectId);
    const cached = globalCache.get(cacheKey);
    if (cached) return cached;
    
    const result = await request(this.config, `/projects/${projectId}`, { method: 'GET' });
    globalCache.set(cacheKey, result, CacheTTL.PROJECT);
    return result;
  }

  async projectOverview(projectId: string) {
    uuidSchema.parse(projectId);
    
    const cacheKey = `project_overview:${projectId}`;
    const cached = globalCache.get(cacheKey);
    if (cached) return cached;
    
    const result = await request(this.config, `/projects/${projectId}/overview`, { method: 'GET' });
    globalCache.set(cacheKey, result, CacheTTL.PROJECT);
    return result;
  }

  projectStatistics(projectId: string) {
    uuidSchema.parse(projectId);
    return request(this.config, `/projects/${projectId}/statistics`, { method: 'GET' });
  }

  projectFiles(projectId: string) {
    uuidSchema.parse(projectId);
    return request(this.config, `/projects/${projectId}/files`, { method: 'GET' });
  }

  projectIndexStatus(projectId: string) {
    uuidSchema.parse(projectId);
    return request(this.config, `/projects/${projectId}/index/status`, { method: 'GET' });
  }

  /**
   * Ingest files for indexing
   * This uploads files to the API for indexing
   */
  ingestFiles(projectId: string, files: Array<{ path: string; content: string; language?: string }>) {
    uuidSchema.parse(projectId);
    return request(this.config, `/projects/${projectId}/files/ingest`, {
      body: { files },
    });
  }

  // Workspace extended operations (with caching)
  async getWorkspace(workspaceId: string) {
    uuidSchema.parse(workspaceId);
    
    const cacheKey = CacheKeys.workspace(workspaceId);
    const cached = globalCache.get(cacheKey);
    if (cached) return cached;
    
    const result = await request(this.config, `/workspaces/${workspaceId}`, { method: 'GET' });
    globalCache.set(cacheKey, result, CacheTTL.WORKSPACE);
    return result;
  }

  async workspaceOverview(workspaceId: string) {
    uuidSchema.parse(workspaceId);
    
    const cacheKey = `workspace_overview:${workspaceId}`;
    const cached = globalCache.get(cacheKey);
    if (cached) return cached;
    
    const result = await request(this.config, `/workspaces/${workspaceId}/overview`, { method: 'GET' });
    globalCache.set(cacheKey, result, CacheTTL.WORKSPACE);
    return result;
  }

  workspaceAnalytics(workspaceId: string) {
    uuidSchema.parse(workspaceId);
    return request(this.config, `/workspaces/${workspaceId}/analytics`, { method: 'GET' });
  }

  workspaceContent(workspaceId: string) {
    uuidSchema.parse(workspaceId);
    return request(this.config, `/workspaces/${workspaceId}/content`, { method: 'GET' });
  }

  // Memory extended operations
  getMemoryEvent(eventId: string) {
    uuidSchema.parse(eventId);
    return request(this.config, `/memory/events/${eventId}`, { method: 'GET' });
  }

  updateMemoryEvent(eventId: string, body: { title?: string; content?: string; metadata?: Record<string, any> }) {
    uuidSchema.parse(eventId);
    return request(this.config, `/memory/events/${eventId}`, { method: 'PUT', body });
  }

  deleteMemoryEvent(eventId: string) {
    uuidSchema.parse(eventId);
    return request(this.config, `/memory/events/${eventId}`, { method: 'DELETE' });
  }

  distillMemoryEvent(eventId: string) {
    uuidSchema.parse(eventId);
    return request(this.config, `/memory/events/${eventId}/distill`, { body: {} });
  }

  getKnowledgeNode(nodeId: string) {
    uuidSchema.parse(nodeId);
    return request(this.config, `/memory/nodes/${nodeId}`, { method: 'GET' });
  }

  updateKnowledgeNode(nodeId: string, body: { title?: string; content?: string; relations?: Array<{ type: string; target_id: string }> }) {
    uuidSchema.parse(nodeId);
    return request(this.config, `/memory/nodes/${nodeId}`, { method: 'PUT', body });
  }

  deleteKnowledgeNode(nodeId: string) {
    uuidSchema.parse(nodeId);
    return request(this.config, `/memory/nodes/${nodeId}`, { method: 'DELETE' });
  }

  supersedeKnowledgeNode(nodeId: string, body: { new_content: string; reason?: string }) {
    uuidSchema.parse(nodeId);
    return request(this.config, `/memory/nodes/${nodeId}/supersede`, { body });
  }

  memoryTimeline(workspaceId: string) {
    uuidSchema.parse(workspaceId);
    return request(this.config, `/memory/search/timeline/${workspaceId}`, { method: 'GET' });
  }

  memorySummary(workspaceId: string) {
    uuidSchema.parse(workspaceId);
    return request(this.config, `/memory/search/summary/${workspaceId}`, { method: 'GET' });
  }

  // Graph extended operations
  findCircularDependencies(projectId: string) {
    uuidSchema.parse(projectId);
    return request(this.config, `/graph/circular-dependencies/${projectId}`, { method: 'GET' });
  }

  findUnusedCode(projectId: string) {
    uuidSchema.parse(projectId);
    return request(this.config, `/graph/unused-code/${projectId}`, { method: 'GET' });
  }

  findContradictions(nodeId: string) {
    uuidSchema.parse(nodeId);
    return request(this.config, `/graph/knowledge/contradictions/${nodeId}`, { method: 'GET' });
  }

  // Search suggestions
  searchSuggestions(body: { query: string; workspace_id?: string; project_id?: string }) {
    return request(this.config, '/search/suggest', { body: this.withDefaults(body) });
  }

  // ============================================
  // Session & Auto-Context Initialization
  // ============================================

  /**
   * Initialize a conversation session and retrieve relevant context automatically.
   * This is the key tool for AI assistants to get context at the start of a conversation.
   * 
   * Discovery chain:
   * 1. Check local .contextstream/config.json in repo root
   * 2. Check parent folder heuristic mappings (~/.contextstream-mappings.json)
   * 3. If ambiguous, return workspace candidates for user/agent selection
   * 
   * Once workspace is resolved, loads WORKSPACE-LEVEL context (not just project),
   * ensuring cross-project decisions and memory are available.
   */
  async initSession(params: {
    workspace_id?: string;
    project_id?: string;
    session_id?: string;
    context_hint?: string;
    include_recent_memory?: boolean;
    include_decisions?: boolean;
    include_user_preferences?: boolean;
    auto_index?: boolean;
  }, ideRoots: string[] = []) {
    let workspaceId = params.workspace_id || this.config.defaultWorkspaceId;
    let projectId = params.project_id || this.config.defaultProjectId;
    let workspaceName: string | undefined;
    
    // Build comprehensive initial context
    const context: Record<string, unknown> = {
      session_id: params.session_id || randomUUID(),
      initialized_at: new Date().toISOString(),
    };

    const rootPath = ideRoots.length > 0 ? ideRoots[0] : undefined;

    // ========================================
    // STEP 1: Workspace Discovery Chain
    // ========================================
    if (!workspaceId && rootPath) {
      // Try local config and parent mappings first
      const resolved = resolveWorkspace(rootPath);
      
      if (resolved.config) {
        workspaceId = resolved.config.workspace_id;
        workspaceName = resolved.config.workspace_name;
        projectId = resolved.config.project_id || projectId;
        context.workspace_source = resolved.source;
        context.workspace_resolved_from = resolved.source === 'local_config' 
          ? `${rootPath}/.contextstream/config.json`
          : 'parent_folder_mapping';
      } else {
        // No local config - try to find matching workspace by name or project
        const folderName = rootPath?.split('/').pop()?.toLowerCase() || '';
        
        try {
          const workspaces = await this.listWorkspaces({ page_size: 50 }) as { 
            items?: Array<{ id: string; name: string; description?: string }> 
          };
          
          if (workspaces.items && workspaces.items.length > 0) {
            // Try to find a workspace with a matching or similar name
            let matchedWorkspace: { id: string; name: string; description?: string } | undefined;
            let matchSource: string | undefined;
            
            // 1. Exact name match (case-insensitive)
            matchedWorkspace = workspaces.items.find(
              w => w.name.toLowerCase() === folderName
            );
            if (matchedWorkspace) {
              matchSource = 'workspace_name_exact';
            }
            
            // 2. Workspace name contains folder name or vice versa
            if (!matchedWorkspace) {
              matchedWorkspace = workspaces.items.find(
                w => w.name.toLowerCase().includes(folderName) || 
                     folderName.includes(w.name.toLowerCase())
              );
              if (matchedWorkspace) {
                matchSource = 'workspace_name_partial';
              }
            }
            
            // 3. Check if any workspace has a project with matching name
            if (!matchedWorkspace) {
              for (const ws of workspaces.items) {
                try {
                  const projects = await this.listProjects({ workspace_id: ws.id, page_size: 50 }) as { 
                    items?: Array<{ id: string; name: string }> 
                  };
                  const matchingProject = projects.items?.find(
                    p => p.name.toLowerCase() === folderName ||
                         p.name.toLowerCase().includes(folderName) ||
                         folderName.includes(p.name.toLowerCase())
                  );
                  if (matchingProject) {
                    matchedWorkspace = ws;
                    matchSource = 'project_name_match';
                    projectId = matchingProject.id;
                    context.project_source = 'matched_existing';
                    break;
                  }
                } catch { /* continue checking other workspaces */ }
              }
            }
            
            if (matchedWorkspace) {
              // Found a matching workspace - use it
              workspaceId = matchedWorkspace.id;
              workspaceName = matchedWorkspace.name;
              context.workspace_source = matchSource;
              context.workspace_auto_matched = true;
              
              // Save to local config for next time
              writeLocalConfig(rootPath, {
                workspace_id: matchedWorkspace.id,
                workspace_name: matchedWorkspace.name,
                associated_at: new Date().toISOString(),
              });
            } else {
              // No match found - need user selection
              context.status = 'requires_workspace_selection';
              context.workspace_candidates = workspaces.items.map(w => ({
                id: w.id,
                name: w.name,
                description: w.description,
              }));
              context.message = `New folder detected: "${rootPath?.split('/').pop()}". Please select which workspace this belongs to, or create a new one.`;
              context.ide_roots = ideRoots;
              context.folder_name = rootPath?.split('/').pop();
              
              // Return early - agent needs to ask user
              return context;
            }
          } else {
            // No workspaces exist - create one with folder name
            const newWorkspace = await this.createWorkspace({
              name: folderName || 'My Workspace',
              description: `Workspace created for ${rootPath}`,
              visibility: 'private',
            }) as { id?: string; name?: string };
            if (newWorkspace.id) {
              workspaceId = newWorkspace.id;
              workspaceName = newWorkspace.name;
              context.workspace_source = 'auto_created';
              context.workspace_created = true;
              
              // Save to local config for next time
              writeLocalConfig(rootPath, {
                workspace_id: newWorkspace.id,
                workspace_name: newWorkspace.name,
                associated_at: new Date().toISOString(),
              });
            }
          }
        } catch (e) {
          context.workspace_error = String(e);
        }
      }
    }

    // Fallback: if still no workspace and no IDE roots, pick first available
    if (!workspaceId && !rootPath) {
      try {
        const workspaces = await this.listWorkspaces({ page_size: 1 }) as { items?: Array<{ id: string; name: string }> };
        if (workspaces.items && workspaces.items.length > 0) {
          workspaceId = workspaces.items[0].id;
          workspaceName = workspaces.items[0].name;
          context.workspace_source = 'fallback_first';
        }
      } catch (e) {
        context.workspace_error = String(e);
      }
    }

    // ========================================
    // STEP 2: Project Discovery
    // ========================================
    if (!projectId && workspaceId && rootPath && params.auto_index !== false) {
      const projectName = rootPath.split('/').pop() || 'My Project';
      
      try {
        // Check if a project with this name (or similar) already exists in this workspace
        const projects = await this.listProjects({ workspace_id: workspaceId }) as { items?: Array<{ id: string; name: string }> };
        const projectNameLower = projectName.toLowerCase();
        
        // Try exact match first, then partial match
        let existingProject = projects.items?.find(p => p.name.toLowerCase() === projectNameLower);
        if (existingProject) {
          context.project_match_type = 'exact';
        } else {
          existingProject = projects.items?.find(
            p => p.name.toLowerCase().includes(projectNameLower) ||
                 projectNameLower.includes(p.name.toLowerCase())
          );
          if (existingProject) {
            context.project_match_type = 'partial';
          }
        }
        
        if (existingProject) {
          projectId = existingProject.id;
          context.project_source = 'existing';
          context.matched_project_name = existingProject.name;
        } else {
          // Create project from IDE root
          const newProject = await this.createProject({
            name: projectName,
            description: `Auto-created from ${rootPath}`,
            workspace_id: workspaceId,
          }) as { id?: string };
          
          if (newProject.id) {
            projectId = newProject.id;
            context.project_source = 'auto_created';
            context.project_created = true;
            context.project_path = rootPath;
          }
        }
        
        // Update local config with project info
        if (projectId) {
          const existingConfig = readLocalConfig(rootPath);
          if (existingConfig || workspaceId) {
            writeLocalConfig(rootPath, {
              workspace_id: workspaceId!,
              workspace_name: workspaceName,
              project_id: projectId,
              project_name: projectName,
              associated_at: existingConfig?.associated_at || new Date().toISOString(),
            });
          }
        }
        
        // Ingest files if auto_index is enabled (default: true)
        // Runs in BACKGROUND - does not block session_init
        if (projectId && (params.auto_index === undefined || params.auto_index === true)) {
          context.indexing_status = 'started';
          
          // Fire-and-forget: start indexing in background
          const projectIdCopy = projectId;
          const rootPathCopy = rootPath;
          (async () => {
            try {
              for await (const batch of readAllFilesInBatches(rootPathCopy, { batchSize: 50 })) {
                await this.ingestFiles(projectIdCopy, batch);
              }
              console.error(`[ContextStream] Background indexing completed for ${rootPathCopy}`);
            } catch (e) {
              console.error(`[ContextStream] Background indexing failed:`, e);
            }
          })();
        }
      } catch (e) {
        context.project_error = String(e);
      }
    }

    context.status = 'connected';
    context.workspace_id = workspaceId;
    context.workspace_name = workspaceName;
    context.project_id = projectId;
    context.ide_roots = ideRoots;

    // ========================================
    // STEP 3: Load Context via Batched Endpoint
    // Single API call instead of 5-6 separate calls
    // ========================================
    if (workspaceId) {
      try {
        const batchedContext = await this._fetchSessionContextBatched({
          workspace_id: workspaceId,
          project_id: projectId,
          session_id: context.session_id as string,
          context_hint: params.context_hint,
          include_recent_memory: params.include_recent_memory !== false,
          include_decisions: params.include_decisions !== false,
        });
        
        // Merge batched response into context
        if (batchedContext.workspace) {
          context.workspace = batchedContext.workspace;
        }
        if (batchedContext.project) {
          context.project = batchedContext.project;
        }
        if (batchedContext.recent_memory) {
          context.recent_memory = { items: batchedContext.recent_memory };
        }
        if (batchedContext.recent_decisions) {
          context.recent_decisions = { items: batchedContext.recent_decisions };
        }
        if (batchedContext.relevant_context) {
          context.relevant_context = batchedContext.relevant_context;
        }

        // Load high-priority lessons (critical/high severity)
        try {
          const lessons = await this.getHighPriorityLessons({
            workspace_id: workspaceId,
            project_id: projectId,
            context_hint: params.context_hint,
            limit: 5,
          });
          if (lessons.length > 0) {
            context.lessons = lessons;
            context.lessons_warning = `⚠️ ${lessons.length} lesson(s) from past mistakes. Review before making changes.`;
          }
        } catch { /* optional */ }
      } catch (e) {
        // Fallback to individual calls if batched endpoint fails
        console.error('[ContextStream] Batched endpoint failed, falling back to individual calls:', e);
        await this._fetchSessionContextFallback(context, workspaceId, projectId, params);
      }
    }

    return context;
  }

  /**
   * Fetch session context using the batched /session/init endpoint.
   * This is much faster than making 5-6 individual API calls.
   */
  private async _fetchSessionContextBatched(params: {
    workspace_id: string;
    project_id?: string;
    session_id?: string;
    context_hint?: string;
    include_recent_memory?: boolean;
    include_decisions?: boolean;
  }): Promise<{
    workspace?: { id: string; name: string; description?: string };
    project?: { id: string; name: string; description?: string };
    recent_memory?: unknown[];
    recent_decisions?: unknown[];
    relevant_context?: unknown;
  }> {
    interface SessionContextData {
      workspace?: { id: string; name: string; description?: string };
      project?: { id: string; name: string; description?: string };
      recent_memory?: unknown[];
      recent_decisions?: unknown[];
      relevant_context?: unknown;
    }

    // Check cache first
    const cacheKey = CacheKeys.sessionInit(params.workspace_id, params.project_id);
    const cached = globalCache.get<SessionContextData>(cacheKey);
    if (cached) {
      console.error('[ContextStream] Session context cache HIT');
      return cached;
    }

    // Call batched endpoint
    const result = await request(this.config, '/session/init', {
      body: {
        workspace_id: params.workspace_id,
        project_id: params.project_id,
        session_id: params.session_id,
        include_recent_memory: params.include_recent_memory ?? true,
        include_decisions: params.include_decisions ?? true,
      },
    }) as { data?: SessionContextData } | SessionContextData;

    // Handle both wrapped {data: ...} and direct response formats
    const contextData: SessionContextData = 'data' in result && result.data ? result.data : result as SessionContextData;
    
    // Cache the result
    globalCache.set(cacheKey, contextData, CacheTTL.SESSION_INIT);
    
    return contextData;
  }

  /**
   * Fallback to individual API calls if batched endpoint is unavailable.
   */
  private async _fetchSessionContextFallback(
    context: Record<string, unknown>,
    workspaceId: string,
    projectId: string | undefined,
    params: { include_recent_memory?: boolean; include_decisions?: boolean; context_hint?: string }
  ): Promise<void> {
    // Individual calls (slower but more compatible)
    try {
      context.workspace = await this.workspaceOverview(workspaceId);
    } catch { /* optional */ }

    if (projectId) {
      try {
        context.project = await this.projectOverview(projectId);
      } catch { /* optional */ }
    }

    if (params.include_recent_memory !== false) {
      try {
        context.recent_memory = await this.listMemoryEvents({
          workspace_id: workspaceId,
          limit: 10,
        });
      } catch { /* optional */ }
    }

    if (params.include_decisions !== false) {
      try {
        context.recent_decisions = await this.memoryDecisions({
          workspace_id: workspaceId,
          limit: 5,
        });
      } catch { /* optional */ }
    }

    if (params.context_hint) {
      try {
        context.relevant_context = await this.memorySearch({
          query: params.context_hint,
          workspace_id: workspaceId,
          limit: 5,
        });
      } catch { /* optional */ }
    }

    // Load high-priority lessons (critical/high severity)
    try {
      const lessons = await this.getHighPriorityLessons({
        workspace_id: workspaceId,
        project_id: projectId,
        context_hint: params.context_hint,
        limit: 5,
      });
      if (lessons.length > 0) {
        context.lessons = lessons;
        context.lessons_warning = `⚠️ ${lessons.length} lesson(s) from past mistakes. Review before making changes.`;
      }
    } catch { /* optional */ }
  }

  /**
   * Associate a folder with a workspace (called after user selects from candidates).
   * Persists the selection to .contextstream/config.json for future sessions.
   */
  async associateWorkspace(params: {
    folder_path: string;
    workspace_id: string;
    workspace_name?: string;
    create_parent_mapping?: boolean; // Also create a parent folder mapping
  }) {
    const { folder_path, workspace_id, workspace_name, create_parent_mapping } = params;
    
    // Save local config
    const saved = writeLocalConfig(folder_path, {
      workspace_id,
      workspace_name,
      associated_at: new Date().toISOString(),
    });

    // Optionally create parent folder mapping (e.g., /home/user/dev/company/* -> workspace)
    if (create_parent_mapping) {
      const parentDir = folder_path.split('/').slice(0, -1).join('/');
      addGlobalMapping({
        pattern: `${parentDir}/*`,
        workspace_id,
        workspace_name: workspace_name || 'Unknown',
      });
    }

    return {
      success: saved,
      config_path: `${folder_path}/.contextstream/config.json`,
      workspace_id,
      workspace_name,
      parent_mapping_created: create_parent_mapping || false,
    };
  }

  /**
   * Get user preferences and persona from memory.
   * Useful for AI to understand user's coding style, preferences, etc.
   */
  async getUserContext(params: { workspace_id?: string }) {
    const withDefaults = this.withDefaults(params);
    
    if (!withDefaults.workspace_id) {
      throw new Error('workspace_id is required for getUserContext');
    }

    const context: Record<string, unknown> = {};

    // Search for user preferences
    try {
      context.preferences = await this.memorySearch({
        query: 'user preferences coding style settings',
        workspace_id: withDefaults.workspace_id,
        limit: 10,
      });
    } catch { /* optional */ }

    // Get memory summary for overall context
    try {
      context.summary = await this.memorySummary(withDefaults.workspace_id);
    } catch { /* optional */ }

    return context;
  }

  /**
   * Capture and store conversation context automatically.
   * Call this to persist important context from the current conversation.
   */
  async captureContext(params: {
    workspace_id?: string;
    project_id?: string;
    session_id?: string;
    event_type: 'conversation' | 'decision' | 'insight' | 'preference' | 'task' | 'bug' | 'feature' | 'correction' | 'lesson' | 'warning' | 'frustration';
    title: string;
    content: string;
    tags?: string[];
    importance?: 'low' | 'medium' | 'high' | 'critical';
  }) {
    const withDefaults = this.withDefaults(params);

    // Map high-level types to API EventType
    let apiEventType = 'manual_note';
    const tags = params.tags || [];

    switch (params.event_type) {
      case 'conversation':
        apiEventType = 'chat';
        break;
      case 'task':
        apiEventType = 'task_created';
        break;
      case 'bug':
      case 'feature':
        apiEventType = 'ticket';
        tags.push(params.event_type);
        break;
      case 'decision':
      case 'insight':
      case 'preference':
        apiEventType = 'manual_note';
        tags.push(params.event_type);
        break;
      // Lesson system types - all stored as manual_note with specific tags
      case 'correction':
      case 'lesson':
      case 'warning':
      case 'frustration':
        apiEventType = 'manual_note';
        tags.push(params.event_type);
        // Add lesson-related tag for easier filtering
        if (!tags.includes('lesson_system')) {
          tags.push('lesson_system');
        }
        break;
      default:
        apiEventType = 'manual_note';
        tags.push(params.event_type);
    }

    return this.createMemoryEvent({
      workspace_id: withDefaults.workspace_id,
      project_id: withDefaults.project_id,
      event_type: apiEventType,
      title: params.title,
      content: params.content,
      metadata: {
        original_type: params.event_type,
        session_id: params.session_id,
        tags: tags,
        importance: params.importance || 'medium',
        captured_at: new Date().toISOString(),
        source: 'mcp_auto_capture',
      },
    });
  }

  /**
   * Search memory with automatic context enrichment.
   * Returns both direct matches and related context.
   */
  async smartSearch(params: {
    query: string;
    workspace_id?: string;
    project_id?: string;
    include_related?: boolean;
    include_decisions?: boolean;
  }) {
    const withDefaults = this.withDefaults(params);
    
    const results: Record<string, unknown> = {};

    // Primary memory search
    try {
      results.memory_results = await this.memorySearch({
        query: params.query,
        workspace_id: withDefaults.workspace_id,
        project_id: withDefaults.project_id,
        limit: 10,
      });
    } catch { /* optional */ }

    // Semantic code search if project specified
    if (withDefaults.project_id) {
      try {
        results.code_results = await this.searchSemantic({
          query: params.query,
          workspace_id: withDefaults.workspace_id,
          project_id: withDefaults.project_id,
          limit: 5,
        });
      } catch { /* optional */ }
    }

    // Include related decisions
    if (params.include_decisions !== false && withDefaults.workspace_id) {
      try {
        results.related_decisions = await this.memoryDecisions({
          workspace_id: withDefaults.workspace_id,
          project_id: withDefaults.project_id,
          limit: 3,
        });
      } catch { /* optional */ }
    }

    return results;
  }

  // ============================================
  // Token-Saving Context Tools
  // ============================================

  /**
   * Get a compact, token-efficient summary of workspace context.
   * Designed to be included in every AI prompt without consuming many tokens.
   *
   * Target: ~500 tokens max
   *
   * This replaces loading full chat history - AI can call session_recall
   * for specific details when needed.
   */
  async getContextSummary(params: {
    workspace_id?: string;
    project_id?: string;
    max_tokens?: number;
  }): Promise<{
    summary: string;
    workspace_name?: string;
    project_name?: string;
    decision_count: number;
    memory_count: number;
  }> {
    const withDefaults = this.withDefaults(params);
    const maxTokens = params.max_tokens || 500;
    
    if (!withDefaults.workspace_id) {
      return {
        summary: 'No workspace context loaded. Call session_init first.',
        decision_count: 0,
        memory_count: 0,
      };
    }

    const parts: string[] = [];
    let workspaceName: string | undefined;
    let projectName: string | undefined;
    let decisionCount = 0;
    let memoryCount = 0;

    // Get workspace info (cached)
    try {
      const ws = await this.getWorkspace(withDefaults.workspace_id) as { name?: string };
      workspaceName = ws?.name;
      if (workspaceName) {
        parts.push(`📁 Workspace: ${workspaceName}`);
      }
    } catch { /* optional */ }

    // Get project info if specified (cached)
    if (withDefaults.project_id) {
      try {
        const proj = await this.getProject(withDefaults.project_id) as { name?: string };
        projectName = proj?.name;
        if (projectName) {
          parts.push(`📂 Project: ${projectName}`);
        }
      } catch { /* optional */ }
    }

    // Get recent decisions (titles only for token efficiency)
    try {
      const decisions = await this.memoryDecisions({
        workspace_id: withDefaults.workspace_id,
        project_id: withDefaults.project_id,
        limit: 5,
      }) as { items?: Array<{ title?: string }> };
      
      if (decisions.items && decisions.items.length > 0) {
        decisionCount = decisions.items.length;
        parts.push('');
        parts.push('📋 Recent Decisions:');
        decisions.items.slice(0, 3).forEach((d, i) => {
          parts.push(`  ${i + 1}. ${d.title || 'Untitled'}`);
        });
        if (decisions.items.length > 3) {
          parts.push(`  (+${decisions.items.length - 3} more)`);
        }
      }
    } catch { /* optional */ }

    // Get preferences count and sample
    try {
      const prefs = await this.memorySearch({
        query: 'user preferences coding style settings',
        workspace_id: withDefaults.workspace_id,
        limit: 5,
      }) as { results?: Array<{ title?: string }> };
      
      if (prefs.results && prefs.results.length > 0) {
        parts.push('');
        parts.push('⚙️ Preferences:');
        prefs.results.slice(0, 3).forEach((p) => {
          const title = p.title || 'Preference';
          // Truncate to save tokens
          parts.push(`  • ${title.slice(0, 60)}${title.length > 60 ? '...' : ''}`);
        });
      }
    } catch { /* optional */ }

    // Get memory count
    try {
      const summary = await this.memorySummary(withDefaults.workspace_id) as { events?: number };
      memoryCount = summary.events || 0;
      if (memoryCount > 0) {
        parts.push('');
        parts.push(`🧠 Memory: ${memoryCount} events stored`);
      }
    } catch { /* optional */ }

    // Add usage hint
    parts.push('');
    parts.push('💡 Use session_recall("topic") for specific context');

    const summary = parts.join('\n');

    return {
      summary,
      workspace_name: workspaceName,
      project_name: projectName,
      decision_count: decisionCount,
      memory_count: memoryCount,
    };
  }

  /**
   * Compress chat history into structured memory events.
   * This extracts key information and stores it, allowing the chat
   * history to be cleared while preserving context.
   *
   * Use this at the end of a conversation or when context window is full.
   */
  async compressChat(params: {
    workspace_id?: string;
    project_id?: string;
    chat_history: string;
    extract_types?: Array<'decisions' | 'preferences' | 'insights' | 'tasks' | 'code_patterns'>;
  }): Promise<{
    events_created: number;
    extracted: {
      decisions: string[];
      preferences: string[];
      insights: string[];
      tasks: string[];
      code_patterns: string[];
    };
  }> {
    const withDefaults = this.withDefaults(params);
    
    if (!withDefaults.workspace_id) {
      throw new Error('workspace_id is required for compressChat');
    }

    const extractTypes = params.extract_types || ['decisions', 'preferences', 'insights', 'tasks', 'code_patterns'];
    const extracted: {
      decisions: string[];
      preferences: string[];
      insights: string[];
      tasks: string[];
      code_patterns: string[];
    } = {
      decisions: [],
      preferences: [],
      insights: [],
      tasks: [],
      code_patterns: [],
    };

    let eventsCreated = 0;

    // Simple extraction patterns (AI can do better, but this works without LLM call)
    const lines = params.chat_history.split('\n');
    
    for (const line of lines) {
      const lowerLine = line.toLowerCase();
      
      // Decision patterns
      if (extractTypes.includes('decisions')) {
        if (lowerLine.includes('decided to') ||
            lowerLine.includes('decision:') ||
            lowerLine.includes('we\'ll use') ||
            lowerLine.includes('going with') ||
            lowerLine.includes('chose ')) {
          extracted.decisions.push(line.trim());
        }
      }
      
      // Preference patterns
      if (extractTypes.includes('preferences')) {
        if (lowerLine.includes('prefer') ||
            lowerLine.includes('i like') ||
            lowerLine.includes('always use') ||
            lowerLine.includes('don\'t use') ||
            lowerLine.includes('never use')) {
          extracted.preferences.push(line.trim());
        }
      }
      
      // Task patterns
      if (extractTypes.includes('tasks')) {
        if (lowerLine.includes('todo:') ||
            lowerLine.includes('task:') ||
            lowerLine.includes('need to') ||
            lowerLine.includes('should implement') ||
            lowerLine.includes('will add')) {
          extracted.tasks.push(line.trim());
        }
      }
      
      // Insight patterns
      if (extractTypes.includes('insights')) {
        if (lowerLine.includes('learned that') ||
            lowerLine.includes('realized') ||
            lowerLine.includes('found out') ||
            lowerLine.includes('discovered') ||
            lowerLine.includes('important:') ||
            lowerLine.includes('note:')) {
          extracted.insights.push(line.trim());
        }
      }
      
      // Code pattern patterns
      if (extractTypes.includes('code_patterns')) {
        if (lowerLine.includes('pattern:') ||
            lowerLine.includes('convention:') ||
            lowerLine.includes('style:') ||
            lowerLine.includes('always format') ||
            lowerLine.includes('naming convention')) {
          extracted.code_patterns.push(line.trim());
        }
      }
    }

    // Store extracted items as memory events
    for (const decision of extracted.decisions.slice(0, 5)) {
      try {
        await this.captureContext({
          workspace_id: withDefaults.workspace_id,
          project_id: withDefaults.project_id,
          event_type: 'decision',
          title: decision.slice(0, 100),
          content: decision,
          importance: 'medium',
        });
        eventsCreated++;
      } catch { /* continue */ }
    }

    for (const pref of extracted.preferences.slice(0, 5)) {
      try {
        await this.captureContext({
          workspace_id: withDefaults.workspace_id,
          project_id: withDefaults.project_id,
          event_type: 'preference',
          title: pref.slice(0, 100),
          content: pref,
          importance: 'medium',
        });
        eventsCreated++;
      } catch { /* continue */ }
    }

    for (const task of extracted.tasks.slice(0, 5)) {
      try {
        await this.captureContext({
          workspace_id: withDefaults.workspace_id,
          project_id: withDefaults.project_id,
          event_type: 'task',
          title: task.slice(0, 100),
          content: task,
          importance: 'medium',
        });
        eventsCreated++;
      } catch { /* continue */ }
    }

    for (const insight of extracted.insights.slice(0, 5)) {
      try {
        await this.captureContext({
          workspace_id: withDefaults.workspace_id,
          project_id: withDefaults.project_id,
          event_type: 'insight',
          title: insight.slice(0, 100),
          content: insight,
          importance: 'medium',
        });
        eventsCreated++;
      } catch { /* continue */ }
    }

    return {
      events_created: eventsCreated,
      extracted,
    };
  }

  /**
   * Get context optimized for a token budget.
   * Returns the most relevant context that fits within the specified token limit.
   *
   * This is the key tool for token-efficient AI interactions:
   * - AI calls this with a query and token budget
   * - Gets optimally selected context
   * - No need to include full chat history
   */
  async getContextWithBudget(params: {
    query: string;
    workspace_id?: string;
    project_id?: string;
    max_tokens: number;
    include_decisions?: boolean;
    include_code?: boolean;
    include_memory?: boolean;
  }): Promise<{
    context: string;
    token_estimate: number;
    sources: Array<{ type: string; title: string }>;
  }> {
    const withDefaults = this.withDefaults(params);
    const maxTokens = params.max_tokens || 2000;
    
    // Rough token estimation: ~4 chars per token
    const charsPerToken = 4;
    const maxChars = maxTokens * charsPerToken;
    
    const parts: string[] = [];
    const sources: Array<{ type: string; title: string }> = [];
    let currentChars = 0;

    // Priority 1: Decisions (most valuable per token)
    if (params.include_decisions !== false && withDefaults.workspace_id) {
      try {
        const decisions = await this.memoryDecisions({
          workspace_id: withDefaults.workspace_id,
          project_id: withDefaults.project_id,
          limit: 10,
        }) as { items?: Array<{ title?: string; content?: string }> };
        
        if (decisions.items) {
          parts.push('## Relevant Decisions\n');
          currentChars += 25;
          
          for (const d of decisions.items) {
            const entry = `• ${d.title || 'Decision'}\n`;
            if (currentChars + entry.length > maxChars * 0.4) break; // Reserve 40% for decisions
            parts.push(entry);
            currentChars += entry.length;
            sources.push({ type: 'decision', title: d.title || 'Decision' });
          }
          parts.push('\n');
        }
      } catch { /* optional */ }
    }

    // Priority 2: Memory search results (query-relevant)
    if (params.include_memory !== false && withDefaults.workspace_id) {
      try {
        const memory = await this.memorySearch({
          query: params.query,
          workspace_id: withDefaults.workspace_id,
          project_id: withDefaults.project_id,
          limit: 5,
        }) as { results?: Array<{ title?: string; content?: string }> };
        
        if (memory.results) {
          parts.push('## Related Context\n');
          currentChars += 20;
          
          for (const m of memory.results) {
            // Truncate content to fit budget
            const title = m.title || 'Context';
            const content = m.content?.slice(0, 200) || '';
            const entry = `• ${title}: ${content}...\n`;
            if (currentChars + entry.length > maxChars * 0.7) break; // Reserve 30% for code
            parts.push(entry);
            currentChars += entry.length;
            sources.push({ type: 'memory', title });
          }
          parts.push('\n');
        }
      } catch { /* optional */ }
    }

    // Priority 3: Code search results (if budget allows)
    if (params.include_code && withDefaults.project_id && currentChars < maxChars * 0.8) {
      try {
        const code = await this.searchSemantic({
          query: params.query,
          workspace_id: withDefaults.workspace_id,
          project_id: withDefaults.project_id,
          limit: 3,
        }) as { results?: Array<{ file_path?: string; content?: string }> };
        
        if (code.results) {
          parts.push('## Relevant Code\n');
          currentChars += 18;
          
          for (const c of code.results) {
            const path = c.file_path || 'file';
            const content = c.content?.slice(0, 150) || '';
            const entry = `• ${path}: ${content}...\n`;
            if (currentChars + entry.length > maxChars) break;
            parts.push(entry);
            currentChars += entry.length;
            sources.push({ type: 'code', title: path });
          }
        }
      } catch { /* optional */ }
    }

    const context = parts.join('');
    const tokenEstimate = Math.ceil(context.length / charsPerToken);

    return {
      context,
      token_estimate: tokenEstimate,
      sources,
    };
  }

  /**
   * Get incremental context changes since a given timestamp.
   * Useful for syncing context without reloading everything.
   */
  async getContextDelta(params: {
    workspace_id?: string;
    project_id?: string;
    since: string; // ISO timestamp
    limit?: number;
  }): Promise<{
    new_decisions: number;
    new_memory: number;
    items: Array<{ type: string; title: string; created_at: string }>;
  }> {
    const withDefaults = this.withDefaults(params);
    
    if (!withDefaults.workspace_id) {
      return { new_decisions: 0, new_memory: 0, items: [] };
    }

    const items: Array<{ type: string; title: string; created_at: string }> = [];
    let newDecisions = 0;
    let newMemory = 0;

    try {
      const memory = await this.listMemoryEvents({
        workspace_id: withDefaults.workspace_id,
        project_id: withDefaults.project_id,
        limit: params.limit || 20,
      }) as { items?: Array<{ title?: string; created_at?: string; metadata?: { original_type?: string } }> };

      if (memory.items) {
        for (const item of memory.items) {
          const createdAt = item.created_at || '';
          if (createdAt > params.since) {
            const type = item.metadata?.original_type || 'memory';
            items.push({
              type,
              title: item.title || 'Untitled',
              created_at: createdAt,
            });
            
            if (type === 'decision') newDecisions++;
            else newMemory++;
          }
        }
      }
    } catch { /* optional */ }

    return {
      new_decisions: newDecisions,
      new_memory: newMemory,
      items,
    };
  }

  /**
   * Get smart context for a user query - CALL THIS BEFORE EVERY RESPONSE.
   *
   * This is the key tool for automatic context injection:
   * 1. Analyzes the user's message to understand what context is needed
   * 2. Retrieves relevant context in a minified, token-efficient format
   * 3. Returns context that the AI can use without including chat history
   *
   * The format is optimized for AI consumption:
   * - Compact notation (D: for Decision, P: for Preference, etc.)
   * - No redundant whitespace
   * - Structured for easy parsing
   *
   * Format options:
   * - 'minified': Ultra-compact TYPE:value|TYPE:value|...
   * - 'readable': Human-readable with line breaks
   * - 'structured': JSON-like grouped format
   */
  async getSmartContext(params: {
    user_message: string;
    workspace_id?: string;
    project_id?: string;
    max_tokens?: number;
    format?: 'minified' | 'readable' | 'structured';
  }): Promise<{
    context: string;
    token_estimate: number;
    format: string;
    sources_used: number;
    errors?: string[];
  }> {
    const withDefaults = this.withDefaults(params);
    const maxTokens = params.max_tokens || 800;
    const format = params.format || 'minified';
    
    if (!withDefaults.workspace_id) {
      return {
        context: '[NO_WORKSPACE]',
        token_estimate: 2,
        format,
        sources_used: 0,
      };
    }

    // Extract keywords from user message for targeted search
    const message = params.user_message.toLowerCase();
    const keywords = this.extractKeywords(message);
    
    // Collect context items
    const items: Array<{ type: string; key: string; value: string; relevance: number }> = [];
    const errors: string[] = [];

    // 1. Get workspace/project info (always include, very compact)
    try {
      const ws = await this.getWorkspace(withDefaults.workspace_id) as { name?: string };
      if (ws?.name) {
        items.push({ type: 'W', key: 'workspace', value: ws.name, relevance: 1 });
      } else {
        // Workspace exists but no name - still indicate we have context
        items.push({ type: 'W', key: 'workspace', value: withDefaults.workspace_id!.slice(0, 8), relevance: 1 });
      }
    } catch (e) {
      errors.push(`workspace: ${(e as Error)?.message || 'fetch failed'}`);
      // Still add workspace ID so we know context exists
      items.push({ type: 'W', key: 'workspace', value: `id:${withDefaults.workspace_id!.slice(0, 8)}`, relevance: 0.5 });
    }

    if (withDefaults.project_id) {
      try {
        const proj = await this.getProject(withDefaults.project_id) as { name?: string };
        if (proj?.name) {
          items.push({ type: 'P', key: 'project', value: proj.name, relevance: 1 });
        }
      } catch (e) {
        errors.push(`project: ${(e as Error)?.message || 'fetch failed'}`);
      }
    }

    // 2. Get decisions (prioritize based on keyword match)
    try {
      const decisions = await this.memoryDecisions({
        workspace_id: withDefaults.workspace_id,
        project_id: withDefaults.project_id,
        limit: 10,
      }) as { items?: Array<{ title?: string; content?: string }> };

      if (decisions.items) {
        for (const d of decisions.items) {
          const title = d.title || '';
          const content = d.content || '';
          const relevance = this.calculateRelevance(keywords, title + ' ' + content);
          items.push({
            type: 'D',
            key: 'decision',
            value: title.slice(0, 80),
            relevance,
          });
        }
      }
    } catch (e) {
      errors.push(`decisions: ${(e as Error)?.message || 'fetch failed'}`);
    }
    
    // 3. Search memory for query-relevant items
    if (keywords.length > 0) {
      try {
        const memory = await this.memorySearch({
          query: params.user_message.slice(0, 200),
          workspace_id: withDefaults.workspace_id,
          project_id: withDefaults.project_id,
          limit: 5,
        }) as { results?: Array<{ title?: string; content?: string }> };

        if (memory.results) {
          for (const m of memory.results) {
            const title = m.title || '';
            const content = m.content || '';
            items.push({
              type: 'M',
              key: 'memory',
              value: title.slice(0, 80) + (content ? ': ' + content.slice(0, 100) : ''),
              relevance: 0.8, // Memory search already ranked by relevance
            });
          }
        }
      } catch (e) {
        errors.push(`memory: ${(e as Error)?.message || 'search failed'}`);
      }
    }

    // 4. Get relevant lessons (high priority - surface warnings)
    try {
      const lessons = await this.getHighPriorityLessons({
        workspace_id: withDefaults.workspace_id,
        project_id: withDefaults.project_id,
        context_hint: params.user_message,
        limit: 3,
      });

      for (const lesson of lessons) {
        // Use L for Lesson type, add warning emoji for critical
        const prefix = lesson.severity === 'critical' ? '⚠️ ' : '';
        items.push({
          type: 'L',
          key: 'lesson',
          value: `${prefix}${lesson.title}: ${lesson.prevention.slice(0, 100)}`,
          relevance: lesson.severity === 'critical' ? 1.0 : 0.9, // Lessons are high priority
        });
      }
    } catch (e) {
      errors.push(`lessons: ${(e as Error)?.message || 'fetch failed'}`);
    }

    // Log errors for debugging if any occurred
    if (errors.length > 0) {
      console.error('[ContextStream] context_smart errors:', errors.join(', '));
    }

    // Sort by relevance
    items.sort((a, b) => b.relevance - a.relevance);
    
    // Build context string based on format
    let context: string;
    let charsUsed = 0;
    const maxChars = maxTokens * 4; // ~4 chars per token
    
    if (format === 'minified') {
      // Ultra-compact format: TYPE:value|TYPE:value|...
      const parts: string[] = [];
      for (const item of items) {
        const entry = `${item.type}:${item.value}`;
        if (charsUsed + entry.length + 1 > maxChars) break;
        parts.push(entry);
        charsUsed += entry.length + 1;
      }
      context = parts.join('|');
    } else if (format === 'structured') {
      // JSON-like compact format
      const grouped: Record<string, string[]> = {};
      for (const item of items) {
        if (charsUsed > maxChars) break;
        if (!grouped[item.type]) grouped[item.type] = [];
        grouped[item.type].push(item.value);
        charsUsed += item.value.length + 5;
      }
      context = JSON.stringify(grouped);
    } else {
      // Readable format (default)
      const lines: string[] = ['[CTX]'];
      for (const item of items) {
        const line = `${item.type}:${item.value}`;
        if (charsUsed + line.length + 1 > maxChars) break;
        lines.push(line);
        charsUsed += line.length + 1;
      }
      lines.push('[/CTX]');
      context = lines.join('\n');
    }
    
    // If context is empty but we have workspace, add a hint
    if (context.length === 0 && withDefaults.workspace_id) {
      const wsHint = items.find(i => i.type === 'W')?.value || withDefaults.workspace_id.slice(0, 8);
      context = format === 'minified'
        ? `W:${wsHint}|[NO_MATCHES]`
        : `[CTX]\nW:${wsHint}\n[NO_MATCHES]\n[/CTX]`;
    }

    return {
      context,
      token_estimate: Math.ceil(context.length / 4),
      format,
      sources_used: items.filter(i => context.includes(i.value.slice(0, 20))).length,
      ...(errors.length > 0 && { errors }), // Include errors for debugging
    };
  }

  /**
   * Get high-priority lessons that should be surfaced proactively.
   * Returns critical and high severity lessons for warnings.
   */
  async getHighPriorityLessons(params: {
    workspace_id: string;
    project_id?: string;
    context_hint?: string;
    limit?: number;
  }): Promise<Array<{
    title: string;
    severity: string;
    category: string;
    prevention: string;
  }>> {
    const limit = params.limit || 5;

    try {
      // Search for lessons, prioritizing those relevant to the context
      const searchQuery = params.context_hint
        ? `${params.context_hint} lesson warning prevention mistake`
        : 'lesson warning prevention mistake critical high';

      const searchResult = await this.memorySearch({
        query: searchQuery,
        workspace_id: params.workspace_id,
        project_id: params.project_id,
        limit: limit * 2, // Fetch more to filter
      }) as { results?: Array<{
        title?: string;
        content?: string;
        metadata?: { tags?: string[]; importance?: string };
      }> };

      if (!searchResult?.results) return [];

      // Filter for lessons with high/critical severity
      const lessons = searchResult.results
        .filter((item) => {
          const tags = item.metadata?.tags || [];
          const isLesson = tags.includes('lesson') || tags.includes('lesson_system');
          if (!isLesson) return false;

          // Get severity from tags or importance
          const severityTag = tags.find((t: string) => t.startsWith('severity:'));
          const severity = severityTag?.split(':')[1] || item.metadata?.importance || 'medium';
          return severity === 'critical' || severity === 'high';
        })
        .slice(0, limit)
        .map((item) => {
          const tags = item.metadata?.tags || [];
          const severityTag = tags.find((t: string) => t.startsWith('severity:'));
          const severity = severityTag?.split(':')[1] || item.metadata?.importance || 'medium';
          const category = tags.find((t: string) =>
            ['workflow', 'code_quality', 'verification', 'communication', 'project_specific'].includes(t)
          ) || 'unknown';

          // Extract prevention from content
          const content = item.content || '';
          const preventionMatch = content.match(/### Prevention\n([\s\S]*?)(?:\n\n|\n\*\*|$)/);
          const prevention = preventionMatch?.[1]?.trim() || content.slice(0, 200);

          return {
            title: item.title || 'Lesson',
            severity,
            category,
            prevention,
          };
        });

      return lessons;
    } catch {
      return [];
    }
  }

  /**
   * Extract keywords from a message for relevance matching
   */
  private extractKeywords(message: string): string[] {
    // Remove common words and extract meaningful terms
    const stopWords = new Set([
      'the', 'a', 'an', 'is', 'are', 'was', 'were', 'be', 'been', 'being',
      'have', 'has', 'had', 'do', 'does', 'did', 'will', 'would', 'could',
      'should', 'may', 'might', 'must', 'can', 'to', 'of', 'in', 'for',
      'on', 'with', 'at', 'by', 'from', 'as', 'into', 'through', 'during',
      'before', 'after', 'above', 'below', 'between', 'under', 'again',
      'further', 'then', 'once', 'here', 'there', 'when', 'where', 'why',
      'how', 'all', 'each', 'few', 'more', 'most', 'other', 'some', 'such',
      'no', 'nor', 'not', 'only', 'own', 'same', 'so', 'than', 'too', 'very',
      'just', 'and', 'but', 'if', 'or', 'because', 'until', 'while', 'this',
      'that', 'these', 'those', 'what', 'which', 'who', 'whom', 'i', 'me',
      'my', 'we', 'our', 'you', 'your', 'he', 'she', 'it', 'they', 'them',
    ]);
    
    return message
      .toLowerCase()
      .replace(/[^\w\s]/g, ' ')
      .split(/\s+/)
      .filter(word => word.length > 2 && !stopWords.has(word));
  }

  /**
   * Calculate relevance score based on keyword matches
   */
  private calculateRelevance(keywords: string[], text: string): number {
    if (keywords.length === 0) return 0.5;
    
    const textLower = text.toLowerCase();
    let matches = 0;
    for (const keyword of keywords) {
      if (textLower.includes(keyword)) {
        matches++;
      }
    }
    
    return matches / keywords.length;
  }
}
