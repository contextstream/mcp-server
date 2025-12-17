import { z } from 'zod';
import type { McpServer } from '@modelcontextprotocol/sdk/server/mcp.js';
import type { ContextStreamClient } from './client.js';
import { readFilesFromDirectory, readAllFilesInBatches } from './files.js';
import { SessionManager } from './session-manager.js';
import { getAvailableEditors, generateRuleContent, generateAllRuleFiles } from './rules-templates.js';

type StructuredContent = { [x: string]: unknown } | undefined;
type ToolTextResult = {
  content: Array<{ type: 'text'; text: string }>;
  structuredContent?: StructuredContent;
  isError?: boolean;
};

function formatContent(data: unknown) {
  return JSON.stringify(data, null, 2);
}

function toStructured(data: unknown): StructuredContent {
  if (data && typeof data === 'object' && !Array.isArray(data)) {
    return data as { [x: string]: unknown };
  }
  return undefined;
}

export function registerTools(server: McpServer, client: ContextStreamClient, sessionManager?: SessionManager) {
  const upgradeUrl = process.env.CONTEXTSTREAM_UPGRADE_URL || 'https://contextstream.io/pricing';
  const defaultProTools = new Set<string>([
    // AI endpoints (typically paid/credit-metered)
    'ai_context',
    'ai_enhanced_context',
    'ai_context_budget',
    'ai_embeddings',
    'ai_plan',
    'ai_tasks',
  ]);

  const proTools = (() => {
    const raw = process.env.CONTEXTSTREAM_PRO_TOOLS;
    if (!raw) return defaultProTools;
    const parsed = raw
      .split(',')
      .map((t) => t.trim())
      .filter(Boolean);
    return parsed.length > 0 ? new Set(parsed) : defaultProTools;
  })();

  function getToolAccessTier(toolName: string): 'free' | 'pro' {
    return proTools.has(toolName) ? 'pro' : 'free';
  }

  function getToolAccessLabel(toolName: string): 'Free' | 'PRO' {
    return getToolAccessTier(toolName) === 'pro' ? 'PRO' : 'Free';
  }

  async function gateIfProTool(toolName: string): Promise<ToolTextResult | null> {
    if (getToolAccessTier(toolName) !== 'pro') return null;

    const planName = await client.getPlanName();
    if (planName !== 'free') return null;

    return errorResult(
      [
        `Access denied: \`${toolName}\` requires ContextStream PRO.`,
        `Upgrade: ${upgradeUrl}`,
      ].join('\n')
    );
  }
  
  /**
   * AUTO-CONTEXT WRAPPER
   * 
   * This wraps tool handlers to automatically initialize session context
   * on the FIRST tool call of any conversation.
   * 
   * Benefits:
   * - Works with ALL MCP clients (Windsurf, Cursor, Claude Desktop, VS Code, etc.)
   * - No client-side changes required
   * - Context is loaded regardless of which tool the AI calls first
   * - Only runs once per session (efficient)
   */
  function wrapWithAutoContext<T, R>(
    toolName: string,
    handler: (input: T) => Promise<R>
  ): (input: T) => Promise<R> {
    if (!sessionManager) {
      return handler; // No session manager = no auto-context
    }

    return async (input: T): Promise<R> => {
      // Skip auto-init for session_init itself
      const skipAutoInit = toolName === 'session_init';

      let contextPrefix = '';

      if (!skipAutoInit) {
        const autoInitResult = await sessionManager.autoInitialize();
        if (autoInitResult) {
          contextPrefix = autoInitResult.contextSummary + '\n\n';
        }
      }

      // Warn if context_smart hasn't been called yet
      sessionManager.warnIfContextSmartNotCalled(toolName);

      // Call the original handler
      const result = await handler(input);

      // Prepend context to the response if we auto-initialized
      if (contextPrefix && result && typeof result === 'object') {
        const r = result as { content?: Array<{ type: string; text: string }> };
        if (r.content && r.content.length > 0 && r.content[0].type === 'text') {
          r.content[0] = {
            ...r.content[0],
            text: contextPrefix + '--- Tool Response ---\n\n' + r.content[0].text,
          };
        }
      }

      return result;
    };
  }

  /**
   * Helper to register a tool with auto-context wrapper applied.
   * This is a drop-in replacement for server.registerTool that adds auto-context.
   */
  function registerTool<T extends z.ZodType>(
    name: string,
    config: { title: string; description: string; inputSchema: T },
    handler: (input: z.infer<T>) => Promise<ToolTextResult>
  ) {
    const accessLabel = getToolAccessLabel(name);
    const labeledConfig = {
      ...config,
      title: `${config.title} (${accessLabel})`,
      description: `${config.description}\n\nAccess: ${accessLabel}${accessLabel === 'PRO' ? ` (upgrade: ${upgradeUrl})` : ''}`,
    };

    // Wrap handler with error handling to ensure proper serialization
    const safeHandler = async (input: z.infer<T>) => {
      try {
        const gated = await gateIfProTool(name);
        if (gated) return gated;

        return await handler(input);
      } catch (error: any) {
        // Convert error to a properly serializable format
        const errorMessage = error?.message || String(error);
        const errorDetails = error?.body || error?.details || null;
        const errorCode = error?.code || error?.status || 'UNKNOWN_ERROR';

        const isPlanLimit =
          String(errorCode).toUpperCase() === 'FORBIDDEN' &&
          String(errorMessage).toLowerCase().includes('plan limit reached');
        const upgradeHint = isPlanLimit ? `\nUpgrade: ${upgradeUrl}` : '';
        
        // Re-throw with a proper Error that has a string message
        const serializedError = new Error(
          `[${errorCode}] ${errorMessage}${upgradeHint}${errorDetails ? `: ${JSON.stringify(errorDetails)}` : ''}`
        );
        throw serializedError;
      }
    };
    
    server.registerTool(
      name,
      labeledConfig,
      wrapWithAutoContext(name, safeHandler)
    );
  }

  function errorResult(text: string): ToolTextResult {
    return {
      content: [{ type: 'text' as const, text }],
      isError: true,
    };
  }

  function resolveWorkspaceId(explicitWorkspaceId?: string): string | undefined {
    if (explicitWorkspaceId) return explicitWorkspaceId;
    const ctx = sessionManager?.getContext();
    return typeof ctx?.workspace_id === 'string' ? (ctx.workspace_id as string) : undefined;
  }

  function resolveProjectId(explicitProjectId?: string): string | undefined {
    if (explicitProjectId) return explicitProjectId;
    const ctx = sessionManager?.getContext();
    return typeof ctx?.project_id === 'string' ? (ctx.project_id as string) : undefined;
  }

  // Auth
  registerTool(
    'auth_me',
    {
      title: 'Get current user',
      description: 'Fetch authenticated user profile',
      inputSchema: z.object({}),
    },
    async () => {
      const result = await client.me();
      return { content: [{ type: 'text' as const, text: formatContent(result) }], structuredContent: toStructured(result) };
    }
  );

  // Workspaces
  registerTool(
    'workspaces_list',
    {
      title: 'List workspaces',
      description: 'List accessible workspaces',
      inputSchema: z.object({ page: z.number().optional(), page_size: z.number().optional() }),
    },
    async (input) => {
      const result = await client.listWorkspaces(input);
      return { content: [{ type: 'text' as const, text: formatContent(result) }], structuredContent: toStructured(result) };
    }
  );

  registerTool(
    'workspaces_create',
    {
      title: 'Create workspace',
      description: 'Create a new workspace',
      inputSchema: z.object({
        name: z.string(),
        description: z.string().optional(),
        visibility: z.enum(['private', 'team', 'org']).optional(),
      }),
    },
    async (input) => {
      const result = await client.createWorkspace(input);
      return { content: [{ type: 'text' as const, text: formatContent(result) }], structuredContent: toStructured(result) };
    }
  );

  registerTool(
    'workspaces_update',
    {
      title: 'Update workspace',
      description: 'Update a workspace (rename, change description, or visibility)',
      inputSchema: z.object({
        workspace_id: z.string().uuid(),
        name: z.string().optional(),
        description: z.string().optional(),
        visibility: z.enum(['private', 'team', 'org']).optional(),
      }),
    },
    async (input) => {
      const { workspace_id, ...updates } = input;
      const result = await client.updateWorkspace(workspace_id, updates);
      return { content: [{ type: 'text' as const, text: formatContent(result) }], structuredContent: toStructured(result) };
    }
  );

  registerTool(
    'workspaces_delete',
    {
      title: 'Delete workspace',
      description: 'Delete a workspace and all its contents (projects, memory, etc.). This action is irreversible.',
      inputSchema: z.object({
        workspace_id: z.string().uuid(),
      }),
    },
    async (input) => {
      const result = await client.deleteWorkspace(input.workspace_id);
      return { content: [{ type: 'text' as const, text: formatContent(result || { success: true, message: 'Workspace deleted successfully' }) }], structuredContent: toStructured(result) };
    }
  );

  // Projects
  registerTool(
    'projects_list',
    {
      title: 'List projects',
      description: 'List projects (optionally by workspace)',
      inputSchema: z.object({ workspace_id: z.string().uuid().optional(), page: z.number().optional(), page_size: z.number().optional() }),
    },
    async (input) => {
      const result = await client.listProjects(input);
      return { content: [{ type: 'text' as const, text: formatContent(result) }], structuredContent: toStructured(result) };
    }
  );

  registerTool(
    'projects_create',
    {
      title: 'Create project',
      description: 'Create a project within a workspace',
      inputSchema: z.object({
        name: z.string(),
        description: z.string().optional(),
        workspace_id: z.string().uuid().optional(),
      }),
    },
    async (input) => {
      const result = await client.createProject(input);
      return { content: [{ type: 'text' as const, text: formatContent(result) }], structuredContent: toStructured(result) };
    }
  );

  registerTool(
    'projects_update',
    {
      title: 'Update project',
      description: 'Update a project (rename or change description)',
      inputSchema: z.object({
        project_id: z.string().uuid(),
        name: z.string().optional(),
        description: z.string().optional(),
      }),
    },
    async (input) => {
      const { project_id, ...updates } = input;
      const result = await client.updateProject(project_id, updates);
      return { content: [{ type: 'text' as const, text: formatContent(result) }], structuredContent: toStructured(result) };
    }
  );

  registerTool(
    'projects_delete',
    {
      title: 'Delete project',
      description: 'Delete a project and all its contents (indexed files, memory events, etc.). This action is irreversible.',
      inputSchema: z.object({
        project_id: z.string().uuid(),
      }),
    },
    async (input) => {
      const result = await client.deleteProject(input.project_id);
      return { content: [{ type: 'text' as const, text: formatContent(result || { success: true, message: 'Project deleted successfully' }) }], structuredContent: toStructured(result) };
    }
  );

  registerTool(
    'projects_index',
    {
      title: 'Index project',
      description: 'Trigger indexing for a project',
      inputSchema: z.object({ project_id: z.string().uuid().optional() }),
    },
    async (input) => {
      const projectId = resolveProjectId(input.project_id);
      if (!projectId) {
        return errorResult('Error: project_id is required. Please call session_init first or provide project_id explicitly.');
      }

      const result = await client.indexProject(projectId);
      return { content: [{ type: 'text' as const, text: formatContent(result) }], structuredContent: toStructured(result) };
    }
  );

  // Search
  const searchSchema = z.object({
    query: z.string(),
    workspace_id: z.string().uuid().optional(),
    project_id: z.string().uuid().optional(),
    limit: z.number().optional(),
  });

  registerTool(
    'search_semantic',
    { title: 'Semantic search', description: 'Semantic vector search', inputSchema: searchSchema },
    async (input) => {
      const result = await client.searchSemantic(input);
      return { content: [{ type: 'text' as const, text: formatContent(result) }], structuredContent: toStructured(result) };
    }
  );

  registerTool(
    'search_hybrid',
    { title: 'Hybrid search', description: 'Hybrid search (semantic + keyword)', inputSchema: searchSchema },
    async (input) => {
      const result = await client.searchHybrid(input);
      return { content: [{ type: 'text' as const, text: formatContent(result) }], structuredContent: toStructured(result) };
    }
  );

  registerTool(
    'search_keyword',
    { title: 'Keyword search', description: 'Keyword search', inputSchema: searchSchema },
    async (input) => {
      const result = await client.searchKeyword(input);
      return { content: [{ type: 'text' as const, text: formatContent(result) }], structuredContent: toStructured(result) };
    }
  );

  registerTool(
    'search_pattern',
    { title: 'Pattern search', description: 'Pattern/regex search', inputSchema: searchSchema },
    async (input) => {
      const result = await client.searchPattern(input);
      return { content: [{ type: 'text' as const, text: formatContent(result) }], structuredContent: toStructured(result) };
    }
  );

  // Memory / Knowledge
  registerTool(
    'memory_create_event',
    {
      title: 'Create memory event',
      description: 'Create a memory event for a workspace/project',
      inputSchema: z.object({
        workspace_id: z.string().uuid().optional(),
        project_id: z.string().uuid().optional(),
        event_type: z.string(),
        title: z.string(),
        content: z.string(),
        metadata: z.record(z.any()).optional(),
      }),
    },
    async (input) => {
      const result = await client.createMemoryEvent(input);
      return { content: [{ type: 'text' as const, text: formatContent(result) }], structuredContent: toStructured(result) };
    }
  );

  registerTool(
    'memory_bulk_ingest',
    {
      title: 'Bulk ingest events',
      description: 'Bulk ingest multiple memory events',
      inputSchema: z.object({
        workspace_id: z.string().uuid().optional(),
        project_id: z.string().uuid().optional(),
        events: z.array(z.record(z.any())),
      }),
    },
    async (input) => {
      const result = await client.bulkIngestEvents(input);
      return { content: [{ type: 'text' as const, text: formatContent(result) }], structuredContent: toStructured(result) };
    }
  );

  registerTool(
    'memory_list_events',
    {
      title: 'List memory events',
      description: 'List memory events (optionally scoped)',
      inputSchema: z.object({
        workspace_id: z.string().uuid().optional(),
        project_id: z.string().uuid().optional(),
        limit: z.number().optional(),
      }),
    },
    async (input) => {
      const result = await client.listMemoryEvents(input);
      return { content: [{ type: 'text' as const, text: formatContent(result) }], structuredContent: toStructured(result) };
    }
  );

  registerTool(
    'memory_create_node',
    {
      title: 'Create knowledge node',
      description: 'Create a knowledge node with optional relations',
      inputSchema: z.object({
        workspace_id: z.string().uuid().optional(),
        project_id: z.string().uuid().optional(),
        node_type: z.string(),
        title: z.string(),
        content: z.string(),
        relations: z
          .array(
            z.object({
              type: z.string(),
              target_id: z.string().uuid(),
            })
          )
          .optional(),
      }),
    },
    async (input) => {
      const result = await client.createKnowledgeNode(input);
      return { content: [{ type: 'text' as const, text: formatContent(result) }], structuredContent: toStructured(result) };
    }
  );

  registerTool(
    'memory_list_nodes',
    {
      title: 'List knowledge nodes',
      description: 'List knowledge graph nodes',
      inputSchema: z.object({
        workspace_id: z.string().uuid().optional(),
        project_id: z.string().uuid().optional(),
        limit: z.number().optional(),
      }),
    },
    async (input) => {
      const result = await client.listKnowledgeNodes(input);
      return { content: [{ type: 'text' as const, text: formatContent(result) }], structuredContent: toStructured(result) };
    }
  );

  registerTool(
    'memory_search',
    {
      title: 'Memory-aware search',
      description: 'Search memory events/notes',
      inputSchema: z.object({
        query: z.string(),
        workspace_id: z.string().uuid().optional(),
        project_id: z.string().uuid().optional(),
        limit: z.number().optional(),
      }),
    },
    async (input) => {
      const result = await client.memorySearch(input);
      return { content: [{ type: 'text' as const, text: formatContent(result) }], structuredContent: toStructured(result) };
    }
  );

  registerTool(
    'memory_decisions',
    {
      title: 'Decision summaries',
      description: 'List decision summaries from workspace memory',
      inputSchema: z.object({
        workspace_id: z.string().uuid().optional(),
        project_id: z.string().uuid().optional(),
        limit: z.number().optional(),
      }),
    },
    async (input) => {
      const result = await client.memoryDecisions(input);
      return { content: [{ type: 'text' as const, text: formatContent(result) }], structuredContent: toStructured(result) };
    }
  );

  // Graph
  registerTool(
    'graph_related',
    {
      title: 'Related knowledge nodes',
      description: 'Find related nodes in the knowledge graph',
      inputSchema: z.object({
        node_id: z.string(),
        workspace_id: z.string().uuid().optional(),
        project_id: z.string().uuid().optional(),
        limit: z.number().optional(),
      }),
    },
    async (input) => {
      const result = await client.graphRelated(input);
      return { content: [{ type: 'text' as const, text: formatContent(result) }], structuredContent: toStructured(result) };
    }
  );

  registerTool(
    'graph_path',
    {
      title: 'Knowledge path',
      description: 'Find path between two nodes',
      inputSchema: z.object({
        source_id: z.string(),
        target_id: z.string(),
        workspace_id: z.string().uuid().optional(),
        project_id: z.string().uuid().optional(),
      }),
    },
    async (input) => {
      const result = await client.graphPath(input);
      return { content: [{ type: 'text' as const, text: formatContent(result) }], structuredContent: toStructured(result) };
    }
  );

  registerTool(
    'graph_decisions',
    {
      title: 'Decision graph',
      description: 'Decision history in the knowledge graph',
      inputSchema: z.object({
        workspace_id: z.string().uuid().optional(),
        project_id: z.string().uuid().optional(),
        limit: z.number().optional(),
      }),
    },
    async (input) => {
      const result = await client.graphDecisions(input);
      return { content: [{ type: 'text' as const, text: formatContent(result) }], structuredContent: toStructured(result) };
    }
  );

  registerTool(
    'graph_dependencies',
    {
      title: 'Code dependencies',
      description: 'Dependency graph query',
      inputSchema: z.object({
        target: z.object({ type: z.string(), id: z.string() }),
        max_depth: z.number().optional(),
        include_transitive: z.boolean().optional(),
      }),
    },
    async (input) => {
      const result = await client.graphDependencies(input);
      return { content: [{ type: 'text' as const, text: formatContent(result) }], structuredContent: toStructured(result) };
    }
  );

  registerTool(
    'graph_call_path',
    {
      title: 'Call path',
      description: 'Find call path between two targets',
      inputSchema: z.object({
        source: z.object({ type: z.string(), id: z.string() }),
        target: z.object({ type: z.string(), id: z.string() }),
        max_depth: z.number().optional(),
      }),
    },
    async (input) => {
      const result = await client.graphCallPath(input);
      return { content: [{ type: 'text' as const, text: formatContent(result) }], structuredContent: toStructured(result) };
    }
  );

  registerTool(
    'graph_impact',
    {
      title: 'Impact analysis',
      description: 'Analyze impact of a target node',
      inputSchema: z.object({ target: z.object({ type: z.string(), id: z.string() }), max_depth: z.number().optional() }),
    },
    async (input) => {
      const result = await client.graphImpact(input);
      return { content: [{ type: 'text' as const, text: formatContent(result) }], structuredContent: toStructured(result) };
    }
  );

  // AI
  registerTool(
    'ai_context',
    {
      title: 'Build AI context',
      description: 'Build LLM context (docs/memory/code) for a query',
      inputSchema: z.object({
        query: z.string(),
        workspace_id: z.string().uuid().optional(),
        project_id: z.string().uuid().optional(),
        include_code: z.boolean().optional(),
        include_docs: z.boolean().optional(),
        include_memory: z.boolean().optional(),
        limit: z.number().optional(),
      }),
    },
    async (input) => {
      const result = await client.aiContext(input);
      return { content: [{ type: 'text' as const, text: formatContent(result) }], structuredContent: toStructured(result) };
    }
  );

  registerTool(
    'ai_embeddings',
    {
      title: 'Generate embeddings',
      description: 'Generate embeddings for a text',
      inputSchema: z.object({ text: z.string() }),
    },
    async (input) => {
      const result = await client.aiEmbeddings(input);
      return { content: [{ type: 'text' as const, text: formatContent(result) }], structuredContent: toStructured(result) };
    }
  );

  registerTool(
    'ai_plan',
    {
      title: 'Generate dev plan',
      description: 'Generate development plan from description',
      inputSchema: z.object({
        description: z.string(),
        project_id: z.string().uuid().optional(),
        complexity: z.string().optional(),
      }),
    },
    async (input) => {
      const result = await client.aiPlan(input);
      return { content: [{ type: 'text' as const, text: formatContent(result) }], structuredContent: toStructured(result) };
    }
  );

  registerTool(
    'ai_tasks',
    {
      title: 'Generate tasks',
      description: 'Generate tasks from plan or description',
      inputSchema: z.object({
        plan_id: z.string().optional(),
        description: z.string().optional(),
        project_id: z.string().uuid().optional(),
        granularity: z.string().optional(),
      }),
    },
    async (input) => {
      const result = await client.aiTasks(input);
      return { content: [{ type: 'text' as const, text: formatContent(result) }], structuredContent: toStructured(result) };
    }
  );

  registerTool(
    'ai_enhanced_context',
    {
      title: 'Enhanced AI context',
      description: 'Build enhanced LLM context with deeper analysis',
      inputSchema: z.object({
        query: z.string(),
        workspace_id: z.string().uuid().optional(),
        project_id: z.string().uuid().optional(),
        include_code: z.boolean().optional(),
        include_docs: z.boolean().optional(),
        include_memory: z.boolean().optional(),
        limit: z.number().optional(),
      }),
    },
    async (input) => {
      const result = await client.aiEnhancedContext(input);
      return { content: [{ type: 'text' as const, text: formatContent(result) }], structuredContent: toStructured(result) };
    }
  );

  // Extended project operations
  registerTool(
    'projects_get',
    {
      title: 'Get project',
      description: 'Get project details by ID',
      inputSchema: z.object({ project_id: z.string().uuid().optional() }),
    },
    async (input) => {
      const projectId = resolveProjectId(input.project_id);
      if (!projectId) {
        return errorResult('Error: project_id is required. Please call session_init first or provide project_id explicitly.');
      }

      const result = await client.getProject(projectId);
      return { content: [{ type: 'text' as const, text: formatContent(result) }], structuredContent: toStructured(result) };
    }
  );

  registerTool(
    'projects_overview',
    {
      title: 'Project overview',
      description: 'Get project overview with summary information',
      inputSchema: z.object({ project_id: z.string().uuid().optional() }),
    },
    async (input) => {
      const projectId = resolveProjectId(input.project_id);
      if (!projectId) {
        return errorResult('Error: project_id is required. Please call session_init first or provide project_id explicitly.');
      }

      const result = await client.projectOverview(projectId);
      return { content: [{ type: 'text' as const, text: formatContent(result) }], structuredContent: toStructured(result) };
    }
  );

  registerTool(
    'projects_statistics',
    {
      title: 'Project statistics',
      description: 'Get project statistics (files, lines, complexity)',
      inputSchema: z.object({ project_id: z.string().uuid().optional() }),
    },
    async (input) => {
      const projectId = resolveProjectId(input.project_id);
      if (!projectId) {
        return errorResult('Error: project_id is required. Please call session_init first or provide project_id explicitly.');
      }

      const result = await client.projectStatistics(projectId);
      return { content: [{ type: 'text' as const, text: formatContent(result) }], structuredContent: toStructured(result) };
    }
  );

  registerTool(
    'projects_files',
    {
      title: 'List project files',
      description: 'List all indexed files in a project',
      inputSchema: z.object({ project_id: z.string().uuid().optional() }),
    },
    async (input) => {
      const projectId = resolveProjectId(input.project_id);
      if (!projectId) {
        return errorResult('Error: project_id is required. Please call session_init first or provide project_id explicitly.');
      }

      const result = await client.projectFiles(projectId);
      return { content: [{ type: 'text' as const, text: formatContent(result) }], structuredContent: toStructured(result) };
    }
  );

  registerTool(
    'projects_index_status',
    {
      title: 'Index status',
      description: 'Get project indexing status',
      inputSchema: z.object({ project_id: z.string().uuid().optional() }),
    },
    async (input) => {
      const projectId = resolveProjectId(input.project_id);
      if (!projectId) {
        return errorResult('Error: project_id is required. Please call session_init first or provide project_id explicitly.');
      }

      const result = await client.projectIndexStatus(projectId);
      return { content: [{ type: 'text' as const, text: formatContent(result) }], structuredContent: toStructured(result) };
    }
  );

  registerTool(
    'projects_ingest_local',
    {
      title: 'Ingest local files',
      description: `Read ALL files from a local directory and ingest them for indexing.
This indexes your entire project by reading files in batches.
Automatically detects code files and skips ignored directories like node_modules, target, dist, etc.`,
      inputSchema: z.object({
        project_id: z.string().uuid().optional().describe('Project to ingest files into (defaults to current session project)'),
        path: z.string().describe('Local directory path to read files from'),
      }),
    },
    async (input) => {
      const projectId = resolveProjectId(input.project_id);
      if (!projectId) {
        return errorResult('Error: project_id is required. Please call session_init first or provide project_id explicitly.');
      }

      // Start ingestion in background to avoid blocking the agent
      (async () => {
        try {
          let totalIndexed = 0;
          let batchCount = 0;
          
          console.error(`[ContextStream] Starting background ingestion for project ${projectId} from ${input.path}`);
          
          for await (const batch of readAllFilesInBatches(input.path, { batchSize: 50 })) {
            const result = await client.ingestFiles(projectId, batch) as { data?: { files_indexed: number } };
            totalIndexed += result.data?.files_indexed ?? batch.length;
            batchCount++;
          }
          
          console.error(`[ContextStream] Completed background ingestion: ${totalIndexed} files in ${batchCount} batches`);
        } catch (error) {
          console.error(`[ContextStream] Ingestion failed:`, error);
        }
      })();

      const summary = {
        status: 'started',
        message: 'Ingestion running in background',
        project_id: projectId,
        path: input.path,
        note: "Use 'projects_index_status' to monitor progress."
      };
      
      return { 
        content: [{ 
          type: 'text' as const, 
          text: `Ingestion started in background for directory: ${input.path}. Use 'projects_index_status' to monitor progress.` 
        }],
        structuredContent: toStructured(summary)
      };
    }
  );

  // Extended workspace operations
  registerTool(
    'workspaces_get',
    {
      title: 'Get workspace',
      description: 'Get workspace details by ID',
      inputSchema: z.object({ workspace_id: z.string().uuid().optional() }),
    },
    async (input) => {
      const workspaceId = resolveWorkspaceId(input.workspace_id);
      if (!workspaceId) {
        return errorResult('Error: workspace_id is required. Please call session_init first or provide workspace_id explicitly.');
      }

      const result = await client.getWorkspace(workspaceId);
      return { content: [{ type: 'text' as const, text: formatContent(result) }], structuredContent: toStructured(result) };
    }
  );

  registerTool(
    'workspaces_overview',
    {
      title: 'Workspace overview',
      description: 'Get workspace overview with summary information',
      inputSchema: z.object({ workspace_id: z.string().uuid().optional() }),
    },
    async (input) => {
      const workspaceId = resolveWorkspaceId(input.workspace_id);
      if (!workspaceId) {
        return errorResult('Error: workspace_id is required. Please call session_init first or provide workspace_id explicitly.');
      }

      const result = await client.workspaceOverview(workspaceId);
      return { content: [{ type: 'text' as const, text: formatContent(result) }], structuredContent: toStructured(result) };
    }
  );

  registerTool(
    'workspaces_analytics',
    {
      title: 'Workspace analytics',
      description: 'Get workspace usage analytics',
      inputSchema: z.object({ workspace_id: z.string().uuid().optional() }),
    },
    async (input) => {
      const workspaceId = resolveWorkspaceId(input.workspace_id);
      if (!workspaceId) {
        return errorResult('Error: workspace_id is required. Please call session_init first or provide workspace_id explicitly.');
      }

      const result = await client.workspaceAnalytics(workspaceId);
      return { content: [{ type: 'text' as const, text: formatContent(result) }], structuredContent: toStructured(result) };
    }
  );

  registerTool(
    'workspaces_content',
    {
      title: 'Workspace content',
      description: 'List content in a workspace',
      inputSchema: z.object({ workspace_id: z.string().uuid().optional() }),
    },
    async (input) => {
      const workspaceId = resolveWorkspaceId(input.workspace_id);
      if (!workspaceId) {
        return errorResult('Error: workspace_id is required. Please call session_init first or provide workspace_id explicitly.');
      }

      const result = await client.workspaceContent(workspaceId);
      return { content: [{ type: 'text' as const, text: formatContent(result) }], structuredContent: toStructured(result) };
    }
  );

  // Extended memory operations
  registerTool(
    'memory_get_event',
    {
      title: 'Get memory event',
      description: 'Get a specific memory event by ID',
      inputSchema: z.object({ event_id: z.string().uuid() }),
    },
    async (input) => {
      const result = await client.getMemoryEvent(input.event_id);
      return { content: [{ type: 'text' as const, text: formatContent(result) }], structuredContent: toStructured(result) };
    }
  );

  registerTool(
    'memory_update_event',
    {
      title: 'Update memory event',
      description: 'Update a memory event',
      inputSchema: z.object({
        event_id: z.string().uuid(),
        title: z.string().optional(),
        content: z.string().optional(),
        metadata: z.record(z.any()).optional(),
      }),
    },
    async (input) => {
      const { event_id, ...body } = input;
      const result = await client.updateMemoryEvent(event_id, body);
      return { content: [{ type: 'text' as const, text: formatContent(result) }], structuredContent: toStructured(result) };
    }
  );

  registerTool(
    'memory_delete_event',
    {
      title: 'Delete memory event',
      description: 'Delete a memory event',
      inputSchema: z.object({ event_id: z.string().uuid() }),
    },
    async (input) => {
      const result = await client.deleteMemoryEvent(input.event_id);
      return { content: [{ type: 'text' as const, text: formatContent(result) }], structuredContent: toStructured(result) };
    }
  );

  registerTool(
    'memory_distill_event',
    {
      title: 'Distill memory event',
      description: 'Extract and condense key insights from a memory event',
      inputSchema: z.object({ event_id: z.string().uuid() }),
    },
    async (input) => {
      const result = await client.distillMemoryEvent(input.event_id);
      return { content: [{ type: 'text' as const, text: formatContent(result) }], structuredContent: toStructured(result) };
    }
  );

  registerTool(
    'memory_get_node',
    {
      title: 'Get knowledge node',
      description: 'Get a specific knowledge node by ID',
      inputSchema: z.object({ node_id: z.string().uuid() }),
    },
    async (input) => {
      const result = await client.getKnowledgeNode(input.node_id);
      return { content: [{ type: 'text' as const, text: formatContent(result) }], structuredContent: toStructured(result) };
    }
  );

  registerTool(
    'memory_update_node',
    {
      title: 'Update knowledge node',
      description: 'Update a knowledge node',
      inputSchema: z.object({
        node_id: z.string().uuid(),
        title: z.string().optional(),
        content: z.string().optional(),
        relations: z.array(z.object({ type: z.string(), target_id: z.string().uuid() })).optional(),
      }),
    },
    async (input) => {
      const { node_id, ...body } = input;
      const result = await client.updateKnowledgeNode(node_id, body);
      return { content: [{ type: 'text' as const, text: formatContent(result) }], structuredContent: toStructured(result) };
    }
  );

  registerTool(
    'memory_delete_node',
    {
      title: 'Delete knowledge node',
      description: 'Delete a knowledge node',
      inputSchema: z.object({ node_id: z.string().uuid() }),
    },
    async (input) => {
      const result = await client.deleteKnowledgeNode(input.node_id);
      return { content: [{ type: 'text' as const, text: formatContent(result) }], structuredContent: toStructured(result) };
    }
  );

  registerTool(
    'memory_supersede_node',
    {
      title: 'Supersede knowledge node',
      description: 'Replace a knowledge node with updated information (maintains history)',
      inputSchema: z.object({
        node_id: z.string().uuid(),
        new_content: z.string(),
        reason: z.string().optional(),
      }),
    },
    async (input) => {
      const { node_id, ...body } = input;
      const result = await client.supersedeKnowledgeNode(node_id, body);
      return { content: [{ type: 'text' as const, text: formatContent(result) }], structuredContent: toStructured(result) };
    }
  );

  registerTool(
    'memory_timeline',
    {
      title: 'Memory timeline',
      description: 'Get chronological timeline of memory events for a workspace',
      inputSchema: z.object({ workspace_id: z.string().uuid().optional() }),
    },
    async (input) => {
      const workspaceId = resolveWorkspaceId(input.workspace_id);
      if (!workspaceId) {
        return errorResult('Error: workspace_id is required. Please call session_init first or provide workspace_id explicitly.');
      }

      const result = await client.memoryTimeline(workspaceId);
      return { content: [{ type: 'text' as const, text: formatContent(result) }], structuredContent: toStructured(result) };
    }
  );

  registerTool(
    'memory_summary',
    {
      title: 'Memory summary',
      description: 'Get condensed summary of workspace memory',
      inputSchema: z.object({ workspace_id: z.string().uuid().optional() }),
    },
    async (input) => {
      const workspaceId = resolveWorkspaceId(input.workspace_id);
      if (!workspaceId) {
        return errorResult('Error: workspace_id is required. Please call session_init first or provide workspace_id explicitly.');
      }

      const result = await client.memorySummary(workspaceId);
      return { content: [{ type: 'text' as const, text: formatContent(result) }], structuredContent: toStructured(result) };
    }
  );

  // Extended graph operations
  registerTool(
    'graph_circular_dependencies',
    {
      title: 'Find circular dependencies',
      description: 'Detect circular dependencies in project code',
      inputSchema: z.object({ project_id: z.string().uuid().optional() }),
    },
    async (input) => {
      const projectId = resolveProjectId(input.project_id);
      if (!projectId) {
        return errorResult('Error: project_id is required. Please call session_init first or provide project_id explicitly.');
      }

      const result = await client.findCircularDependencies(projectId);
      return { content: [{ type: 'text' as const, text: formatContent(result) }], structuredContent: toStructured(result) };
    }
  );

  registerTool(
    'graph_unused_code',
    {
      title: 'Find unused code',
      description: 'Detect unused code in project',
      inputSchema: z.object({ project_id: z.string().uuid().optional() }),
    },
    async (input) => {
      const projectId = resolveProjectId(input.project_id);
      if (!projectId) {
        return errorResult('Error: project_id is required. Please call session_init first or provide project_id explicitly.');
      }

      const result = await client.findUnusedCode(projectId);
      return { content: [{ type: 'text' as const, text: formatContent(result) }], structuredContent: toStructured(result) };
    }
  );

  registerTool(
    'graph_contradictions',
    {
      title: 'Find contradictions',
      description: 'Find contradicting information related to a knowledge node',
      inputSchema: z.object({ node_id: z.string().uuid() }),
    },
    async (input) => {
      const result = await client.findContradictions(input.node_id);
      return { content: [{ type: 'text' as const, text: formatContent(result) }], structuredContent: toStructured(result) };
    }
  );

  // Search suggestions
  registerTool(
    'search_suggestions',
    {
      title: 'Search suggestions',
      description: 'Get search suggestions based on partial query',
      inputSchema: z.object({
        query: z.string(),
        workspace_id: z.string().uuid().optional(),
        project_id: z.string().uuid().optional(),
      }),
    },
    async (input) => {
      const result = await client.searchSuggestions(input);
      return { content: [{ type: 'text' as const, text: formatContent(result) }], structuredContent: toStructured(result) };
    }
  );

  // ============================================
  // Session & Auto-Context Tools (CORE-like)
  // ============================================

  registerTool(
    'session_init',
    {
      title: 'Initialize conversation session',
      description: `Initialize a new conversation session and automatically retrieve relevant context.
This is the FIRST tool AI assistants should call when starting a conversation.
Returns: workspace info, project info, recent memory, recent decisions, relevant context, and high-priority lessons.
Automatically detects the IDE workspace/project path and can auto-index code.

IMPORTANT: Pass the user's FIRST MESSAGE as context_hint to get semantically relevant context!
Example: session_init(folder_path="/path/to/project", context_hint="how do I implement auth?")

This does semantic search on the first message. You only need context_smart on subsequent messages.`,
      inputSchema: z.object({
        folder_path: z.string().optional().describe('Current workspace/project folder path (absolute). Use this when IDE roots are not available.'),
        workspace_id: z.string().uuid().optional().describe('Workspace to initialize context for'),
        project_id: z.string().uuid().optional().describe('Project to initialize context for'),
        session_id: z.string().optional().describe('Custom session ID (auto-generated if not provided)'),
        context_hint: z.string().optional().describe('RECOMMENDED: Pass the user\'s first message here for semantic search. This finds relevant context from ANY time, not just recent items.'),
        include_recent_memory: z.boolean().optional().describe('Include recent memory events (default: true)'),
        include_decisions: z.boolean().optional().describe('Include recent decisions (default: true)'),
        auto_index: z.boolean().optional().describe('Automatically create and index project from IDE workspace (default: true)'),
      }),
    },
    async (input) => {
      // Get IDE workspace roots if available
      let ideRoots: string[] = [];
      try {
        const rootsResponse = await server.server.listRoots();
        if (rootsResponse?.roots) {
          ideRoots = rootsResponse.roots.map((r: { uri: string; name?: string }) => r.uri.replace('file://', ''));
        }
      } catch {
        // IDE may not support roots - that's okay
      }
      
      // Fallback to explicit folder_path if IDE roots not available
      if (ideRoots.length === 0 && input.folder_path) {
        ideRoots = [input.folder_path];
      }
      
      const result = await client.initSession(input, ideRoots) as Record<string, unknown>;
      
      // Mark session as initialized to prevent auto-init on subsequent tool calls
      if (sessionManager) {
        sessionManager.markInitialized(result);
      }
      
      return { content: [{ type: 'text' as const, text: formatContent(result) }], structuredContent: toStructured(result) };
    }
  );

  registerTool(
    'session_get_user_context',
    {
      title: 'Get user context and preferences',
      description: `Retrieve user preferences, coding style, and persona from memory.
Use this to understand how the user likes to work and adapt your responses accordingly.`,
      inputSchema: z.object({
        workspace_id: z.string().uuid().optional().describe('Workspace to get user context from'),
      }),
    },
    async (input) => {
      const result = await client.getUserContext(input);
      return { content: [{ type: 'text' as const, text: formatContent(result) }], structuredContent: toStructured(result) };
    }
  );

  registerTool(
    'workspace_associate',
    {
      title: 'Associate folder with workspace',
      description: `Associate a folder/repo with a workspace after user selection.
Call this after session_init returns status='requires_workspace_selection' and the user has chosen a workspace.
This persists the selection to .contextstream/config.json so future sessions auto-connect.
Optionally creates a parent folder mapping (e.g., all repos under /dev/company/* map to the same workspace).
Optionally generates AI editor rules for automatic ContextStream usage.`,
      inputSchema: z.object({
        folder_path: z.string().describe('Absolute path to the folder/repo to associate'),
        workspace_id: z.string().uuid().describe('Workspace ID to associate with'),
        workspace_name: z.string().optional().describe('Workspace name for reference'),
        create_parent_mapping: z.boolean().optional().describe('Also create a parent folder mapping (e.g., /dev/maker/* -> workspace)'),
        generate_editor_rules: z.boolean().optional().describe('Generate AI editor rules for Windsurf, Cursor, Cline, Kilo Code, Roo Code, Claude Code, and Aider'),
      }),
    },
    async (input) => {
      const result = await client.associateWorkspace(input);
      
      // Optionally generate editor rules
      let rulesGenerated: string[] = [];
      if (input.generate_editor_rules) {
        const fs = await import('fs');
        const path = await import('path');
        
        for (const editor of getAvailableEditors()) {
          const rule = generateRuleContent(editor, {
            workspaceName: input.workspace_name,
            workspaceId: input.workspace_id,
          });
          if (rule) {
            const filePath = path.join(input.folder_path, rule.filename);
            try {
              // Only create if doesn't exist or already has ContextStream section
              let existingContent = '';
              try {
                existingContent = fs.readFileSync(filePath, 'utf-8');
              } catch {
                // File doesn't exist
              }
              
              if (!existingContent) {
                fs.writeFileSync(filePath, rule.content);
                rulesGenerated.push(rule.filename);
              } else if (!existingContent.includes('ContextStream Integration')) {
                fs.writeFileSync(filePath, existingContent + '\n\n' + rule.content);
                rulesGenerated.push(rule.filename + ' (appended)');
              }
            } catch {
              // Ignore errors for individual files
            }
          }
        }
      }
      
      const response = {
        ...result,
        editor_rules_generated: rulesGenerated.length > 0 ? rulesGenerated : undefined,
      };
      
      return { content: [{ type: 'text' as const, text: formatContent(response) }], structuredContent: toStructured(response) };
    }
  );

  registerTool(
    'workspace_bootstrap',
    {
      title: 'Create workspace + project from folder',
      description: `Create a new workspace (user-provided name) and onboard the current folder as a project.
This is useful when session_init returns status='requires_workspace_name' (no workspaces exist yet) or when you want to create a new workspace for a repo.

Behavior:
- Creates a workspace with the given name
- Associates the folder to that workspace (writes .contextstream/config.json)
- Initializes a session for the folder, which creates the project (folder name) and starts indexing (if enabled)`,
      inputSchema: z.object({
        workspace_name: z.string().min(1).describe('Name for the new workspace (ask the user)'),
        folder_path: z.string().optional().describe('Absolute folder path (defaults to IDE root/cwd)'),
        description: z.string().optional().describe('Optional workspace description'),
        visibility: z.enum(['private', 'public']).optional().describe('Workspace visibility (default: private)'),
        create_parent_mapping: z.boolean().optional().describe('Also create a parent folder mapping (e.g., /dev/company/* -> workspace)'),
        generate_editor_rules: z.boolean().optional().describe('Generate AI editor rules in the folder for automatic ContextStream usage'),
        context_hint: z.string().optional().describe('Optional context hint for session initialization'),
        auto_index: z.boolean().optional().describe('Automatically create and index project from folder (default: true)'),
      }),
    },
    async (input) => {
      // Resolve folder path (prefer explicit; fallback to IDE roots; then cwd)
      let folderPath = input.folder_path;
      if (!folderPath) {
        try {
          const rootsResponse = await server.server.listRoots();
          if (rootsResponse?.roots && rootsResponse.roots.length > 0) {
            folderPath = rootsResponse.roots[0].uri.replace('file://', '');
          }
        } catch {
          // IDE may not support roots - that's okay
        }
      }

      if (!folderPath) {
        folderPath = process.cwd();
      }

      if (!folderPath) {
        return errorResult('Error: folder_path is required. Provide folder_path or run from a project directory.');
      }

      const folderName = folderPath.split('/').pop() || 'My Project';

      let newWorkspace: { id?: string; name?: string };
      try {
        newWorkspace = (await client.createWorkspace({
          name: input.workspace_name,
          description: input.description || `Workspace created for ${folderPath}`,
          visibility: input.visibility || 'private',
        })) as { id?: string; name?: string };
      } catch (err: any) {
        const message = err?.message || String(err);
        if (typeof message === 'string' && message.includes('workspaces_slug_key')) {
          return errorResult(
            [
              'Failed to create workspace: the workspace slug is already taken (or reserved by a deleted workspace).',
              '',
              'Try a slightly different workspace name (e.g., add a suffix) and re-run `workspace_bootstrap`.',
            ].join('\n')
          );
        }
        throw err;
      }

      if (!newWorkspace?.id) {
        return errorResult('Error: failed to create workspace.');
      }

      // Persist folder -> workspace mapping (and optional parent mapping)
      const associateResult = await client.associateWorkspace({
        folder_path: folderPath,
        workspace_id: newWorkspace.id,
        workspace_name: newWorkspace.name || input.workspace_name,
        create_parent_mapping: input.create_parent_mapping,
      });

      // Optionally generate editor rules
      let rulesGenerated: string[] = [];
      if (input.generate_editor_rules) {
        const fs = await import('fs');
        const path = await import('path');

        for (const editor of getAvailableEditors()) {
          const rule = generateRuleContent(editor, {
            workspaceName: newWorkspace.name || input.workspace_name,
            workspaceId: newWorkspace.id,
          });
          if (!rule) continue;

          const filePath = path.join(folderPath, rule.filename);
          try {
            let existingContent = '';
            try {
              existingContent = fs.readFileSync(filePath, 'utf-8');
            } catch {
              // File doesn't exist
            }

            if (!existingContent) {
              fs.writeFileSync(filePath, rule.content);
              rulesGenerated.push(rule.filename);
            } else if (!existingContent.includes('ContextStream Integration')) {
              fs.writeFileSync(filePath, existingContent + '\n\n' + rule.content);
              rulesGenerated.push(rule.filename + ' (appended)');
            }
          } catch {
            // Ignore per-file failures
          }
        }
      }

      // Initialize a session for this folder; this creates the project (folder name) and starts indexing (if enabled)
      const session = await client.initSession(
        {
          workspace_id: newWorkspace.id,
          context_hint: input.context_hint,
          include_recent_memory: true,
          include_decisions: true,
          auto_index: input.auto_index,
        },
        [folderPath]
      ) as Record<string, unknown>;

      // Mark session as initialized so subsequent tool calls can omit IDs
      if (sessionManager) {
        sessionManager.markInitialized(session);
      }

      const response = {
        ...session,
        bootstrap: {
          folder_path: folderPath,
          project_name: folderName,
          workspace: {
            id: newWorkspace.id,
            name: newWorkspace.name || input.workspace_name,
          },
          association: associateResult,
          editor_rules_generated: rulesGenerated.length > 0 ? rulesGenerated : undefined,
        },
      };

      return { content: [{ type: 'text' as const, text: formatContent(response) }], structuredContent: toStructured(response) };
    }
  );

  registerTool(
    'session_capture',
    {
      title: 'Capture context to memory',
      description: `Automatically capture and store important context from the conversation.
Use this to persist decisions, insights, preferences, or important information.`,
      inputSchema: z.object({
        workspace_id: z.string().uuid().optional(),
        project_id: z.string().uuid().optional(),
        session_id: z.string().optional().describe('Session ID to associate with this capture'),
        event_type: z.enum([
          'conversation', 'decision', 'insight', 'preference', 'task', 'bug', 'feature',
          // Lesson system types
          'correction',    // User corrected the AI
          'lesson',        // Extracted lesson from correction
          'warning',       // Proactive reminder
          'frustration'    // User expressed frustration
        ]).describe('Type of context being captured'),
        title: z.string().describe('Brief title for the captured context'),
        content: z.string().describe('Full content/details to capture'),
        tags: z.array(z.string()).optional().describe('Tags for categorization'),
        importance: z.enum(['low', 'medium', 'high', 'critical']).optional().describe('Importance level'),
      }),
    },
    async (input) => {
      // Get workspace_id and project_id from session context if not provided
      let workspaceId = input.workspace_id;
      let projectId = input.project_id;
      
      if (!workspaceId && sessionManager) {
        const ctx = sessionManager.getContext();
        if (ctx) {
          workspaceId = ctx.workspace_id as string | undefined;
          projectId = projectId || ctx.project_id as string | undefined;
        }
      }
      
      if (!workspaceId) {
        return { 
          content: [{ 
            type: 'text' as const, 
            text: 'Error: workspace_id is required. Please call session_init first or provide workspace_id explicitly.' 
          }],
          isError: true,
        };
      }
      
      const result = await client.captureContext({
        ...input,
        workspace_id: workspaceId,
        project_id: projectId,
      });
      return { content: [{ type: 'text' as const, text: formatContent(result) }], structuredContent: toStructured(result) };
    }
  );

  // ============================================
  // Lesson System Tools
  // ============================================

  registerTool(
    'session_capture_lesson',
    {
      title: 'Capture a lesson learned',
      description: `Capture a lesson learned from a mistake or correction.
Use this when the user corrects you, expresses frustration, or points out an error.
These lessons are surfaced in future sessions to prevent repeating the same mistakes.

Example triggers:
- User says "No, you should..." or "That's wrong"
- User expresses frustration (caps, "COME ON", "WTF")
- Code breaks due to a preventable mistake

The lesson will be tagged with 'lesson' and stored with structured metadata for easy retrieval.`,
      inputSchema: z.object({
        workspace_id: z.string().uuid().optional(),
        project_id: z.string().uuid().optional(),
        title: z.string().describe('Lesson title - what to remember (e.g., "Always verify assets in git before pushing")'),
        severity: z.enum(['low', 'medium', 'high', 'critical']).default('medium')
          .describe('Severity: critical for production issues, high for breaking changes, medium for workflow, low for minor'),
        category: z.enum(['workflow', 'code_quality', 'verification', 'communication', 'project_specific'])
          .describe('Category of the lesson'),
        trigger: z.string().describe('What action caused the problem (e.g., "Pushed code referencing images without committing them")'),
        impact: z.string().describe('What went wrong (e.g., "Production 404 errors - broken landing page")'),
        prevention: z.string().describe('How to prevent in future (e.g., "Run git status to check untracked files before pushing")'),
        keywords: z.array(z.string()).optional()
          .describe('Keywords for matching in future contexts (e.g., ["git", "images", "assets", "push"])'),
      }),
    },
    async (input) => {
      // Get workspace_id from session context if not provided
      let workspaceId = input.workspace_id;
      let projectId = input.project_id;

      if (!workspaceId && sessionManager) {
        const ctx = sessionManager.getContext();
        if (ctx) {
          workspaceId = ctx.workspace_id as string | undefined;
          projectId = projectId || ctx.project_id as string | undefined;
        }
      }

      if (!workspaceId) {
        return {
          content: [{
            type: 'text' as const,
            text: 'Error: workspace_id is required. Please call session_init first or provide workspace_id explicitly.'
          }],
          isError: true,
        };
      }

      // Build structured content for the lesson
      const lessonContent = [
        `## ${input.title}`,
        '',
        `**Severity:** ${input.severity}`,
        `**Category:** ${input.category}`,
        '',
        '### Trigger',
        input.trigger,
        '',
        '### Impact',
        input.impact,
        '',
        '### Prevention',
        input.prevention,
        input.keywords?.length ? `\n**Keywords:** ${input.keywords.join(', ')}` : '',
      ].filter(Boolean).join('\n');

      const result = await client.captureContext({
        workspace_id: workspaceId,
        project_id: projectId,
        event_type: 'lesson',
        title: input.title,
        content: lessonContent,
        importance: input.severity,
        tags: [
          'lesson',
          input.category,
          `severity:${input.severity}`,
          ...(input.keywords || []),
        ],
      });

      return {
        content: [{
          type: 'text' as const,
          text: `✅ Lesson captured: "${input.title}"\n\nThis lesson will be surfaced in future sessions when relevant context is detected.`
        }],
        structuredContent: toStructured(result)
      };
    }
  );

  registerTool(
    'session_get_lessons',
    {
      title: 'Get lessons learned',
      description: `Retrieve lessons learned from past mistakes and corrections.
Use this to check for relevant warnings before taking actions that have caused problems before.

Returns lessons filtered by:
- Query: semantic search for relevant lessons
- Category: workflow, code_quality, verification, communication, project_specific
- Severity: low, medium, high, critical`,
      inputSchema: z.object({
        workspace_id: z.string().uuid().optional(),
        project_id: z.string().uuid().optional(),
        query: z.string().optional().describe('Search for relevant lessons (e.g., "git push images")'),
        category: z.enum(['workflow', 'code_quality', 'verification', 'communication', 'project_specific']).optional()
          .describe('Filter by category'),
        severity: z.enum(['low', 'medium', 'high', 'critical']).optional()
          .describe('Filter by minimum severity'),
        limit: z.number().default(10).describe('Maximum lessons to return'),
      }),
    },
    async (input) => {
      // Get workspace_id from session context if not provided
      let workspaceId = input.workspace_id;
      let projectId = input.project_id;

      if (!workspaceId && sessionManager) {
        const ctx = sessionManager.getContext();
        if (ctx) {
          workspaceId = ctx.workspace_id as string | undefined;
          projectId = projectId || ctx.project_id as string | undefined;
        }
      }

      // Build search query with lesson-specific terms
      const searchQuery = input.query
        ? `${input.query} lesson prevention warning`
        : 'lesson prevention warning mistake';

      const searchResult = await client.memorySearch({
        query: searchQuery,
        workspace_id: workspaceId,
        project_id: projectId,
        limit: input.limit * 2, // Fetch more to filter
      }) as { results?: any[] };

      // Filter for lessons and apply filters
      let lessons = (searchResult.results || []).filter((item: any) => {
        const tags = item.metadata?.tags || [];
        const isLesson = tags.includes('lesson');
        if (!isLesson) return false;

        // Filter by category if specified
        if (input.category && !tags.includes(input.category)) {
          return false;
        }

        // Filter by severity if specified
        if (input.severity) {
          const severityOrder = ['low', 'medium', 'high', 'critical'];
          const minSeverityIndex = severityOrder.indexOf(input.severity);
          const itemSeverity = tags.find((t: string) => t.startsWith('severity:'))?.split(':')[1] || 'medium';
          const itemSeverityIndex = severityOrder.indexOf(itemSeverity);
          if (itemSeverityIndex < minSeverityIndex) return false;
        }

        return true;
      }).slice(0, input.limit);

      if (lessons.length === 0) {
        return {
          content: [{ type: 'text' as const, text: 'No lessons found matching your criteria.' }],
          structuredContent: toStructured({ lessons: [], count: 0 }),
        };
      }

      // Format lessons for display
      const formattedLessons = lessons.map((lesson: any, i: number) => {
        const tags = lesson.metadata?.tags || [];
        const severity = tags.find((t: string) => t.startsWith('severity:'))?.split(':')[1] || 'medium';
        const category = tags.find((t: string) => ['workflow', 'code_quality', 'verification', 'communication', 'project_specific'].includes(t)) || 'unknown';

        const severityEmoji = ({
          low: '🟢',
          medium: '🟡',
          high: '🟠',
          critical: '🔴',
        } as Record<string, string>)[severity] || '⚪';

        return `${i + 1}. ${severityEmoji} **${lesson.title}**\n   Category: ${category} | Severity: ${severity}\n   ${lesson.content?.slice(0, 200)}...`;
      }).join('\n\n');

      return {
        content: [{
          type: 'text' as const,
          text: `📚 Found ${lessons.length} lesson(s):\n\n${formattedLessons}`
        }],
        structuredContent: toStructured({ lessons, count: lessons.length }),
      };
    }
  );

  registerTool(
    'session_smart_search',
    {
      title: 'Smart context search',
      description: `Search memory with automatic context enrichment.
Returns memory matches, relevant code, and related decisions in one call.`,
      inputSchema: z.object({
        query: z.string().describe('What to search for'),
        workspace_id: z.string().uuid().optional(),
        project_id: z.string().uuid().optional(),
        include_related: z.boolean().optional().describe('Include related context (default: true)'),
        include_decisions: z.boolean().optional().describe('Include related decisions (default: true)'),
      }),
    },
    async (input) => {
      // Get workspace_id from session context if not provided
      let workspaceId = input.workspace_id;
      let projectId = input.project_id;
      
      if (!workspaceId && sessionManager) {
        const ctx = sessionManager.getContext();
        if (ctx) {
          workspaceId = ctx.workspace_id as string | undefined;
          projectId = projectId || ctx.project_id as string | undefined;
        }
      }
      
      const result = await client.smartSearch({
        ...input,
        workspace_id: workspaceId,
        project_id: projectId,
      });
      return { content: [{ type: 'text' as const, text: formatContent(result) }], structuredContent: toStructured(result) };
    }
  );

  registerTool(
    'session_remember',
    {
      title: 'Remember this',
      description: `Quick way to store something in memory. Use natural language.
Example: "Remember that I prefer TypeScript strict mode" or "Remember we decided to use PostgreSQL"`,
      inputSchema: z.object({
        content: z.string().describe('What to remember (natural language)'),
        workspace_id: z.string().uuid().optional(),
        project_id: z.string().uuid().optional(),
        importance: z.enum(['low', 'medium', 'high']).optional(),
      }),
    },
    async (input) => {
      // Get workspace_id from session context if not provided
      let workspaceId = input.workspace_id;
      let projectId = input.project_id;
      
      if (!workspaceId && sessionManager) {
        const ctx = sessionManager.getContext();
        if (ctx) {
          workspaceId = ctx.workspace_id as string | undefined;
          projectId = projectId || ctx.project_id as string | undefined;
        }
      }
      
      if (!workspaceId) {
        return { 
          content: [{ type: 'text' as const, text: 'Error: workspace_id is required. Please call session_init first.' }],
          isError: true,
        };
      }
      
      // Auto-detect type from content
      const lowerContent = input.content.toLowerCase();
      let eventType: 'preference' | 'decision' | 'insight' | 'task' = 'insight';
      if (lowerContent.includes('prefer') || lowerContent.includes('like') || lowerContent.includes('always')) {
        eventType = 'preference';
      } else if (lowerContent.includes('decided') || lowerContent.includes('decision') || lowerContent.includes('chose')) {
        eventType = 'decision';
      } else if (lowerContent.includes('todo') || lowerContent.includes('task') || lowerContent.includes('need to')) {
        eventType = 'task';
      }

      const result = await client.captureContext({
        workspace_id: workspaceId,
        project_id: projectId,
        event_type: eventType,
        title: input.content.slice(0, 100),
        content: input.content,
        importance: input.importance || 'medium',
      });
      return { content: [{ type: 'text' as const, text: `Remembered: ${input.content.slice(0, 100)}...` }], structuredContent: toStructured(result) };
    }
  );

  registerTool(
    'session_recall',
    {
      title: 'Recall from memory',
      description: `Quick way to recall relevant context. Use natural language.
Example: "What were the auth decisions?" or "What are my TypeScript preferences?"`,
      inputSchema: z.object({
        query: z.string().describe('What to recall (natural language)'),
        workspace_id: z.string().uuid().optional(),
        project_id: z.string().uuid().optional(),
      }),
    },
    async (input) => {
      // Get workspace_id from session context if not provided
      let workspaceId = input.workspace_id;
      let projectId = input.project_id;
      
      if (!workspaceId && sessionManager) {
        const ctx = sessionManager.getContext();
        if (ctx) {
          workspaceId = ctx.workspace_id as string | undefined;
          projectId = projectId || ctx.project_id as string | undefined;
        }
      }
      
      const result = await client.smartSearch({
        query: input.query,
        workspace_id: workspaceId,
        project_id: projectId,
        include_decisions: true,
      });
      return { content: [{ type: 'text' as const, text: formatContent(result) }], structuredContent: toStructured(result) };
    }
  );

  // Editor rules generation
  registerTool(
    'generate_editor_rules',
    {
      title: 'Generate editor AI rules',
      description: `Generate AI rule files for editors (Windsurf, Cursor, Cline, Kilo Code, Roo Code, Claude Code, Aider).
These rules instruct the AI to automatically use ContextStream for memory and context.
Supported editors: ${getAvailableEditors().join(', ')}`,
      inputSchema: z.object({
        folder_path: z.string().describe('Absolute path to the project folder'),
        editors: z.array(z.enum(['windsurf', 'cursor', 'cline', 'kilo', 'roo', 'claude', 'aider', 'all']))
          .optional()
          .describe('Which editors to generate rules for. Defaults to all.'),
        workspace_name: z.string().optional().describe('Workspace name to include in rules'),
        workspace_id: z.string().uuid().optional().describe('Workspace ID to include in rules'),
        project_name: z.string().optional().describe('Project name to include in rules'),
        additional_rules: z.string().optional().describe('Additional project-specific rules to append'),
        dry_run: z.boolean().optional().describe('If true, return content without writing files'),
      }),
    },
    async (input) => {
      const fs = await import('fs');
      const path = await import('path');
      
      const editors = input.editors?.includes('all') || !input.editors 
        ? getAvailableEditors() 
        : input.editors.filter(e => e !== 'all');
      
      const results: Array<{ editor: string; filename: string; status: string; content?: string }> = [];
      
      for (const editor of editors) {
        const rule = generateRuleContent(editor, {
          workspaceName: input.workspace_name,
          workspaceId: input.workspace_id,
          projectName: input.project_name,
          additionalRules: input.additional_rules,
        });
        
        if (!rule) {
          results.push({ editor, filename: '', status: 'unknown editor' });
          continue;
        }
        
        const filePath = path.join(input.folder_path, rule.filename);
        
        if (input.dry_run) {
          results.push({ 
            editor, 
            filename: rule.filename, 
            status: 'dry run - would create',
            content: rule.content,
          });
        } else {
          try {
            // Check if file exists and has custom content
            let existingContent = '';
            try {
              existingContent = fs.readFileSync(filePath, 'utf-8');
            } catch {
              // File doesn't exist
            }
            
            if (existingContent && !existingContent.includes('ContextStream Integration')) {
              // Append to existing file
              const updatedContent = existingContent + '\n\n' + rule.content;
              fs.writeFileSync(filePath, updatedContent);
              results.push({ editor, filename: rule.filename, status: 'appended to existing' });
            } else {
              // Create or overwrite
              fs.writeFileSync(filePath, rule.content);
              results.push({ editor, filename: rule.filename, status: 'created' });
            }
          } catch (err) {
            results.push({ 
              editor, 
              filename: rule.filename, 
              status: `error: ${(err as Error).message}`,
            });
          }
        }
      }
      
      const summary = {
        folder: input.folder_path,
        results,
        message: input.dry_run 
          ? 'Dry run complete. Use dry_run: false to write files.'
          : `Generated ${results.filter(r => r.status === 'created' || r.status.includes('appended')).length} rule files.`,
      };
      
      return { content: [{ type: 'text' as const, text: formatContent(summary) }], structuredContent: toStructured(summary) };
    }
  );

  // ============================================
  // Token-Saving Context Tools
  // ============================================

  registerTool(
    'session_summary',
    {
      title: 'Get compact context summary',
      description: `Get a compact, token-efficient summary of workspace context (~500 tokens).
This is designed to replace loading full chat history in AI prompts.
Returns: workspace/project info, top decisions (titles only), preferences, memory count.
Use this at conversation start instead of loading everything.
For specific details, use session_recall or session_smart_search.`,
      inputSchema: z.object({
        workspace_id: z.string().uuid().optional(),
        project_id: z.string().uuid().optional(),
        max_tokens: z.number().optional().describe('Maximum tokens for summary (default: 500)'),
      }),
    },
    async (input) => {
      // Get workspace_id from session context if not provided
      let workspaceId = input.workspace_id;
      let projectId = input.project_id;
      
      if (!workspaceId && sessionManager) {
        const ctx = sessionManager.getContext();
        if (ctx) {
          workspaceId = ctx.workspace_id as string | undefined;
          projectId = projectId || ctx.project_id as string | undefined;
        }
      }
      
      const result = await client.getContextSummary({
        workspace_id: workspaceId,
        project_id: projectId,
        max_tokens: input.max_tokens,
      });
      
      // Return the summary as plain text for easy inclusion in prompts
      return {
        content: [{ type: 'text' as const, text: result.summary }],
        structuredContent: toStructured(result),
      };
    }
  );

  registerTool(
    'session_compress',
    {
      title: 'Compress chat history to memory',
      description: `Extract and store key information from chat history as memory events.
This allows clearing chat history while preserving important context.
Use at conversation end or when context window is getting full.

Extracts:
- Decisions made
- User preferences learned
- Insights discovered
- Tasks/action items
- Code patterns established

After compression, the AI can use session_recall to retrieve this context in future conversations.`,
      inputSchema: z.object({
        chat_history: z.string().describe('The chat history to compress and extract from'),
        workspace_id: z.string().uuid().optional(),
        project_id: z.string().uuid().optional(),
        extract_types: z.array(z.enum(['decisions', 'preferences', 'insights', 'tasks', 'code_patterns']))
          .optional()
          .describe('Types of information to extract (default: all)'),
      }),
    },
    async (input) => {
      // Get workspace_id from session context if not provided
      let workspaceId = input.workspace_id;
      let projectId = input.project_id;
      
      if (!workspaceId && sessionManager) {
        const ctx = sessionManager.getContext();
        if (ctx) {
          workspaceId = ctx.workspace_id as string | undefined;
          projectId = projectId || ctx.project_id as string | undefined;
        }
      }
      
      if (!workspaceId) {
        return {
          content: [{
            type: 'text' as const,
            text: 'Error: workspace_id is required. Please call session_init first or provide workspace_id explicitly.'
          }],
          isError: true,
        };
      }
      
      const result = await client.compressChat({
        workspace_id: workspaceId,
        project_id: projectId,
        chat_history: input.chat_history,
        extract_types: input.extract_types as Array<'decisions' | 'preferences' | 'insights' | 'tasks' | 'code_patterns'> | undefined,
      });
      
      const summary = [
        `✅ Compressed chat history into ${result.events_created} memory events:`,
        '',
        `📋 Decisions: ${result.extracted.decisions.length}`,
        `⚙️ Preferences: ${result.extracted.preferences.length}`,
        `💡 Insights: ${result.extracted.insights.length}`,
        `📝 Tasks: ${result.extracted.tasks.length}`,
        `🔧 Code patterns: ${result.extracted.code_patterns.length}`,
        '',
        'These are now stored in ContextStream memory.',
        'Future conversations can access them via session_recall.',
      ].join('\n');
      
      return {
        content: [{ type: 'text' as const, text: summary }],
        structuredContent: toStructured(result),
      };
    }
  );

  registerTool(
    'ai_context_budget',
    {
      title: 'Get context within token budget',
      description: `Get the most relevant context that fits within a specified token budget.
This is the key tool for token-efficient AI interactions:

1. AI calls this with a query and token budget
2. Gets optimally selected context (decisions, memory, code)
3. No need to include full chat history in the prompt

The tool prioritizes:
1. Relevant decisions (highest value per token)
2. Query-matched memory events
3. Related code snippets (if requested and budget allows)

Example: ai_context_budget(query="authentication", max_tokens=1000)`,
      inputSchema: z.object({
        query: z.string().describe('What context to retrieve'),
        max_tokens: z.number().describe('Maximum tokens for the context (e.g., 500, 1000, 2000)'),
        workspace_id: z.string().uuid().optional(),
        project_id: z.string().uuid().optional(),
        include_decisions: z.boolean().optional().describe('Include relevant decisions (default: true)'),
        include_memory: z.boolean().optional().describe('Include memory search results (default: true)'),
        include_code: z.boolean().optional().describe('Include code search results (default: false)'),
      }),
    },
    async (input) => {
      // Get workspace_id from session context if not provided
      let workspaceId = input.workspace_id;
      let projectId = input.project_id;
      
      if (!workspaceId && sessionManager) {
        const ctx = sessionManager.getContext();
        if (ctx) {
          workspaceId = ctx.workspace_id as string | undefined;
          projectId = projectId || ctx.project_id as string | undefined;
        }
      }
      
      const result = await client.getContextWithBudget({
        query: input.query,
        workspace_id: workspaceId,
        project_id: projectId,
        max_tokens: input.max_tokens,
        include_decisions: input.include_decisions,
        include_memory: input.include_memory,
        include_code: input.include_code,
      });
      
      const footer = `\n---\n📊 Token estimate: ${result.token_estimate}/${input.max_tokens} | Sources: ${result.sources.length}`;
      
      return {
        content: [{ type: 'text' as const, text: result.context + footer }],
        structuredContent: toStructured(result),
      };
    }
  );

  registerTool(
    'session_delta',
    {
      title: 'Get context changes since timestamp',
      description: `Get new context added since a specific timestamp.
Useful for efficient context synchronization without reloading everything.

Returns:
- Count of new decisions and memory events
- List of new items with titles and timestamps

Use case: AI can track what's new since last session_init.`,
      inputSchema: z.object({
        since: z.string().describe('ISO timestamp to get changes since (e.g., "2025-12-05T00:00:00Z")'),
        workspace_id: z.string().uuid().optional(),
        project_id: z.string().uuid().optional(),
        limit: z.number().optional().describe('Maximum items to return (default: 20)'),
      }),
    },
    async (input) => {
      // Get workspace_id from session context if not provided
      let workspaceId = input.workspace_id;
      let projectId = input.project_id;
      
      if (!workspaceId && sessionManager) {
        const ctx = sessionManager.getContext();
        if (ctx) {
          workspaceId = ctx.workspace_id as string | undefined;
          projectId = projectId || ctx.project_id as string | undefined;
        }
      }
      
      const result = await client.getContextDelta({
        workspace_id: workspaceId,
        project_id: projectId,
        since: input.since,
        limit: input.limit,
      });
      
      const summary = [
        `📈 Context changes since ${input.since}:`,
        `   New decisions: ${result.new_decisions}`,
        `   New memory events: ${result.new_memory}`,
        '',
        ...result.items.slice(0, 10).map(i => `• [${i.type}] ${i.title}`),
        result.items.length > 10 ? `   (+${result.items.length - 10} more)` : '',
      ].filter(Boolean).join('\n');
      
      return {
        content: [{ type: 'text' as const, text: summary }],
        structuredContent: toStructured(result),
      };
    }
  );

  registerTool(
    'context_smart',
    {
      title: 'Get smart context for user query',
      description: `**CALL THIS BEFORE EVERY AI RESPONSE** to get relevant context.

This is the KEY tool for token-efficient AI interactions. It:
1. Analyzes the user's message to understand what context is needed
2. Retrieves only relevant context in a minified, token-efficient format
3. Replaces the need to include full chat history in prompts

Format options:
- 'minified': Ultra-compact D:decision|P:preference|M:memory (default, ~200 tokens)
- 'readable': Line-separated with labels
- 'structured': JSON-like grouped format

Type codes: W=Workspace, P=Project, D=Decision, M=Memory, I=Insight, T=Task, L=Lesson

Example usage:
1. User asks "how should I implement auth?"
2. AI calls context_smart(user_message="how should I implement auth?")
3. Gets: "W:Maker|P:contextstream|D:Use JWT for auth|D:No session cookies|M:Auth API at /auth/..."
4. AI responds with relevant context already loaded

This saves ~80% tokens compared to including full chat history.`,
      inputSchema: z.object({
        user_message: z.string().describe('The user message to analyze and get context for'),
        workspace_id: z.string().uuid().optional(),
        project_id: z.string().uuid().optional(),
        max_tokens: z.number().optional().describe('Maximum tokens for context (default: 800)'),
        format: z.enum(['minified', 'readable', 'structured']).optional().describe('Context format (default: minified)'),
      }),
    },
    async (input) => {
      // Mark that context_smart has been called in this session
      if (sessionManager) {
        sessionManager.markContextSmartCalled();
      }

      // Get workspace_id from session context if not provided
      let workspaceId = input.workspace_id;
      let projectId = input.project_id;

      if (!workspaceId && sessionManager) {
        const ctx = sessionManager.getContext();
        if (ctx) {
          workspaceId = ctx.workspace_id as string | undefined;
          projectId = projectId || ctx.project_id as string | undefined;
        }
      }

      const result = await client.getSmartContext({
        user_message: input.user_message,
        workspace_id: workspaceId,
        project_id: projectId,
        max_tokens: input.max_tokens,
        format: input.format,
      });

      // Return context directly for easy inclusion in AI prompts
      const footer = `\n---\n🎯 ${result.sources_used} sources | ~${result.token_estimate} tokens | format: ${result.format}`;

      return {
        content: [{ type: 'text' as const, text: result.context + footer }],
        structuredContent: toStructured(result),
      };
    }
  );
}
