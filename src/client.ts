import { randomUUID } from 'node:crypto';
import * as path from 'node:path';
import { z } from 'zod';
import type { Config } from './config.js';
import { request, HttpError } from './http.js';
import { readFilesFromDirectory, readAllFilesInBatches } from './files.js';
import {
  resolveWorkspace,
  readLocalConfig,
  writeLocalConfig,
  addGlobalMapping,
  type WorkspaceConfig
} from './workspace-config.js';
import { globalCache, CacheKeys, CacheTTL } from './cache.js';
import { VERSION, getUpdateNotice, type VersionNotice } from './version.js';

const uuidSchema = z.string().uuid();

function unwrapApiResponse<T>(result: unknown): T {
  if (!result || typeof result !== 'object') return result as T;
  const maybe = result as any;
  if (typeof maybe.success === 'boolean' && 'data' in maybe) {
    return maybe.data as T;
  }
  return result as T;
}

function normalizeNodeType(input: string): string {
  const t = String(input ?? '').trim().toLowerCase();
  switch (t) {
    case 'fact':
    case 'insight':
    case 'note':
      return 'Fact';
    case 'decision':
      return 'Decision';
    case 'preference':
      return 'Preference';
    case 'constraint':
      return 'Constraint';
    case 'habit':
      return 'Habit';
    case 'lesson':
      return 'Lesson';
    default:
      throw new Error(
        `Invalid node_type: ${JSON.stringify(input)} (expected one of fact|decision|preference|constraint|habit|lesson)`
      );
  }
}

type GraphTier = 'none' | 'lite' | 'full';

function pickString(value: unknown): string | null {
  if (typeof value !== 'string') return null;
  const trimmed = value.trim();
  return trimmed ? trimmed : null;
}

function normalizeGraphTier(value: string): GraphTier | null {
  const normalized = value.trim().toLowerCase();
  if (!normalized) return null;
  if (normalized.includes('full') || normalized.includes('elite') || normalized.includes('team')) return 'full';
  if (normalized.includes('lite') || normalized.includes('light') || normalized.includes('basic') || normalized.includes('module')) return 'lite';
  if (normalized.includes('none') || normalized.includes('off') || normalized.includes('disabled') || normalized.includes('free')) return 'none';
  return null;
}

const AI_PLAN_TIMEOUT_MS = 50_000;
const AI_PLAN_RETRIES = 0;

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
    const providedWorkspaceId = this.coerceUuid(input.workspace_id);
    const workspaceId = providedWorkspaceId || defaultWorkspaceId;

    // Only use defaultProjectId if:
    // 1. project_id is not explicitly provided, AND
    // 2. workspace_id matches defaultWorkspaceId (or both are undefined)
    // This prevents using a cached project_id that belongs to a different workspace
    const providedProjectId = this.coerceUuid(input.project_id);
    const useDefaultProject = !providedProjectId &&
      (!providedWorkspaceId || providedWorkspaceId === defaultWorkspaceId);

    return {
      ...input,
      workspace_id: workspaceId,
      project_id: providedProjectId || (useDefaultProject ? defaultProjectId : undefined),
    } as T;
  }

  private coerceUuid(value?: string): string | undefined {
    if (!value) return undefined;
    try {
      uuidSchema.parse(value);
      return value;
    } catch {
      return undefined;
    }
  }

  private requireNonEmpty(value: unknown, field: string, tool: string): string {
    const text = String(value ?? '').trim();
    if (!text) {
      throw new HttpError(400, `${field} is required for ${tool}`);
    }
    return text;
  }

  private isBadRequestDeserialization(error: unknown): boolean {
    if (!(error instanceof HttpError)) return false;
    if (String(error.code || '').toUpperCase() !== 'BAD_REQUEST') return false;
    const message = String(error.message || '').toLowerCase();
    return message.includes('deserialize') || message.includes('deserial');
  }

  // Auth
  me() {
    return request(this.config, '/auth/me');
  }

  startDeviceLogin() {
    return request(this.config, '/auth/device/start', { method: 'POST' });
  }

  pollDeviceLogin(input: { device_code: string }) {
    return request(this.config, '/auth/device/token', { body: input });
  }

  createApiKey(input: { name: string; permissions?: string[]; expires_at?: string | null }) {
    return request(this.config, '/auth/api-keys', { body: input });
  }

  // Credits / Billing (used for plan gating)
  async getCreditBalance(): Promise<any> {
    const cacheKey = CacheKeys.creditBalance();
    const cached = globalCache.get(cacheKey);
    if (cached) return cached;

    const result = await request(this.config, '/credits/balance', { method: 'GET' }) as any;
    const data = result && typeof result === 'object' && 'data' in result && (result as any).data ? (result as any).data : result;

    globalCache.set(cacheKey, data, CacheTTL.CREDIT_BALANCE);
    return data;
  }

  async getPlanName(): Promise<string | null> {
    try {
      const balance = await this.getCreditBalance();
      const planName = balance?.plan?.name;
      return typeof planName === 'string' ? planName.toLowerCase() : null;
    } catch {
      return null;
    }
  }

  async getGraphTier(): Promise<GraphTier> {
    try {
      const balance = await this.getCreditBalance();
      const plan = balance?.plan ?? {};
      const features = plan?.features ?? {};

      const tierCandidate =
        pickString(plan.graph_tier) ||
        pickString(plan.graphTier) ||
        pickString(features.graph_tier) ||
        pickString(features.graphTier) ||
        pickString(balance?.graph_tier) ||
        pickString(balance?.graphTier);

      const normalizedTier = tierCandidate ? normalizeGraphTier(tierCandidate) : null;
      if (normalizedTier) return normalizedTier;

      const planName = pickString(plan.name)?.toLowerCase() ?? null;
      if (!planName) return 'none';

      if (
        planName.includes('elite') ||
        planName.includes('team') ||
        planName.includes('enterprise') ||
        planName.includes('business')
      ) {
        return 'full';
      }
      if (planName.includes('pro')) return 'lite';
      if (planName.includes('free')) return 'none';

      return 'lite';
    } catch {
      return 'none';
    }
  }

  // Workspaces & Projects
  listWorkspaces(params?: { page?: number; page_size?: number }) {
    const query = new URLSearchParams();
    if (params?.page) query.set('page', String(params.page));
    if (params?.page_size) query.set('page_size', String(params.page_size));
    const suffix = query.toString() ? `?${query.toString()}` : '';
    return request(this.config, `/workspaces${suffix}`);
  }

  async createWorkspace(
    input: { name: string; description?: string; visibility?: string },
    options?: { unwrap?: boolean }
  ) {
    const result = await request(this.config, '/workspaces', { body: input });
    return options?.unwrap === false ? result : unwrapApiResponse(result);
  }

  async updateWorkspace(workspaceId: string, input: { name?: string; description?: string; visibility?: string }) {
    uuidSchema.parse(workspaceId);
    const result = await request(this.config, `/workspaces/${workspaceId}`, { method: 'PUT', body: input });
    // Invalidate caches so subsequent reads reflect updates.
    globalCache.delete(CacheKeys.workspace(workspaceId));
    globalCache.delete(`workspace_overview:${workspaceId}`);
    return result;
  }

  async deleteWorkspace(workspaceId: string) {
    uuidSchema.parse(workspaceId);
    const result = await request(this.config, `/workspaces/${workspaceId}`, { method: 'DELETE' });
    globalCache.delete(CacheKeys.workspace(workspaceId));
    globalCache.delete(`workspace_overview:${workspaceId}`);
    return result;
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

  async createProject(
    input: { name: string; description?: string; workspace_id?: string },
    options?: { unwrap?: boolean }
  ) {
    const payload = this.withDefaults(input);
    const result = await request(this.config, '/projects', { body: payload });
    return options?.unwrap === false ? result : unwrapApiResponse(result);
  }

  async updateProject(projectId: string, input: { name?: string; description?: string }) {
    uuidSchema.parse(projectId);
    const result = await request(this.config, `/projects/${projectId}`, { method: 'PUT', body: input });
    globalCache.delete(CacheKeys.project(projectId));
    globalCache.delete(`project_overview:${projectId}`);
    return result;
  }

  async deleteProject(projectId: string) {
    uuidSchema.parse(projectId);
    const result = await request(this.config, `/projects/${projectId}`, { method: 'DELETE' });
    globalCache.delete(CacheKeys.project(projectId));
    globalCache.delete(`project_overview:${projectId}`);
    return result;
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
    provenance?: Record<string, unknown>;
    code_refs?: Array<{ file_path: string; symbol_id?: string; symbol_name?: string }>;
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
    const withDefaults = this.withDefaults(body);
    if (!withDefaults.workspace_id) {
      throw new Error('workspace_id is required for creating knowledge nodes');
    }

    const summary =
      String(withDefaults.title ?? '').trim() ||
      String(withDefaults.content ?? '').trim().slice(0, 120) ||
      'Untitled';
    const details = String(withDefaults.content ?? '').trim();

    // API expects CreateKnowledgeNodeRequest.
    const apiBody: Record<string, any> = {
      workspace_id: withDefaults.workspace_id,
      project_id: withDefaults.project_id,
      node_type: normalizeNodeType(withDefaults.node_type),
      summary,
      details: details || undefined,
      valid_from: new Date().toISOString(),
    };

    if (withDefaults.relations && withDefaults.relations.length) {
      // Preserve requested relations in node context (API does not currently accept relations on create).
      apiBody.context = { relations: withDefaults.relations };
    }

    return request(this.config, '/memory/nodes', { body: apiBody });
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

  memoryDecisions(params?: { workspace_id?: string; project_id?: string; category?: string; limit?: number }) {
    const query = new URLSearchParams();
    const withDefaults = this.withDefaults(params || {});
    if (withDefaults.workspace_id) query.set('workspace_id', withDefaults.workspace_id);
    if (withDefaults.project_id) query.set('project_id', withDefaults.project_id);
    if (params?.category) query.set('category', params.category);
    if (params?.limit) query.set('limit', String(params.limit));
    const suffix = query.toString() ? `?${query.toString()}` : '';
    return request(this.config, `/memory/search/decisions${suffix}`, { method: 'GET' });
  }

  // Graph
  graphRelated(body: {
    workspace_id?: string;
    project_id?: string;
    node_id: string;
    limit?: number;
    relation_types?: string[];
    max_depth?: number;
  }) {
    const withDefaults = this.withDefaults(body);
    const apiBody = {
      node_id: withDefaults.node_id,
      relation_types: body.relation_types,
      max_depth: body.max_depth ?? body.limit,
      workspace_id: withDefaults.workspace_id,
      project_id: withDefaults.project_id,
    };
    return request(this.config, '/graph/knowledge/related', { body: apiBody });
  }

  graphPath(body: {
    workspace_id?: string;
    project_id?: string;
    source_id?: string;
    target_id?: string;
    from?: string;
    to?: string;
    max_depth?: number;
  }) {
    const withDefaults = this.withDefaults(body);
    const from = body.from ?? withDefaults.source_id;
    const to = body.to ?? withDefaults.target_id;
    const apiBody = {
      from,
      to,
      max_depth: body.max_depth,
      workspace_id: withDefaults.workspace_id,
      project_id: withDefaults.project_id,
    };
    return request(this.config, '/graph/knowledge/path', { body: apiBody });
  }

  graphDecisions(body?: {
    workspace_id?: string;
    project_id?: string;
    limit?: number;
    category?: string;
    from?: string;
    to?: string;
  }) {
    const withDefaults = this.withDefaults(body || {});
    const now = new Date();
    const defaultFrom = new Date(now);
    defaultFrom.setUTCFullYear(now.getUTCFullYear() - 5);
    const apiBody = {
      workspace_id: withDefaults.workspace_id,
      project_id: withDefaults.project_id,
      category: body?.category ?? 'general',
      from: body?.from ?? defaultFrom.toISOString(),
      to: body?.to ?? now.toISOString(),
    };
    return request(this.config, '/graph/knowledge/decisions', { body: apiBody });
  }

  async graphDependencies(body: {
    target: { type: string; id: string; path?: string };
    max_depth?: number;
    include_transitive?: boolean;
  }) {
    const rawType = String(body.target?.type ?? '').toLowerCase();
    let targetType: 'module' | 'function' | 'type' | 'variable' = 'function';
    switch (rawType) {
      case 'module':
      case 'file':
      case 'path':
        targetType = 'module';
        break;
      case 'type':
        targetType = 'type';
        break;
      case 'variable':
      case 'var':
      case 'const':
        targetType = 'variable';
        break;
      case 'function':
      case 'method':
        targetType = 'function';
        break;
      default:
        targetType = 'function';
    }

    const target =
      targetType === 'module'
        ? { type: targetType, path: body.target.path ?? body.target.id }
        : { type: targetType, id: body.target.id };

    return request(this.config, '/graph/dependencies', {
      body: {
        target,
        max_depth: body.max_depth,
        include_transitive: body.include_transitive,
      },
    });
  }

  graphCallPath(body: {
    source: { type: string; id: string };
    target: { type: string; id: string };
    max_depth?: number;
    from_function_id?: string;
    to_function_id?: string;
  }) {
    const apiBody = {
      from_function_id: body.from_function_id ?? body.source?.id,
      to_function_id: body.to_function_id ?? body.target?.id,
      max_depth: body.max_depth,
    };
    return request(this.config, '/graph/call-paths', { body: apiBody });
  }

  graphImpact(body: {
    target: { type: string; id: string };
    max_depth?: number;
    change_type?: string;
    target_id?: string;
    element_name?: string;
  }) {
    const targetId = body.target_id ?? body.target?.id;
    const elementName = body.element_name ?? body.target?.id ?? body.target?.type ?? 'unknown';
    const apiBody = {
      change_type: body.change_type ?? 'modify_signature',
      target_id: targetId,
      element_name: elementName,
    };
    return request(this.config, '/graph/impact-analysis', { body: apiBody });
  }

  graphIngest(body: { project_id?: string; wait?: boolean }) {
    const withDefaults = this.withDefaults(body);
    const projectId = withDefaults.project_id;
    if (!projectId) {
      throw new Error('project_id is required to ingest the graph.');
    }
    uuidSchema.parse(projectId);

    const apiBody: Record<string, unknown> = {};
    if (body.wait !== undefined) {
      apiBody.wait = body.wait;
    }
    return request(this.config, `/graph/ingest/${projectId}`, { body: apiBody });
  }

  // AI
  private buildAiContextRequest(input: {
    query: string;
    project_id?: string;
    max_tokens?: number;
    max_sections?: number;
    token_budget?: number;
    token_soft_limit?: number;
    include_dependencies?: boolean;
    include_tests?: boolean;
    limit?: number;
  }) {
    const payload: Record<string, unknown> = {
      query: input.query,
    };

    if (input.project_id) payload.project_id = input.project_id;
    if (typeof input.max_tokens === 'number') payload.max_tokens = input.max_tokens;
    if (typeof input.token_budget === 'number') payload.token_budget = input.token_budget;
    if (typeof input.token_soft_limit === 'number') payload.token_soft_limit = input.token_soft_limit;
    if (typeof input.include_dependencies === 'boolean') {
      payload.include_dependencies = input.include_dependencies;
    }
    if (typeof input.include_tests === 'boolean') {
      payload.include_tests = input.include_tests;
    }

    const rawLimit =
      typeof input.max_sections === 'number' ? input.max_sections : input.limit;
    if (typeof rawLimit === 'number' && Number.isFinite(rawLimit)) {
      const bounded = Math.max(1, Math.min(20, Math.floor(rawLimit)));
      payload.max_sections = bounded;
    }

    return payload;
  }

  private buildAiPlanRequest(input: {
    description?: string;
    requirements?: string;
    project_id?: string;
    max_steps?: number;
    context?: string;
    constraints?: string[];
  }) {
    const requirements = input.requirements ?? input.description;
    if (!requirements || !requirements.trim()) {
      throw new Error('description is required for ai_plan');
    }

    const payload: Record<string, unknown> = {
      requirements,
    };

    if (input.project_id) payload.project_id = input.project_id;
    if (typeof input.max_steps === 'number') payload.max_steps = input.max_steps;
    if (input.context) payload.context = input.context;
    if (input.constraints) payload.constraints = input.constraints;

    return payload;
  }

  private normalizeTaskGranularity(value?: string): 'low' | 'medium' | 'high' | undefined {
    if (!value) return undefined;
    const normalized = value.trim().toLowerCase();
    if (normalized === 'low' || normalized === 'medium' || normalized === 'high') {
      return normalized;
    }
    if (normalized === 'coarse' || normalized === 'broad') return 'low';
    if (normalized === 'fine' || normalized === 'detailed') return 'high';
    return undefined;
  }

  private buildAiTasksRequest(input: {
    description?: string;
    plan?: string;
    project_id?: string;
    granularity?: string;
    max_tasks?: number;
    include_estimates?: boolean;
  }) {
    const plan = input.plan ?? input.description;
    if (!plan || !plan.trim()) {
      throw new Error('description is required for ai_tasks');
    }

    const payload: Record<string, unknown> = {
      plan,
    };

    if (input.project_id) payload.project_id = input.project_id;

    const granularity = this.normalizeTaskGranularity(input.granularity);
    if (granularity) payload.granularity = granularity;

    if (typeof input.max_tasks === 'number') payload.max_tasks = input.max_tasks;
    if (typeof input.include_estimates === 'boolean') {
      payload.include_estimates = input.include_estimates;
    }

    return payload;
  }

  aiContext(body: {
    query: string;
    workspace_id?: string;
    project_id?: string;
    include_code?: boolean;
    include_docs?: boolean;
    include_memory?: boolean;
    limit?: number;
  }) {
    const { query, project_id, limit, workspace_id } = this.withDefaults(body);
    const safeQuery = this.requireNonEmpty(query, 'query', 'ai_context');
    const safeProjectId = this.coerceUuid(project_id);
    const payload = this.buildAiContextRequest({ query: safeQuery, project_id: safeProjectId, limit });
    return request(this.config, '/ai/context', { body: payload, workspaceId: workspace_id })
      .catch((error) => {
        if (this.isBadRequestDeserialization(error)) {
          const minimalPayload = this.buildAiContextRequest({ query: safeQuery });
          return request(this.config, '/ai/context', { body: minimalPayload, workspaceId: workspace_id });
        }
        throw error;
      });
  }

  aiEmbeddings(body: { text: string }) {
    return request(this.config, '/ai/embeddings', { body });
  }

  aiPlan(body: { description: string; project_id?: string; complexity?: string }) {
    const { description, project_id } = this.withDefaults(body);
    const safeDescription = this.requireNonEmpty(description, 'description', 'ai_plan');
    const safeProjectId = this.coerceUuid(project_id);
    const payload = this.buildAiPlanRequest({ description: safeDescription, project_id: safeProjectId });
    const requestOptions = { body: payload, timeoutMs: AI_PLAN_TIMEOUT_MS, retries: AI_PLAN_RETRIES };
    return request(this.config, '/ai/plan/generate', requestOptions)
      .catch((error) => {
        if (this.isBadRequestDeserialization(error)) {
          const minimalPayload = this.buildAiPlanRequest({ description: safeDescription });
          return request(this.config, '/ai/plan/generate', {
            body: minimalPayload,
            timeoutMs: AI_PLAN_TIMEOUT_MS,
            retries: AI_PLAN_RETRIES,
          });
        }
        if (error instanceof HttpError && error.status === 0 && /timeout/i.test(error.message)) {
          const seconds = Math.ceil(AI_PLAN_TIMEOUT_MS / 1000);
          throw new HttpError(
            503,
            `AI plan generation timed out after ${seconds} seconds. Try a shorter description, reduce max_steps, or retry later.`
          );
        }
        throw error;
      });
  }

  aiTasks(body: { plan_id?: string; description?: string; project_id?: string; granularity?: string }) {
    if (!body.description && body.plan_id) {
      throw new Error('plan_id is not supported for ai_tasks; provide description instead.');
    }
    const { description, project_id, granularity } = this.withDefaults(body);
    const safeDescription = this.requireNonEmpty(description, 'description', 'ai_tasks');
    const safeProjectId = this.coerceUuid(project_id);
    const payload = this.buildAiTasksRequest({ description: safeDescription, project_id: safeProjectId, granularity });
    return request(this.config, '/ai/tasks/generate', { body: payload })
      .catch((error) => {
        if (this.isBadRequestDeserialization(error)) {
          const minimalPayload = this.buildAiTasksRequest({ description: safeDescription });
          return request(this.config, '/ai/tasks/generate', { body: minimalPayload });
        }
        throw error;
      });
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
    const { query, project_id, limit, workspace_id } = this.withDefaults(body);
    const safeQuery = this.requireNonEmpty(query, 'query', 'ai_enhanced_context');
    const safeProjectId = this.coerceUuid(project_id);
    const payload = this.buildAiContextRequest({ query: safeQuery, project_id: safeProjectId, limit });
    return request(this.config, '/ai/context/enhanced', { body: payload, workspaceId: workspace_id })
      .catch((error) => {
        if (this.isBadRequestDeserialization(error)) {
          const minimalPayload = this.buildAiContextRequest({ query: safeQuery });
          return request(this.config, '/ai/context/enhanced', { body: minimalPayload, workspaceId: workspace_id });
        }
        throw error;
      });
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
   * @param projectId - Project UUID
   * @param files - Array of files to ingest
   * @param options - Optional ingest options
   * @param options.write_to_disk - When true, write files to disk under QA_FILE_WRITE_ROOT before indexing
   * @param options.overwrite - Allow overwriting existing files when write_to_disk is enabled
   */
  ingestFiles(
    projectId: string,
    files: Array<{ path: string; content: string; language?: string }>,
    options?: { write_to_disk?: boolean; overwrite?: boolean }
  ) {
    uuidSchema.parse(projectId);
    return request(this.config, `/projects/${projectId}/files/ingest`, {
      body: {
        files,
        ...(options?.write_to_disk !== undefined && { write_to_disk: options.write_to_disk }),
        ...(options?.overwrite !== undefined && { overwrite: options.overwrite }),
      },
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
    return request(this.config, `/memory/events/${eventId}`, { method: 'DELETE' })
      .then((r) => (r === '' || r == null ? { success: true } : r));
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
    const apiBody: Record<string, any> = {};
    if (body.title !== undefined) apiBody.summary = body.title;
    if (body.content !== undefined) apiBody.details = body.content;
    if (body.relations && body.relations.length) apiBody.context = { relations: body.relations };
    return request(this.config, `/memory/nodes/${nodeId}`, { method: 'PUT', body: apiBody });
  }

  deleteKnowledgeNode(nodeId: string) {
    uuidSchema.parse(nodeId);
    return request(this.config, `/memory/nodes/${nodeId}`, { method: 'DELETE' })
      .then((r) => (r === '' || r == null ? { success: true } : r));
  }

  supersedeKnowledgeNode(nodeId: string, body: { new_content: string; reason?: string }) {
    uuidSchema.parse(nodeId);
    return (async () => {
      const existingResp = await this.getKnowledgeNode(nodeId) as any;
      const existing = unwrapApiResponse<any>(existingResp);
      if (!existing || !existing.workspace_id) {
        throw new Error('Failed to load existing node before superseding');
      }

      const createdResp = await this.createKnowledgeNode({
        workspace_id: existing.workspace_id,
        project_id: existing.project_id ?? undefined,
        node_type: existing.node_type,
        title: existing.summary ?? 'Superseded node',
        content: body.new_content,
      }) as any;
      const created = unwrapApiResponse<any>(createdResp);
      if (!created?.id) {
        throw new Error('Failed to create replacement node for supersede');
      }

      await request(this.config, `/memory/nodes/${nodeId}/supersede`, { body: { superseded_by: created.id } });

      return {
        success: true,
        data: {
          status: 'superseded',
          old_node_id: nodeId,
          new_node_id: created.id,
          reason: body.reason ?? null,
        },
        error: null,
      };
    })();
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
  async findCircularDependencies(projectId: string) {
    uuidSchema.parse(projectId);
    return request(this.config, `/graph/circular-dependencies/${projectId}`, { method: 'GET' });
  }

  async findUnusedCode(projectId: string) {
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
    /**
     * If true, session_init will return `status: "connected"` even when no workspace
     * can be resolved. Workspace-level tools (memory/search/graph) may not work without
     * a workspace, so default behavior is to prompt the user to select/create one.
     */
    allow_no_workspace?: boolean;
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
        const folderName = rootPath ? path.basename(rootPath).toLowerCase() : '';
        
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
              context.message = `New folder detected: "${rootPath ? path.basename(rootPath) : 'this folder'}". Please select which workspace this belongs to, or create a new one.`;
              context.ide_roots = ideRoots;
              context.folder_name = rootPath ? path.basename(rootPath) : undefined;
              
              // Return early - agent needs to ask user
              return context;
            }
          } else {
            // No workspaces exist yet. Ask the user for a name rather than
            // auto-creating a workspace from the folder name.
            const folderDisplayName = rootPath ? (path.basename(rootPath) || 'this folder') : 'this folder';

            context.status = 'requires_workspace_name';
            context.workspace_source = 'none_found';
            context.ide_roots = ideRoots;
            context.folder_name = folderDisplayName;
            context.folder_path = rootPath;
            context.suggested_project_name = folderDisplayName;
            context.message =
              `No workspaces found for this account. Ask the user for a name for a new workspace, then create a project for "${folderDisplayName}".`;

            // Return early - agent needs user input (workspace name)
            return context;
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

    // If we still couldn't resolve a workspace, do not silently continue.
    // Ask the user to select/create a workspace unless explicitly allowed.
    if (!workspaceId && !params.allow_no_workspace) {
      const folderDisplayName = rootPath ? (path.basename(rootPath) || 'this folder') : 'this folder';

      context.ide_roots = ideRoots;
      context.folder_name = folderDisplayName;
      if (rootPath) {
        context.folder_path = rootPath;
      }
      context.suggested_project_name = folderDisplayName;

      try {
        const workspaces = await this.listWorkspaces({ page_size: 50 }) as {
          items?: Array<{ id: string; name: string; description?: string }>;
        };

        const items = Array.isArray(workspaces.items) ? workspaces.items : [];
        if (items.length > 0) {
          context.status = 'requires_workspace_selection';
          context.workspace_candidates = items.map((w) => ({
            id: w.id,
            name: w.name,
            description: w.description,
          }));
          context.message =
            `This folder is not associated with a workspace yet. Please select which workspace to use, or create a new one.`;
          return context;
        }

        context.status = 'requires_workspace_name';
        context.workspace_source = 'none_found';
        context.message =
          `No workspaces found for this account. Ask the user for a name for a new workspace, then create a project for "${folderDisplayName}".`;
        return context;
      } catch (e) {
        context.status = 'requires_workspace_selection';
        context.workspace_error = String(e);
        context.message =
          `Unable to resolve a workspace automatically (${String(e)}). Please provide workspace_id, or create one with workspace_bootstrap.`;
        return context;
      }
    }

    // ========================================
    // STEP 2: Project Discovery
    // ========================================
    if (!projectId && workspaceId && rootPath && params.auto_index !== false) {
      const projectName = path.basename(rootPath) || 'My Project';
      
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

    if (!workspaceId) {
      context.workspace_warning =
        'No workspace was resolved for this session. Workspace-level tools (memory/search/graph) may not work until you associate this folder with a workspace.';
    }

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
          // CRITICAL: If API returned a different workspace (e.g., access denied on original),
          // update workspace_id to use the one we actually have access to.
          // This prevents FORBIDDEN errors on subsequent calls.
          if (batchedContext.workspace.id && batchedContext.workspace.id !== workspaceId) {
            console.error(`[ContextStream] Workspace mismatch: config=${workspaceId}, API returned=${batchedContext.workspace.id}. Using API workspace.`);
            const oldWorkspaceId = workspaceId;
            workspaceId = batchedContext.workspace.id;
            workspaceName = batchedContext.workspace.name;
            context.workspace_id = workspaceId;
            context.workspace_name = workspaceName;
            context.workspace_source = 'api_fallback';
            context.workspace_mismatch_warning = `Config had workspace ${oldWorkspaceId} but you don't have access. Using ${workspaceId} instead.`;

            // Clear project_id since it likely belongs to the old workspace
            // The API returned project (if any) will be used instead
            projectId = batchedContext.project?.id;
            context.project_id = projectId;
            this.config.defaultProjectId = projectId;

            // Update local config to prevent this from happening again
            if (rootPath) {
              writeLocalConfig(rootPath, {
                workspace_id: workspaceId,
                workspace_name: workspaceName,
                project_id: projectId, // Use API-returned project or undefined
                associated_at: new Date().toISOString(),
              });
              console.error(`[ContextStream] Updated local config with accessible workspace: ${workspaceId}`);
            }
          }
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
        client_version: VERSION,
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
   * Uses Promise.allSettled for parallel execution (faster than sequential).
   */
  private async _fetchSessionContextFallback(
    context: Record<string, unknown>,
    workspaceId: string,
    projectId: string | undefined,
    params: { include_recent_memory?: boolean; include_decisions?: boolean; context_hint?: string }
  ): Promise<void> {
    // Build array of parallel requests
    const requests = [
      // 0: workspace overview
      this.workspaceOverview(workspaceId).catch(() => null),
      // 1: project overview (if projectId exists)
      projectId ? this.projectOverview(projectId).catch(() => null) : Promise.resolve(null),
      // 2: recent memory events
      params.include_recent_memory !== false
        ? this.listMemoryEvents({ workspace_id: workspaceId, limit: 10 }).catch(() => null)
        : Promise.resolve(null),
      // 3: recent decisions
      params.include_decisions !== false
        ? this.memoryDecisions({ workspace_id: workspaceId, limit: 5 }).catch(() => null)
        : Promise.resolve(null),
      // 4: relevant context from semantic search
      params.context_hint
        ? this.memorySearch({ query: params.context_hint, workspace_id: workspaceId, limit: 5 }).catch(() => null)
        : Promise.resolve(null),
      // 5: high-priority lessons
      this.getHighPriorityLessons({
        workspace_id: workspaceId,
        project_id: projectId,
        context_hint: params.context_hint,
        limit: 5,
      }).catch(() => null),
    ];

    // Execute all requests in parallel
    const results = await Promise.all(requests);

    // Assign results to context (null values are ignored)
    if (results[0]) context.workspace = results[0];
    if (results[1]) context.project = results[1];
    if (results[2]) context.recent_memory = results[2];
    if (results[3]) context.recent_decisions = results[3];
    if (results[4]) context.relevant_context = results[4];

    // Handle lessons with warning message
    const lessons = results[5] as Array<unknown> | null;
    if (lessons && Array.isArray(lessons) && lessons.length > 0) {
      context.lessons = lessons;
      context.lessons_warning = `⚠️ ${lessons.length} lesson(s) from past mistakes. Review before making changes.`;
    }
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
      const parentDir = path.dirname(folder_path);
      addGlobalMapping({
        pattern: path.join(parentDir, '*'),
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
    provenance?: Record<string, unknown>;
    code_refs?: Array<{ file_path: string; symbol_id?: string; symbol_name?: string }>;
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
      provenance: params.provenance,
      code_refs: params.code_refs,
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

  submitContextFeedback(body: {
    workspace_id?: string;
    project_id?: string;
    item_id: string;
    item_type: 'memory_event' | 'knowledge_node' | 'code_chunk';
    feedback_type: 'relevant' | 'irrelevant' | 'pin';
    query_text?: string;
    metadata?: Record<string, unknown>;
  }) {
    return request(this.config, '/context/smart/feedback', { body: this.withDefaults(body) });
  }

  decisionTrace(body: {
    query: string;
    workspace_id?: string;
    project_id?: string;
    limit?: number;
    include_impact?: boolean;
  }) {
    return request(this.config, '/memory/search/decisions/trace', {
      body: this.withDefaults(body),
    });
  }

  /**
   * Remember something using the session/remember endpoint.
   * This is a simpler interface than captureContext and supports await_indexing.
   */
  async sessionRemember(params: {
    content: string;
    workspace_id?: string;
    project_id?: string;
    importance?: 'low' | 'medium' | 'high';
    await_indexing?: boolean;
  }) {
    const withDefaults = this.withDefaults(params);

    if (!withDefaults.workspace_id) {
      throw new Error('workspace_id is required for session_remember. Set defaultWorkspaceId in config or provide workspace_id.');
    }

    return request(this.config, '/session/remember', {
      body: {
        content: params.content,
        workspace_id: withDefaults.workspace_id,
        project_id: withDefaults.project_id,
        importance: params.importance,
        await_indexing: params.await_indexing,
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
   * Record a token savings event for user-facing dashboard analytics.
   * Best-effort: callers should not await this in latency-sensitive paths.
   */
  trackTokenSavings(body: {
    tool: string;
    source?: string;
    workspace_id?: string;
    project_id?: string;
    candidate_chars: number;
    context_chars: number;
    max_tokens?: number;
    metadata?: any;
  }) {
    const payload = this.withDefaults({
      source: 'mcp',
      ...body,
    });
    return request(this.config, '/analytics/token-savings', { body: payload });
  }

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
      const wsResponse = await this.getWorkspace(withDefaults.workspace_id);
      const ws = unwrapApiResponse<{ name?: string }>(wsResponse);
      workspaceName = pickString(ws?.name) ?? undefined;
      if (workspaceName) {
        parts.push(`📁 Workspace: ${workspaceName}`);
      }
    } catch { /* optional */ }

    // Get project info if specified (cached)
    if (withDefaults.project_id) {
      try {
        const projResponse = await this.getProject(withDefaults.project_id);
        const proj = unwrapApiResponse<{ name?: string }>(projResponse);
        projectName = pickString(proj?.name) ?? undefined;
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

    const candidateSummary = parts.join('\n');
    const maxChars = maxTokens * 4; // ~4 chars per token

    // Enforce max token budget by truncating on line boundaries (keeps summary stable).
    const candidateLines = candidateSummary.split('\n');
    const finalLines: string[] = [];
    let used = 0;
    for (const line of candidateLines) {
      const next = (finalLines.length ? '\n' : '') + line;
      if (used + next.length > maxChars) break;
      finalLines.push(line);
      used += next.length;
    }
    const summary = finalLines.join('\n');

    // Best-effort analytics: record how much we trimmed vs the full candidate summary.
    this.trackTokenSavings({
      tool: 'session_summary',
      workspace_id: withDefaults.workspace_id,
      project_id: withDefaults.project_id,
      candidate_chars: candidateSummary.length,
      context_chars: summary.length,
      max_tokens: maxTokens,
      metadata: {
        decision_count: decisionCount,
        memory_count: memoryCount,
      },
    }).catch(() => {});

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
    const candidateParts: string[] = [];
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
          candidateParts.push('## Relevant Decisions\n');
          currentChars += 25;
          
          const decisionEntries = decisions.items.map((d) => {
            const title = d.title || 'Decision';
            return { title, entry: `• ${title}\n` };
          });

          // Candidate: everything we could include before packing/truncation.
          for (const d of decisionEntries) {
            candidateParts.push(d.entry);
          }
          candidateParts.push('\n');

          // Final: what fits in the budget.
          for (const d of decisionEntries) {
            if (currentChars + d.entry.length > maxChars * 0.4) break; // Reserve 40% for decisions
            parts.push(d.entry);
            currentChars += d.entry.length;
            sources.push({ type: 'decision', title: d.title });
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
          candidateParts.push('## Related Context\n');
          currentChars += 20;
          
          const memoryEntries = memory.results.map((m) => {
            const title = m.title || 'Context';
            const content = m.content?.slice(0, 200) || '';
            return { title, entry: `• ${title}: ${content}...\n` };
          });

          // Candidate: everything we could include before packing/truncation.
          for (const m of memoryEntries) {
            candidateParts.push(m.entry);
          }
          candidateParts.push('\n');

          // Final: what fits in the budget.
          for (const m of memoryEntries) {
            if (currentChars + m.entry.length > maxChars * 0.7) break; // Reserve 30% for code
            parts.push(m.entry);
            currentChars += m.entry.length;
            sources.push({ type: 'memory', title: m.title });
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
          candidateParts.push('## Relevant Code\n');
          currentChars += 18;
          
          const codeEntries = code.results.map((c) => {
            const path = c.file_path || 'file';
            const content = c.content?.slice(0, 150) || '';
            return { path, entry: `• ${path}: ${content}...\n` };
          });

          // Candidate: everything we could include before packing/truncation.
          for (const c of codeEntries) {
            candidateParts.push(c.entry);
          }

          // Final: what fits in the budget.
          for (const c of codeEntries) {
            if (currentChars + c.entry.length > maxChars) break;
            parts.push(c.entry);
            currentChars += c.entry.length;
            sources.push({ type: 'code', title: c.path });
          }
        }
      } catch { /* optional */ }
    }

    const context = parts.join('');
    const candidateContext = candidateParts.join('');
    const tokenEstimate = Math.ceil(context.length / charsPerToken);

    this.trackTokenSavings({
      tool: 'ai_context_budget',
      workspace_id: withDefaults.workspace_id,
      project_id: withDefaults.project_id,
      candidate_chars: candidateContext.length,
      context_chars: context.length,
      max_tokens: maxTokens,
      metadata: {
        include_decisions: params.include_decisions !== false,
        include_memory: params.include_memory !== false,
        include_code: !!params.include_code,
        sources: sources.length,
      },
    }).catch(() => {});

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
    workspace_id?: string;
    project_id?: string;
    errors?: string[];
    version_notice?: VersionNotice;
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
      const wsResponse = await this.getWorkspace(withDefaults.workspace_id);
      const ws = unwrapApiResponse<{ name?: string }>(wsResponse);
      const workspaceName = pickString(ws?.name);
      if (workspaceName) {
        items.push({ type: 'W', key: 'workspace', value: workspaceName, relevance: 1 });
      } else {
        // Workspace exists but no name - still indicate we have context
        items.push({ type: 'W', key: 'workspace', value: `id:${withDefaults.workspace_id}`, relevance: 1 });
      }
    } catch (e) {
      errors.push(`workspace: ${(e as Error)?.message || 'fetch failed'}`);
      // Still add workspace ID so we know context exists
      items.push({ type: 'W', key: 'workspace', value: `id:${withDefaults.workspace_id}`, relevance: 0.5 });
    }

    if (withDefaults.project_id) {
      try {
        const projResponse = await this.getProject(withDefaults.project_id);
        const proj = unwrapApiResponse<{ name?: string }>(projResponse);
        const projectName = pickString(proj?.name);
        if (projectName) {
          items.push({ type: 'P', key: 'project', value: projectName, relevance: 1 });
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
    let candidateContext: string;
    
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
      candidateContext = items.map((i) => `${i.type}:${i.value}`).join('|');
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

      const candidateGrouped: Record<string, string[]> = {};
      for (const item of items) {
        if (!candidateGrouped[item.type]) candidateGrouped[item.type] = [];
        candidateGrouped[item.type].push(item.value);
      }
      candidateContext = JSON.stringify(candidateGrouped);
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

      const candidateLines: string[] = ['[CTX]'];
      for (const item of items) {
        candidateLines.push(`${item.type}:${item.value}`);
      }
      candidateLines.push('[/CTX]');
      candidateContext = candidateLines.join('\n');
    }
    
    // If context is empty but we have workspace, add a hint
    if (context.length === 0 && withDefaults.workspace_id) {
      const wsHint = items.find(i => i.type === 'W')?.value || withDefaults.workspace_id;
      context = format === 'minified'
        ? `W:${wsHint}|[NO_MATCHES]`
        : `[CTX]\nW:${wsHint}\n[NO_MATCHES]\n[/CTX]`;
      candidateContext = context;
    }

    let versionNotice: VersionNotice | null = null;
    try {
      versionNotice = await getUpdateNotice();
    } catch {
      // ignore version check failures
    }

    this.trackTokenSavings({
      tool: 'context_smart',
      workspace_id: withDefaults.workspace_id,
      project_id: withDefaults.project_id,
      candidate_chars: candidateContext.length,
      context_chars: context.length,
      max_tokens: maxTokens,
      metadata: {
        format,
        items: items.length,
        keywords: keywords.slice(0, 10),
        errors: errors.length,
      },
    }).catch(() => {});

    return {
      context,
      token_estimate: Math.ceil(context.length / 4),
      format,
      sources_used: items.filter(i => context.includes(i.value.slice(0, 20))).length,
      workspace_id: withDefaults.workspace_id,
      project_id: withDefaults.project_id,
      ...(versionNotice ? { version_notice: versionNotice } : {}),
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

  // ============================================
  // Slack Integration Methods
  // ============================================

  /**
   * Get Slack integration statistics and overview
   */
  async slackStats(params: {
    workspace_id?: string;
    days?: number;
  }): Promise<{
    summary: {
      total_messages: number;
      total_threads: number;
      active_users: number;
      channels_synced: number;
    };
    channels: Array<{
      channel_id: string;
      channel_name: string;
      message_count: number;
      thread_count: number;
      last_message_at: string | null;
    }>;
    activity: Array<{
      date: string;
      messages: number;
      threads: number;
    }>;
    sync_status: {
      status: string;
      last_sync_at: string | null;
      next_sync_at: string | null;
      error_message: string | null;
    };
  }> {
    const withDefaults = this.withDefaults(params || {});
    if (!withDefaults.workspace_id) {
      throw new Error('workspace_id is required for Slack stats');
    }
    const query = new URLSearchParams();
    if (params?.days) query.set('days', String(params.days));
    const suffix = query.toString() ? `?${query.toString()}` : '';
    return request(this.config, `/workspaces/${withDefaults.workspace_id}/slack/stats${suffix}`, { method: 'GET' });
  }

  /**
   * Get Slack users for a workspace
   */
  async slackUsers(params: {
    workspace_id?: string;
    page?: number;
    per_page?: number;
  }): Promise<{
    items: Array<{
      id: string;
      slack_user_id: string;
      display_name: string | null;
      real_name: string | null;
      email: string | null;
      avatar_url: string | null;
      is_bot: boolean;
      message_count: number;
      last_message_at: string | null;
    }>;
    total: number;
    page: number;
    per_page: number;
    total_pages: number;
  }> {
    const withDefaults = this.withDefaults(params || {});
    if (!withDefaults.workspace_id) {
      throw new Error('workspace_id is required for Slack users');
    }
    const query = new URLSearchParams();
    if (params?.page) query.set('page', String(params.page));
    if (params?.per_page) query.set('per_page', String(params.per_page));
    const suffix = query.toString() ? `?${query.toString()}` : '';
    return request(this.config, `/workspaces/${withDefaults.workspace_id}/slack/users${suffix}`, { method: 'GET' });
  }

  /**
   * Get Slack channels with stats
   */
  async slackChannels(params: {
    workspace_id?: string;
  }): Promise<Array<{
    channel_id: string;
    channel_name: string;
    message_count: number;
    thread_count: number;
    last_message_at: string | null;
  }>> {
    const withDefaults = this.withDefaults(params || {});
    if (!withDefaults.workspace_id) {
      throw new Error('workspace_id is required for Slack channels');
    }
    return request(this.config, `/workspaces/${withDefaults.workspace_id}/slack/channels`, { method: 'GET' });
  }

  /**
   * Get recent Slack activity feed
   */
  async slackActivity(params: {
    workspace_id?: string;
    limit?: number;
    offset?: number;
    channel_id?: string;
  }): Promise<Array<{
    id: string;
    channel_id: string;
    channel_name: string;
    user_id: string | null;
    user_name: string | null;
    user_avatar: string | null;
    content: string;
    content_preview: string | null;
    occurred_at: string;
    reply_count: number;
    reaction_count: number;
    thread_ts: string | null;
  }>> {
    const withDefaults = this.withDefaults(params || {});
    if (!withDefaults.workspace_id) {
      throw new Error('workspace_id is required for Slack activity');
    }
    const query = new URLSearchParams();
    if (params?.limit) query.set('limit', String(params.limit));
    if (params?.offset) query.set('offset', String(params.offset));
    if (params?.channel_id) query.set('channel_id', params.channel_id);
    const suffix = query.toString() ? `?${query.toString()}` : '';
    return request(this.config, `/workspaces/${withDefaults.workspace_id}/slack/activity${suffix}`, { method: 'GET' });
  }

  /**
   * Get high-engagement Slack discussions
   */
  async slackDiscussions(params: {
    workspace_id?: string;
    limit?: number;
  }): Promise<Array<{
    id: string;
    channel_id: string;
    channel_name: string;
    content_preview: string;
    reply_count: number;
    reaction_count: number;
    participant_count: number;
    occurred_at: string;
  }>> {
    const withDefaults = this.withDefaults(params || {});
    if (!withDefaults.workspace_id) {
      throw new Error('workspace_id is required for Slack discussions');
    }
    const query = new URLSearchParams();
    if (params?.limit) query.set('limit', String(params.limit));
    const suffix = query.toString() ? `?${query.toString()}` : '';
    return request(this.config, `/workspaces/${withDefaults.workspace_id}/slack/discussions${suffix}`, { method: 'GET' });
  }

  /**
   * Get top Slack contributors
   */
  async slackContributors(params: {
    workspace_id?: string;
    limit?: number;
  }): Promise<Array<{
    id: string;
    slack_user_id: string;
    display_name: string | null;
    real_name: string | null;
    email: string | null;
    avatar_url: string | null;
    is_bot: boolean;
    message_count: number;
    last_message_at: string | null;
  }>> {
    const withDefaults = this.withDefaults(params || {});
    if (!withDefaults.workspace_id) {
      throw new Error('workspace_id is required for Slack contributors');
    }
    const query = new URLSearchParams();
    if (params?.limit) query.set('limit', String(params.limit));
    const suffix = query.toString() ? `?${query.toString()}` : '';
    return request(this.config, `/workspaces/${withDefaults.workspace_id}/slack/contributors${suffix}`, { method: 'GET' });
  }

  /**
   * Trigger a sync of Slack user profiles
   */
  async slackSyncUsers(params: {
    workspace_id?: string;
  }): Promise<{
    synced_users: number;
    auto_mapped: number;
  }> {
    const withDefaults = this.withDefaults(params || {});
    if (!withDefaults.workspace_id) {
      throw new Error('workspace_id is required for syncing Slack users');
    }
    return request(this.config, `/workspaces/${withDefaults.workspace_id}/slack/sync-users`, { method: 'POST' });
  }

  /**
   * Search Slack messages
   */
  async slackSearch(params: {
    workspace_id?: string;
    q: string;
    limit?: number;
  }): Promise<Array<{
    id: string;
    channel_id: string;
    channel_name: string;
    user_id: string | null;
    user_name: string | null;
    user_avatar: string | null;
    content: string;
    content_preview: string | null;
    occurred_at: string;
    reply_count: number;
    reaction_count: number;
    thread_ts: string | null;
  }>> {
    const withDefaults = this.withDefaults(params || {});
    if (!withDefaults.workspace_id) {
      throw new Error('workspace_id is required for Slack search');
    }
    const query = new URLSearchParams();
    query.set('q', params.q);
    if (params?.limit) query.set('limit', String(params.limit));
    return request(this.config, `/workspaces/${withDefaults.workspace_id}/slack/search?${query.toString()}`, { method: 'GET' });
  }

  // ============================================
  // GitHub Integration Methods
  // ============================================

  /**
   * Get GitHub integration statistics and overview
   */
  async githubStats(params: {
    workspace_id?: string;
  }): Promise<{
    summary: {
      total_issues: number;
      total_prs: number;
      total_releases: number;
      total_comments: number;
      repos_synced: number;
      contributors: number;
    };
    repos: Array<{
      repo_name: string;
      issue_count: number;
      pr_count: number;
      release_count: number;
      comment_count: number;
      last_activity_at: string | null;
    }>;
    activity: Array<{
      date: string;
      issues: number;
      prs: number;
      comments: number;
    }>;
    sync_status: {
      status: string;
      last_sync_at: string | null;
      next_sync_at: string | null;
      error_message: string | null;
    };
  }> {
    const withDefaults = this.withDefaults(params || {});
    if (!withDefaults.workspace_id) {
      throw new Error('workspace_id is required for GitHub stats');
    }
    return request(this.config, `/workspaces/${withDefaults.workspace_id}/github/stats`, { method: 'GET' });
  }

  /**
   * Get GitHub repository stats
   */
  async githubRepos(params: {
    workspace_id?: string;
  }): Promise<Array<{
    repo_name: string;
    issue_count: number;
    pr_count: number;
    release_count: number;
    comment_count: number;
    last_activity_at: string | null;
  }>> {
    const withDefaults = this.withDefaults(params || {});
    if (!withDefaults.workspace_id) {
      throw new Error('workspace_id is required for GitHub repos');
    }
    return request(this.config, `/workspaces/${withDefaults.workspace_id}/github/repos`, { method: 'GET' });
  }

  /**
   * Get recent GitHub activity feed
   */
  async githubActivity(params: {
    workspace_id?: string;
    limit?: number;
    offset?: number;
    repo?: string;
    type?: string;
  }): Promise<Array<{
    id: string;
    item_type: string;
    repo: string;
    number: number | null;
    title: string;
    content_preview: string | null;
    state: string | null;
    author: string | null;
    url: string | null;
    labels: string[];
    comment_count: number;
    occurred_at: string;
  }>> {
    const withDefaults = this.withDefaults(params || {});
    if (!withDefaults.workspace_id) {
      throw new Error('workspace_id is required for GitHub activity');
    }
    const query = new URLSearchParams();
    if (params?.limit) query.set('limit', String(params.limit));
    if (params?.offset) query.set('offset', String(params.offset));
    if (params?.repo) query.set('repo', params.repo);
    if (params?.type) query.set('type', params.type);
    const suffix = query.toString() ? `?${query.toString()}` : '';
    return request(this.config, `/workspaces/${withDefaults.workspace_id}/github/activity${suffix}`, { method: 'GET' });
  }

  /**
   * Get GitHub issues and PRs
   */
  async githubIssues(params: {
    workspace_id?: string;
    limit?: number;
    offset?: number;
    state?: string;
    repo?: string;
  }): Promise<Array<{
    id: string;
    item_type: string;
    repo: string;
    number: number | null;
    title: string;
    content_preview: string | null;
    state: string | null;
    author: string | null;
    url: string | null;
    labels: string[];
    comment_count: number;
    occurred_at: string;
  }>> {
    const withDefaults = this.withDefaults(params || {});
    if (!withDefaults.workspace_id) {
      throw new Error('workspace_id is required for GitHub issues');
    }
    const query = new URLSearchParams();
    if (params?.limit) query.set('limit', String(params.limit));
    if (params?.offset) query.set('offset', String(params.offset));
    if (params?.state) query.set('state', params.state);
    if (params?.repo) query.set('repo', params.repo);
    const suffix = query.toString() ? `?${query.toString()}` : '';
    return request(this.config, `/workspaces/${withDefaults.workspace_id}/github/issues${suffix}`, { method: 'GET' });
  }

  /**
   * Get top GitHub contributors
   */
  async githubContributors(params: {
    workspace_id?: string;
    limit?: number;
  }): Promise<Array<{
    username: string;
    contribution_count: number;
    avatar_url: string | null;
  }>> {
    const withDefaults = this.withDefaults(params || {});
    if (!withDefaults.workspace_id) {
      throw new Error('workspace_id is required for GitHub contributors');
    }
    const query = new URLSearchParams();
    if (params?.limit) query.set('limit', String(params.limit));
    const suffix = query.toString() ? `?${query.toString()}` : '';
    return request(this.config, `/workspaces/${withDefaults.workspace_id}/github/contributors${suffix}`, { method: 'GET' });
  }

  /**
   * Search GitHub content
   */
  async githubSearch(params: {
    workspace_id?: string;
    q: string;
    limit?: number;
  }): Promise<{
    items: Array<{
      id: string;
      item_type: string;
      repo: string;
      number: number | null;
      title: string;
      content_preview: string | null;
      state: string | null;
      author: string | null;
      url: string | null;
      labels: string[];
      comment_count: number;
      occurred_at: string;
    }>;
    total: number;
  }> {
    const withDefaults = this.withDefaults(params || {});
    if (!withDefaults.workspace_id) {
      throw new Error('workspace_id is required for GitHub search');
    }
    const query = new URLSearchParams();
    query.set('q', params.q);
    if (params?.limit) query.set('limit', String(params.limit));
    return request(this.config, `/workspaces/${withDefaults.workspace_id}/github/search?${query.toString()}`, { method: 'GET' });
  }

  /**
   * Get knowledge extracted from GitHub (decisions, lessons, insights)
   */
  async githubKnowledge(params: {
    workspace_id?: string;
    limit?: number;
    node_type?: string;
  }): Promise<Array<{
    id: string;
    node_type: string;
    title: string;
    summary: string;
    confidence: number;
    source_type: string;
    occurred_at: string;
    tags: string[];
  }>> {
    const withDefaults = this.withDefaults(params || {});
    if (!withDefaults.workspace_id) {
      throw new Error('workspace_id is required for GitHub knowledge');
    }
    const query = new URLSearchParams();
    if (params?.limit) query.set('limit', String(params.limit));
    if (params?.node_type) query.set('node_type', params.node_type);
    const suffix = query.toString() ? `?${query.toString()}` : '';
    return request(this.config, `/workspaces/${withDefaults.workspace_id}/github/knowledge${suffix}`, { method: 'GET' });
  }

  /**
   * Get knowledge extracted from Slack (decisions, lessons, insights)
   */
  async slackKnowledge(params: {
    workspace_id?: string;
    limit?: number;
    node_type?: string;
  }): Promise<Array<{
    id: string;
    node_type: string;
    title: string;
    summary: string;
    confidence: number;
    source_type: string;
    occurred_at: string;
    tags: string[];
  }>> {
    const withDefaults = this.withDefaults(params || {});
    if (!withDefaults.workspace_id) {
      throw new Error('workspace_id is required for Slack knowledge');
    }
    const query = new URLSearchParams();
    if (params?.limit) query.set('limit', String(params.limit));
    if (params?.node_type) query.set('node_type', params.node_type);
    const suffix = query.toString() ? `?${query.toString()}` : '';
    return request(this.config, `/workspaces/${withDefaults.workspace_id}/slack/knowledge${suffix}`, { method: 'GET' });
  }

  /**
   * Get integration status for all providers in a workspace
   */
  async integrationsStatus(params: {
    workspace_id?: string;
  }): Promise<Array<{
    provider: string;
    status: string;
    last_sync_at: string | null;
    next_sync_at: string | null;
    error_message: string | null;
    resources_synced: number;
  }>> {
    const withDefaults = this.withDefaults(params || {});
    if (!withDefaults.workspace_id) {
      throw new Error('workspace_id is required for integrations status');
    }
    return request(this.config, `/workspaces/${withDefaults.workspace_id}/integrations/status`, { method: 'GET' });
  }

  /**
   * Get GitHub summary for a workspace
   */
  async githubSummary(params: {
    workspace_id?: string;
    days?: number;
    repo?: string;
  }): Promise<unknown> {
    const withDefaults = this.withDefaults(params || {});
    if (!withDefaults.workspace_id) {
      throw new Error('workspace_id is required for GitHub summary');
    }
    const query = new URLSearchParams();
    if (params?.days) query.set('days', String(params.days));
    if (params?.repo) query.set('repo', params.repo);
    const suffix = query.toString() ? `?${query.toString()}` : '';
    return request(this.config, `/github/summary${suffix}`, { method: 'GET' });
  }

  /**
   * Get Slack summary for a workspace
   */
  async slackSummary(params: {
    workspace_id?: string;
    days?: number;
    channel?: string;
  }): Promise<unknown> {
    const withDefaults = this.withDefaults(params || {});
    if (!withDefaults.workspace_id) {
      throw new Error('workspace_id is required for Slack summary');
    }
    const query = new URLSearchParams();
    if (params?.days) query.set('days', String(params.days));
    if (params?.channel) query.set('channel', params.channel);
    const suffix = query.toString() ? `?${query.toString()}` : '';
    return request(this.config, `/slack/summary${suffix}`, { method: 'GET' });
  }

  /**
   * Cross-source search across all integrations
   */
  async integrationsSearch(params: {
    workspace_id?: string;
    query: string;
    limit?: number;
    sources?: string[];
    days?: number;
    sort_by?: string;
  }): Promise<unknown> {
    const withDefaults = this.withDefaults(params || {});
    if (!withDefaults.workspace_id) {
      throw new Error('workspace_id is required for integrations search');
    }
    const urlParams = new URLSearchParams();
    urlParams.set('q', params.query);
    urlParams.set('workspace_id', withDefaults.workspace_id);
    if (params?.limit) urlParams.set('limit', String(params.limit));
    if (params?.sources) urlParams.set('sources', params.sources.join(','));
    if (params?.days) urlParams.set('days', String(params.days));
    if (params?.sort_by) urlParams.set('sort_by', params.sort_by);
    return request(this.config, `/integrations/search?${urlParams.toString()}`, { method: 'GET' });
  }

  /**
   * Cross-source summary across all integrations
   */
  async integrationsSummary(params: {
    workspace_id?: string;
    days?: number;
  }): Promise<unknown> {
    const withDefaults = this.withDefaults(params || {});
    if (!withDefaults.workspace_id) {
      throw new Error('workspace_id is required for integrations summary');
    }
    const query = new URLSearchParams();
    query.set('workspace_id', withDefaults.workspace_id);
    if (params?.days) query.set('days', String(params.days));
    return request(this.config, `/integrations/summary?${query.toString()}`, { method: 'GET' });
  }

  /**
   * Cross-source knowledge from all integrations
   */
  async integrationsKnowledge(params: {
    workspace_id?: string;
    knowledge_type?: string;
    query?: string;
    sources?: string[];
    limit?: number;
  }): Promise<unknown> {
    const withDefaults = this.withDefaults(params || {});
    if (!withDefaults.workspace_id) {
      throw new Error('workspace_id is required for integrations knowledge');
    }
    const urlParams = new URLSearchParams();
    urlParams.set('workspace_id', withDefaults.workspace_id);
    if (params?.knowledge_type) urlParams.set('knowledge_type', params.knowledge_type);
    if (params?.query) urlParams.set('query', params.query);
    if (params?.sources) urlParams.set('sources', params.sources.join(','));
    if (params?.limit) urlParams.set('limit', String(params.limit));
    return request(this.config, `/integrations/knowledge?${urlParams.toString()}`, { method: 'GET' });
  }

  // ============================================
  // Reminder Methods
  // ============================================

  /**
   * List reminders for the user
   */
  async remindersList(params?: {
    workspace_id?: string;
    project_id?: string;
    status?: string;
    priority?: string;
    limit?: number;
  }): Promise<{
    reminders: Array<{
      id: string;
      title: string;
      content: string;
      remind_at: string;
      priority: string;
      status: string;
      keywords: string[];
      memory_event_id: string | null;
      created_at: string;
    }>;
    total: number;
  }> {
    const withDefaults = this.withDefaults(params || {});
    const query = new URLSearchParams();
    if (withDefaults.workspace_id) query.set('workspace_id', withDefaults.workspace_id);
    if (withDefaults.project_id) query.set('project_id', withDefaults.project_id);
    if (params?.status) query.set('status', params.status);
    if (params?.priority) query.set('priority', params.priority);
    if (params?.limit) query.set('limit', String(params.limit));
    const suffix = query.toString() ? `?${query.toString()}` : '';
    return request(this.config, `/reminders${suffix}`, { method: 'GET' });
  }

  /**
   * Get active reminders (pending, overdue, due soon)
   */
  async remindersActive(params?: {
    workspace_id?: string;
    project_id?: string;
    context?: string;
    limit?: number;
  }): Promise<{
    reminders: Array<{
      id: string;
      title: string;
      content_preview: string;
      remind_at: string;
      priority: string;
      urgency: string;
      keywords: string[];
      memory_event_id: string | null;
    }>;
    overdue_count: number;
  }> {
    const withDefaults = this.withDefaults(params || {});
    const query = new URLSearchParams();
    if (withDefaults.workspace_id) query.set('workspace_id', withDefaults.workspace_id);
    if (withDefaults.project_id) query.set('project_id', withDefaults.project_id);
    if (params?.context) query.set('context', params.context);
    if (params?.limit) query.set('limit', String(params.limit));
    const suffix = query.toString() ? `?${query.toString()}` : '';
    return request(this.config, `/reminders/active${suffix}`, { method: 'GET' });
  }

  /**
   * Create a new reminder
   */
  async remindersCreate(params: {
    workspace_id?: string;
    project_id?: string;
    title: string;
    content: string;
    remind_at: string;
    priority?: string;
    keywords?: string[];
    recurrence?: string;
    memory_event_id?: string;
  }): Promise<{
    id: string;
    title: string;
    content: string;
    remind_at: string;
    priority: string;
    status: string;
  }> {
    const withDefaults = this.withDefaults(params);
    return request(this.config, '/reminders', {
      body: {
        workspace_id: withDefaults.workspace_id,
        project_id: withDefaults.project_id,
        title: params.title,
        content: params.content,
        remind_at: params.remind_at,
        priority: params.priority || 'normal',
        keywords: params.keywords || [],
        recurrence: params.recurrence,
        memory_event_id: params.memory_event_id,
      },
    });
  }

  /**
   * Snooze a reminder
   */
  async remindersSnooze(params: {
    reminder_id: string;
    until: string;
  }): Promise<{ id: string; snoozed_until: string; status: string }> {
    uuidSchema.parse(params.reminder_id);
    return request(this.config, `/reminders/${params.reminder_id}/snooze`, {
      body: { until: params.until },
    });
  }

  /**
   * Mark a reminder as completed
   */
  async remindersComplete(params: {
    reminder_id: string;
  }): Promise<{ id: string; status: string; completed_at: string }> {
    uuidSchema.parse(params.reminder_id);
    return request(this.config, `/reminders/${params.reminder_id}/complete`, { method: 'POST' });
  }

  /**
   * Dismiss a reminder
   */
  async remindersDismiss(params: {
    reminder_id: string;
  }): Promise<{ id: string; status: string; dismissed_at: string }> {
    uuidSchema.parse(params.reminder_id);
    return request(this.config, `/reminders/${params.reminder_id}/dismiss`, { method: 'POST' });
  }

  /**
   * Delete a reminder
   */
  async remindersDelete(params: {
    reminder_id: string;
  }): Promise<{ success: boolean }> {
    uuidSchema.parse(params.reminder_id);
    return request(this.config, `/reminders/${params.reminder_id}`, { method: 'DELETE' });
  }
}
