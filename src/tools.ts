import { z, type ZodRawShape } from 'zod';
import * as fs from 'node:fs';
import * as path from 'node:path';
import type { McpServer } from '@modelcontextprotocol/sdk/server/mcp.js';
import type { MessageExtraInfo, ToolAnnotations } from '@modelcontextprotocol/sdk/types.js';
import type { ContextStreamClient } from './client.js';
import { readFilesFromDirectory, readAllFilesInBatches, countIndexableFiles } from './files.js';
import { SessionManager } from './session-manager.js';
import { getAvailableEditors, generateRuleContent, generateAllRuleFiles } from './rules-templates.js';
import { VERSION } from './version.js';
import { generateToolCatalog, getCoreToolsHint, type CatalogFormat } from './tool-catalog.js';
import { getAuthOverride, runWithAuthOverride, type AuthOverride } from './auth-context.js';

type StructuredContent = { [x: string]: unknown } | undefined;
type ToolTextResult = {
  content: Array<{ type: 'text'; text: string }>;
  structuredContent?: StructuredContent;
  isError?: boolean;
};

const LESSON_DEDUP_WINDOW_MS = 2 * 60 * 1000;
const recentLessonCaptures = new Map<string, number>();

const DEFAULT_PARAM_DESCRIPTIONS: Record<string, string> = {
  api_key: 'ContextStream API key.',
  apiKey: 'ContextStream API key.',
  jwt: 'ContextStream JWT for authentication.',
  workspace_id: 'Workspace ID (UUID).',
  workspaceId: 'Workspace ID (UUID).',
  project_id: 'Project ID (UUID).',
  projectId: 'Project ID (UUID).',
  node_id: 'Node ID (UUID).',
  event_id: 'Event ID (UUID).',
  reminder_id: 'Reminder ID (UUID).',
  folder_path: 'Absolute path to the local folder.',
  file_path: 'Filesystem path to the file.',
  path: 'Filesystem path.',
  name: 'Name for the resource.',
  title: 'Short descriptive title.',
  description: 'Short description.',
  content: 'Full content/body.',
  query: 'Search query string.',
  limit: 'Maximum number of results to return.',
  page: 'Page number for pagination.',
  page_size: 'Results per page.',
  include_decisions: 'Include related decisions.',
  include_related: 'Include related context.',
  include_transitive: 'Include transitive dependencies.',
  max_depth: 'Maximum traversal depth.',
  since: 'ISO 8601 timestamp to query changes since.',
  remind_at: 'ISO 8601 datetime for the reminder.',
  priority: 'Priority level.',
  recurrence: 'Recurrence pattern (daily, weekly, monthly).',
  keywords: 'Keywords for matching.',
  overwrite: 'Allow overwriting existing files on disk.',
  write_to_disk: 'Write ingested files to disk before indexing.',
  await_indexing: 'Wait for indexing to finish before returning.',
  auto_index: 'Automatically index on creation.',
  session_id: 'Session identifier.',
  context_hint: 'User message used to fetch relevant context.',
  context: 'Context to match relevant reminders.',
};

const uuidSchema = z.string().uuid();

function normalizeUuid(value?: string): string | undefined {
  if (!value) return undefined;
  return uuidSchema.safeParse(value).success ? value : undefined;
}

const WRITE_VERBS = new Set([
  'create',
  'update',
  'delete',
  'ingest',
  'index',
  'capture',
  'remember',
  'associate',
  'bootstrap',
  'snooze',
  'complete',
  'dismiss',
  'generate',
  'sync',
  'publish',
  'set',
  'add',
  'remove',
  'revoke',
  'feedback',
  'upload',
  'compress',
  'init',
]);

const READ_ONLY_OVERRIDES = new Set([
  'session_tools',
  'context_smart',
  'session_summary',
  'session_recall',
  'session_get_user_context',
  'session_get_lessons',
  'session_smart_search',
  'session_delta',
  'projects_list',
  'projects_get',
  'projects_overview',
  'projects_statistics',
  'projects_files',
  'projects_index_status',
  'workspaces_list',
  'workspaces_get',
  'memory_search',
  'memory_decisions',
  'decision_trace',
  'memory_get_event',
  'memory_list_events',
  'memory_list_nodes',
  'memory_summary',
  'memory_timeline',
  'graph_related',
  'graph_decisions',
  'graph_path',
  'graph_dependencies',
  'graph_call_path',
  'graph_impact',
  'graph_circular_dependencies',
  'graph_unused_code',
  'search_semantic',
  'search_hybrid',
  'search_keyword',
  'search_pattern',
  'reminders_list',
  'reminders_active',
  'auth_me',
  'mcp_server_version',
]);

const DESTRUCTIVE_VERBS = new Set(['delete', 'dismiss', 'remove', 'revoke', 'supersede']);

const OPEN_WORLD_PREFIXES = new Set(['github', 'slack', 'integrations']);

function humanizeKey(raw: string): string {
  const withSpaces = raw.replace(/([a-z])([A-Z])/g, '$1 $2').replace(/_/g, ' ');
  return withSpaces.toLowerCase();
}

function buildParamDescription(key: string, path: string[]): string {
  const normalized = key in DEFAULT_PARAM_DESCRIPTIONS ? key : key.toLowerCase();
  const parent = path[path.length - 1];

  if (parent === 'target') {
    if (key === 'id') return 'Target identifier (module path, function id, etc.).';
    if (key === 'type') return 'Target type (module, file, function, type, variable).';
  }

  if (parent === 'source') {
    if (key === 'id') return 'Source identifier (module path, function id, etc.).';
    if (key === 'type') return 'Source type (module, file, function, type, variable).';
  }

  if (DEFAULT_PARAM_DESCRIPTIONS[normalized]) {
    return DEFAULT_PARAM_DESCRIPTIONS[normalized];
  }

  if (normalized.endsWith('_id')) {
    return `ID for the ${humanizeKey(normalized.replace(/_id$/, ''))}.`;
  }

  if (normalized.startsWith('include_')) {
    return `Whether to include ${humanizeKey(normalized.replace('include_', ''))}.`;
  }

  if (normalized.startsWith('max_')) {
    return `Maximum ${humanizeKey(normalized.replace('max_', ''))}.`;
  }

  if (normalized.startsWith('min_')) {
    return `Minimum ${humanizeKey(normalized.replace('min_', ''))}.`;
  }

  return `Input parameter: ${humanizeKey(normalized)}.`;
}

function getDescription(schema: z.ZodTypeAny): string | undefined {
  const def = (schema as { _def?: { description?: string } })._def;
  if (def?.description && def.description.trim()) return def.description;
  return undefined;
}

function applyParamDescriptions(schema: z.ZodTypeAny, path: string[] = []): z.ZodTypeAny {
  if (!(schema instanceof z.ZodObject)) {
    return schema;
  }

  const shape = schema.shape;
  let changed = false;
  const nextShape: ZodRawShape = {};

  for (const [key, field] of Object.entries(shape) as Array<[string, z.ZodTypeAny]>) {
    let nextField: z.ZodTypeAny = field;
    const existingDescription = getDescription(field);

    if (field instanceof z.ZodObject) {
      const nested = applyParamDescriptions(field, [...path, key]);
      if (nested !== field) {
        nextField = nested;
        changed = true;
      }
    }

    if (existingDescription) {
      if (!getDescription(nextField)) {
        nextField = nextField.describe(existingDescription);
        changed = true;
      }
    } else {
      nextField = nextField.describe(buildParamDescription(key, path));
      changed = true;
    }

    nextShape[key] = nextField;
  }

  if (!changed) return schema;

  let nextSchema: z.ZodTypeAny = z.object(nextShape);
  const def = (schema as { _def?: { catchall?: z.ZodTypeAny; unknownKeys?: string } })._def;
  if (def?.catchall) nextSchema = (nextSchema as z.ZodObject<any>).catchall(def.catchall);
  if (def?.unknownKeys === 'passthrough') nextSchema = (nextSchema as z.ZodObject<any>).passthrough();
  if (def?.unknownKeys === 'strict') nextSchema = (nextSchema as z.ZodObject<any>).strict();

  return nextSchema;
}

function inferToolAnnotations(toolName: string): ToolAnnotations {
  const parts = toolName.split('_');
  const prefix = parts[0] || toolName;
  const readOnly = READ_ONLY_OVERRIDES.has(toolName) || !parts.some((part) => WRITE_VERBS.has(part));
  const destructive = readOnly ? false : parts.some((part) => DESTRUCTIVE_VERBS.has(part));
  const openWorld = OPEN_WORLD_PREFIXES.has(prefix);

  return {
    readOnlyHint: readOnly,
    destructiveHint: readOnly ? false : destructive,
    idempotentHint: readOnly,
    openWorldHint: openWorld,
  };
}

function normalizeHeaderValue(value: string | string[] | undefined): string | undefined {
  if (!value) return undefined;
  return Array.isArray(value) ? value[0] : value;
}

function resolveAuthOverride(extra?: MessageExtraInfo): AuthOverride | null {
  const token = extra?.authInfo?.token;
  const tokenType = extra?.authInfo?.extra?.tokenType;
  const headers = extra?.requestInfo?.headers;
  const existing = getAuthOverride();

  const workspaceId = normalizeHeaderValue(headers?.['x-contextstream-workspace-id']) ||
    normalizeHeaderValue(headers?.['x-workspace-id']);
  const projectId = normalizeHeaderValue(headers?.['x-contextstream-project-id']) ||
    normalizeHeaderValue(headers?.['x-project-id']);

  if (token) {
    if (tokenType === 'jwt') {
      return { jwt: token, workspaceId, projectId };
    }
    return { apiKey: token, workspaceId, projectId };
  }

  if (existing?.apiKey || existing?.jwt) {
    return {
      apiKey: existing.apiKey,
      jwt: existing.jwt,
      workspaceId: workspaceId ?? existing.workspaceId,
      projectId: projectId ?? existing.projectId,
    };
  }

  if (!workspaceId && !projectId) return null;

  return { workspaceId, projectId };
}

// Light toolset: Core session, project, and basic memory tools (~31 tools)
const LIGHT_TOOLSET = new Set<string>([
  // Core session tools (13)
  'session_init',
  'session_tools',
  'context_smart',
  'context_feedback',
  'session_summary',
  'session_capture',
  'session_capture_lesson',
  'session_get_lessons',
  'session_recall',
  'session_remember',
  'session_get_user_context',
  'session_smart_search',
  'session_compress',
  'session_delta',
  // Setup and configuration (3)
  'generate_editor_rules',
  'workspace_associate',
  'workspace_bootstrap',
  // Project management (5)
  'projects_create',
  'projects_list',
  'projects_get',
  'projects_overview',
  'projects_statistics',
  // Project indexing (4)
  'projects_ingest_local',
  'projects_index',
  'projects_index_status',
  'projects_files',
  // Memory basics (3)
  'memory_search',
  'memory_decisions',
  'memory_get_event',
  // Graph basics (2)
  'graph_related',
  'graph_decisions',
  // Reminders (2)
  'reminders_list',
  'reminders_active',
  // Utility (2)
  'auth_me',
  'mcp_server_version',
]);

// Standard toolset: Balanced set for most users (default) - ~53 tools
const STANDARD_TOOLSET = new Set<string>([
  // Core session tools (13)
  'session_init',
  'session_tools',
  'context_smart',
  'session_summary',
  'session_capture',
  'session_capture_lesson',
  'session_get_lessons',
  'session_recall',
  'session_remember',
  'session_get_user_context',
  'session_smart_search',
  'session_compress',
  'session_delta',
  // Setup and configuration (3)
  'generate_editor_rules',
  'workspace_associate',
  'workspace_bootstrap',
  // Workspace management (2)
  'workspaces_list',
  'workspaces_get',
  // Project management (6)
  'projects_create',
  'projects_list',
  'projects_get',
  'projects_overview',
  'projects_statistics',
  'projects_update',
  // Project indexing (4)
  'projects_ingest_local',
  'projects_index',
  'projects_index_status',
  'projects_files',
  // Memory events (9)
  'memory_search',
  'memory_decisions',
  'decision_trace',
  'memory_create_event',
  'memory_list_events',
  'memory_get_event',
  'memory_update_event',
  'memory_delete_event',
  'memory_timeline',
  'memory_summary',
  // Memory nodes (2)
  'memory_create_node',
  'memory_list_nodes',
  // Knowledge graph analysis (8)
  'graph_related',
  'graph_decisions',
  'graph_path',
  'graph_dependencies',
  'graph_call_path',
  'graph_impact',
  'graph_circular_dependencies',
  'graph_unused_code',
  'graph_ingest',
  // Search (3)
  'search_semantic',
  'search_hybrid',
  'search_keyword',
  // Reminders (6)
  'reminders_list',
  'reminders_active',
  'reminders_create',
  'reminders_snooze',
  'reminders_complete',
  'reminders_dismiss',
  // Utility (2)
  'auth_me',
  'mcp_server_version',
]);

// Complete toolset: All tools (resolved as null allowlist)
// Includes: workspaces, projects, memory, knowledge graph, AI, integrations

const TOOLSET_ALIASES: Record<string, Set<string> | null> = {
  // Light mode - minimal, fastest
  light: LIGHT_TOOLSET,
  minimal: LIGHT_TOOLSET,
  // Standard mode - balanced (default)
  standard: STANDARD_TOOLSET,
  core: STANDARD_TOOLSET,
  essential: STANDARD_TOOLSET,
  // Complete mode - all tools
  complete: null,
  full: null,
  all: null,
};

function parseToolList(raw: string): Set<string> {
  return new Set(
    raw
      .split(',')
      .map((tool) => tool.trim())
      .filter(Boolean)
  );
}

function resolveToolFilter(): { allowlist: Set<string> | null; source: string } {
  const allowlistRaw = process.env.CONTEXTSTREAM_TOOL_ALLOWLIST;
  if (allowlistRaw) {
    const allowlist = parseToolList(allowlistRaw);
    if (allowlist.size === 0) {
      console.error('[ContextStream] CONTEXTSTREAM_TOOL_ALLOWLIST is empty; using standard toolset.');
      return { allowlist: STANDARD_TOOLSET, source: 'standard' };
    }
    return { allowlist, source: 'allowlist' };
  }

  const toolsetRaw = process.env.CONTEXTSTREAM_TOOLSET;
  if (!toolsetRaw) {
    // Default to standard toolset
    return { allowlist: STANDARD_TOOLSET, source: 'standard' };
  }

  const key = toolsetRaw.trim().toLowerCase();
  if (key in TOOLSET_ALIASES) {
    const resolved = TOOLSET_ALIASES[key];
    // null means complete/full toolset
    if (resolved === null) {
      return { allowlist: null, source: 'complete' };
    }
    return { allowlist: resolved, source: key };
  }

  console.error(`[ContextStream] Unknown CONTEXTSTREAM_TOOLSET "${toolsetRaw}". Using standard toolset.`);
  return { allowlist: STANDARD_TOOLSET, source: 'standard' };
}

function formatContent(data: unknown) {
  return JSON.stringify(data, null, 2);
}

function toStructured(data: unknown): StructuredContent {
  if (data && typeof data === 'object' && !Array.isArray(data)) {
    return data as { [x: string]: unknown };
  }
  return undefined;
}

function readStatNumber(payload: unknown, key: string): number | undefined {
  if (!payload || typeof payload !== 'object') return undefined;
  const direct = (payload as Record<string, unknown>)[key];
  if (typeof direct === 'number') return direct;
  const nested = (payload as Record<string, unknown>).data;
  if (nested && typeof nested === 'object') {
    const nestedValue = (nested as Record<string, unknown>)[key];
    if (typeof nestedValue === 'number') return nestedValue;
  }
  return undefined;
}

function estimateGraphIngestMinutes(stats: unknown): { min: number; max: number; basis?: string } | null {
  const totalFiles = readStatNumber(stats, 'total_files');
  const totalLines = readStatNumber(stats, 'total_lines');
  if (!totalFiles && !totalLines) return null;

  const fileScore = totalFiles ? totalFiles / 1000 : 0;
  const lineScore = totalLines ? totalLines / 50000 : 0;
  const sizeScore = Math.max(fileScore, lineScore);

  const minMinutes = Math.min(45, Math.max(1, Math.round(1 + sizeScore * 1.5)));
  const maxMinutes = Math.min(60, Math.max(minMinutes + 1, Math.round(2 + sizeScore * 3)));

  const basisParts = [];
  if (totalFiles) basisParts.push(`${totalFiles.toLocaleString()} files`);
  if (totalLines) basisParts.push(`${totalLines.toLocaleString()} lines`);

  return {
    min: minMinutes,
    max: maxMinutes,
    basis: basisParts.length > 0 ? basisParts.join(' / ') : undefined,
  };
}

function normalizeLessonField(value: string) {
  return value.trim().toLowerCase().replace(/\s+/g, ' ');
}

function buildLessonSignature(input: {
  title: string;
  category: string;
  trigger: string;
  impact: string;
  prevention: string;
}, workspaceId: string, projectId?: string) {
  return [
    workspaceId,
    projectId || 'global',
    input.category,
    input.title,
    input.trigger,
    input.impact,
    input.prevention,
  ].map(normalizeLessonField).join('|');
}

function isDuplicateLessonCapture(signature: string) {
  const now = Date.now();
  for (const [key, ts] of recentLessonCaptures) {
    if (now - ts > LESSON_DEDUP_WINDOW_MS) {
      recentLessonCaptures.delete(key);
    }
  }
  const last = recentLessonCaptures.get(signature);
  if (last && now - last < LESSON_DEDUP_WINDOW_MS) {
    recentLessonCaptures.set(signature, now);
    return true;
  }
  recentLessonCaptures.set(signature, now);
  return false;
}

export function registerTools(server: McpServer, client: ContextStreamClient, sessionManager?: SessionManager) {
  const upgradeUrl = process.env.CONTEXTSTREAM_UPGRADE_URL || 'https://contextstream.io/pricing';
  const toolFilter = resolveToolFilter();
  const toolAllowlist = toolFilter.allowlist;
  if (toolAllowlist) {
    const source = toolFilter.source;
    const hint = source === 'light'
      ? ' Set CONTEXTSTREAM_TOOLSET=standard or complete for more tools.'
      : source === 'standard'
        ? ' Set CONTEXTSTREAM_TOOLSET=complete for all tools.'
        : '';
    console.error(`[ContextStream] Toolset: ${source} (${toolAllowlist.size} tools).${hint}`);
  } else {
    console.error(`[ContextStream] Toolset: complete (all tools).`);
  }
  const defaultProTools = new Set<string>([
    // AI endpoints (typically paid/credit-metered)
    'ai_context',
    'ai_enhanced_context',
    'ai_context_budget',
    'ai_embeddings',
    'ai_plan',
    'ai_tasks',
    // Slack integration tools
    'slack_stats',
    'slack_channels',
    'slack_contributors',
    'slack_activity',
    'slack_discussions',
    'slack_search',
    'slack_sync_users',
    // GitHub integration tools
    'github_stats',
    'github_repos',
    'github_contributors',
    'github_activity',
    'github_issues',
    'github_search',
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

  function getToolAccessLabel(toolName: string): 'Free' | 'PRO' | 'Pro (Graph-Lite)' | 'Elite/Team (Full Graph)' {
    const graphTier = graphToolTiers.get(toolName);
    if (graphTier === 'lite') return 'Pro (Graph-Lite)';
    if (graphTier === 'full') return 'Elite/Team (Full Graph)';
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

  const graphToolTiers = new Map<string, 'lite' | 'full'>([
    ['graph_dependencies', 'lite'],
    ['graph_impact', 'lite'],
    ['graph_related', 'full'],
    ['graph_decisions', 'full'],
    ['graph_path', 'full'],
    ['graph_call_path', 'full'],
    ['graph_circular_dependencies', 'full'],
    ['graph_unused_code', 'full'],
    ['graph_ingest', 'full'],
    ['graph_contradictions', 'full'],
  ]);

  const graphLiteMaxDepth = 1;

  function normalizeGraphTargetType(value: unknown): string {
    return String(value ?? '').trim().toLowerCase();
  }

  function isModuleTargetType(value: string): boolean {
    return value === 'module' || value === 'file' || value === 'path';
  }

  function graphLiteConstraintError(toolName: string, detail: string): ToolTextResult {
    return errorResult(
      [
        `Access denied: \`${toolName}\` is limited to Graph-Lite (module-level, 1-hop queries).`,
        detail,
        `Upgrade to Elite or Team for full graph access: ${upgradeUrl}`,
      ].join('\n')
    );
  }

  async function gateIfGraphTool(toolName: string, input?: any): Promise<ToolTextResult | null> {
    const requiredTier = graphToolTiers.get(toolName);
    if (!requiredTier) return null;

    const graphTier = await client.getGraphTier();

    if (graphTier === 'full') return null;

    if (graphTier === 'lite') {
      if (requiredTier === 'full') {
        return errorResult(
          [
            `Access denied: \`${toolName}\` requires Elite or Team (Full Graph).`,
            'Pro includes Graph-Lite (module-level dependencies and 1-hop impact only).',
            `Upgrade: ${upgradeUrl}`,
          ].join('\n')
        );
      }

      if (toolName === 'graph_dependencies') {
        const targetType = normalizeGraphTargetType(input?.target?.type);
        if (!isModuleTargetType(targetType)) {
          return graphLiteConstraintError(
            toolName,
            'Set target.type to module, file, or path.'
          );
        }
        if (typeof input?.max_depth === 'number' && input.max_depth > graphLiteMaxDepth) {
          return graphLiteConstraintError(
            toolName,
            `Set max_depth to ${graphLiteMaxDepth} or lower.`
          );
        }
        if (input?.include_transitive === true) {
          return graphLiteConstraintError(
            toolName,
            'Set include_transitive to false.'
          );
        }
      }

      if (toolName === 'graph_impact') {
        const targetType = normalizeGraphTargetType(input?.target?.type);
        if (!isModuleTargetType(targetType)) {
          return graphLiteConstraintError(
            toolName,
            'Set target.type to module, file, or path.'
          );
        }
        if (typeof input?.max_depth === 'number' && input.max_depth > graphLiteMaxDepth) {
          return graphLiteConstraintError(
            toolName,
            `Set max_depth to ${graphLiteMaxDepth} or lower.`
          );
        }
      }

      return null;
    }

    return errorResult(
      [
        `Access denied: \`${toolName}\` requires ContextStream Pro (Graph-Lite) or Elite/Team (Full Graph).`,
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
    handler: (input: T, extra?: MessageExtraInfo) => Promise<R>
  ): (input: T, extra?: MessageExtraInfo) => Promise<R> {
    if (!sessionManager) {
      return async (input: T, extra?: MessageExtraInfo): Promise<R> => {
        const authOverride = resolveAuthOverride(extra);
        return runWithAuthOverride(authOverride, async () => handler(input, extra));
      };
    }

    return async (input: T, extra?: MessageExtraInfo): Promise<R> => {
      const authOverride = resolveAuthOverride(extra);

      return runWithAuthOverride(authOverride, async () => {
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
        const result = await handler(input, extra);

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
      });
    };
  }

  /**
   * Helper to register a tool with auto-context wrapper applied.
   * This is a drop-in replacement for server.registerTool that adds auto-context.
   */
  function registerTool<T extends z.ZodType>(
    name: string,
    config: { title: string; description: string; inputSchema: T },
    handler: (input: z.infer<T>, extra?: MessageExtraInfo) => Promise<ToolTextResult>
  ) {
    if (toolAllowlist && !toolAllowlist.has(name)) {
      return;
    }
    const accessLabel = getToolAccessLabel(name);
    const showUpgrade = accessLabel !== 'Free';
    const labeledConfig = {
      ...config,
      title: `${config.title} (${accessLabel})`,
      description: `${config.description}\n\nAccess: ${accessLabel}${showUpgrade ? ` (upgrade: ${upgradeUrl})` : ''}`,
    };
    const annotatedConfig = {
      ...labeledConfig,
      inputSchema: labeledConfig.inputSchema ? applyParamDescriptions(labeledConfig.inputSchema) : undefined,
      annotations: {
        ...inferToolAnnotations(name),
        ...(labeledConfig as { annotations?: ToolAnnotations }).annotations,
      },
    };

    // Wrap handler with error handling to ensure proper serialization
    const safeHandler = async (input: z.infer<T>, extra?: MessageExtraInfo) => {
      try {
        const gated = await gateIfProTool(name);
        if (gated) return gated;

        return await handler(input, extra);
      } catch (error: any) {
        // Convert error to a properly serializable format
        const errorMessage = error?.message || String(error);
        const errorDetails = error?.body || error?.details || null;
        const errorCode = error?.code || error?.status || 'UNKNOWN_ERROR';

        const isPlanLimit =
          String(errorCode).toUpperCase() === 'FORBIDDEN' &&
          String(errorMessage).toLowerCase().includes('plan limit reached');
        const upgradeHint = isPlanLimit ? `\nUpgrade: ${upgradeUrl}` : '';

        // Return structured error response instead of throwing
        const errorPayload = {
          success: false,
          error: {
            code: errorCode,
            message: errorMessage,
            details: errorDetails,
          },
        };
        const errorText = `[${errorCode}] ${errorMessage}${upgradeHint}${errorDetails ? `: ${JSON.stringify(errorDetails)}` : ''}`;
        return {
          content: [{ type: 'text' as const, text: errorText }],
          structuredContent: errorPayload,
          isError: true,
        };
      }
    };
    
    server.registerTool(
      name,
      annotatedConfig,
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
    const normalizedExplicit = normalizeUuid(explicitWorkspaceId);
    if (normalizedExplicit) return normalizedExplicit;
    const ctx = sessionManager?.getContext();
    return normalizeUuid(typeof ctx?.workspace_id === 'string' ? (ctx.workspace_id as string) : undefined);
  }

  function resolveProjectId(explicitProjectId?: string): string | undefined {
    const normalizedExplicit = normalizeUuid(explicitProjectId);
    if (normalizedExplicit) return normalizedExplicit;
    const ctx = sessionManager?.getContext();
    return normalizeUuid(typeof ctx?.project_id === 'string' ? (ctx.project_id as string) : undefined);
  }

  async function validateReadableDirectory(inputPath: string): Promise<{ ok: true; resolvedPath: string } | { ok: false; error: string }> {
    const resolvedPath = path.resolve(inputPath);
    let stats: fs.Stats;
    try {
      stats = await fs.promises.stat(resolvedPath);
    } catch (error: any) {
      if (error?.code === 'ENOENT') {
        return { ok: false, error: `Error: path does not exist: ${inputPath}` };
      }
      return {
        ok: false,
        error: `Error: unable to access path: ${inputPath}${error?.message ? ` (${error.message})` : ''}`,
      };
    }

    if (!stats.isDirectory()) {
      return { ok: false, error: `Error: path is not a directory: ${inputPath}` };
    }

    try {
      await fs.promises.access(resolvedPath, fs.constants.R_OK | fs.constants.X_OK);
    } catch (error: any) {
      return {
        ok: false,
        error: `Error: path is not readable: ${inputPath}${error?.code ? ` (${error.code})` : ''}`,
      };
    }

    return { ok: true, resolvedPath };
  }

  // Auth
  registerTool(
    'mcp_server_version',
    {
      title: 'Get MCP server version',
      description: 'Return the running ContextStream MCP server package version',
      inputSchema: z.object({}),
    },
    async () => {
      const result = { name: 'contextstream-mcp', version: VERSION };
      return { content: [{ type: 'text' as const, text: formatContent(result) }], structuredContent: toStructured(result) };
    }
  );

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
      description: 'List accessible workspaces (paginated list: items, total, page, per_page, has_next, has_prev).',
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
      description: 'Create a new workspace (returns ApiResponse with created workspace in data).',
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
      // Normalize response to match {success, data, error, metadata} structure
      const normalized = result || { success: true, data: { id: input.workspace_id, deleted: true }, error: null, metadata: {} };
      return { content: [{ type: 'text' as const, text: formatContent(normalized) }], structuredContent: toStructured(normalized) };
    }
  );

  // Projects
  registerTool(
    'projects_list',
    {
      title: 'List projects',
      description: 'List projects (optionally by workspace; paginated list: items, total, page, per_page, has_next, has_prev).',
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
      description: `Create a new project within a workspace.
Use this when you need to create a project for a specific folder/codebase.
If workspace_id is not provided, uses the current session's workspace.
Optionally associates a local folder and generates AI editor rules.

Access: Free`,
      inputSchema: z.object({
        name: z.string().describe('Project name'),
        description: z.string().optional().describe('Project description'),
        workspace_id: z.string().uuid().optional().describe('Workspace ID (uses current session workspace if not provided)'),
        folder_path: z.string().optional().describe('Optional: Local folder path to associate with this project'),
        generate_editor_rules: z.boolean().optional().describe('Generate AI editor rules in folder_path (requires folder_path)'),
      }),
    },
    async (input) => {
      // Resolve workspace ID from session if not provided
      const workspaceId = resolveWorkspaceId(input.workspace_id);
      if (!workspaceId) {
        return errorResult('Error: workspace_id is required. Please call session_init first or provide workspace_id explicitly.');
      }

      // Create the project
      const result = await client.createProject({
        name: input.name,
        description: input.description,
        workspace_id: workspaceId,
      });

      const projectData = result as { id?: string; name?: string };
      let rulesGenerated: string[] = [];

      // If folder_path provided, associate it with the project
      if (input.folder_path && projectData.id) {
        try {
          // Write project config to folder
          const configDir = path.join(input.folder_path, '.contextstream');
          const configPath = path.join(configDir, 'config.json');

          if (!fs.existsSync(configDir)) {
            fs.mkdirSync(configDir, { recursive: true });
          }

          const config = {
            workspace_id: workspaceId,
            project_id: projectData.id,
            project_name: input.name,
          };
          fs.writeFileSync(configPath, JSON.stringify(config, null, 2));

          // Generate editor rules if requested
          if (input.generate_editor_rules) {
            for (const editor of getAvailableEditors()) {
              const rule = generateRuleContent(editor, {
                workspaceId: workspaceId,
                projectName: input.name,
              });
              if (rule) {
                const filePath = path.join(input.folder_path, rule.filename);
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
                  } else if (!existingContent.includes('ContextStream')) {
                    fs.writeFileSync(filePath, existingContent + '\n\n' + rule.content);
                    rulesGenerated.push(rule.filename + ' (appended)');
                  }
                } catch {
                  // Ignore errors for individual files
                }
              }
            }
          }
        } catch (err: unknown) {
          // Log but don't fail - project was created successfully
          console.error('[ContextStream] Failed to write project config:', err);
        }
      }

      const response = {
        ...(result && typeof result === 'object' ? result : {}),
        folder_path: input.folder_path,
        config_written: input.folder_path ? true : undefined,
        editor_rules_generated: rulesGenerated.length > 0 ? rulesGenerated : undefined,
      };

      return { content: [{ type: 'text' as const, text: formatContent(response) }], structuredContent: toStructured(response) };
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
      // Normalize response to match {success, data, error, metadata} structure
      const normalized = result || { success: true, data: { id: input.project_id, deleted: true }, error: null, metadata: {} };
      return { content: [{ type: 'text' as const, text: formatContent(normalized) }], structuredContent: toStructured(normalized) };
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
        provenance: z.object({
          repo: z.string().optional(),
          branch: z.string().optional(),
          commit_sha: z.string().optional(),
          pr_url: z.string().url().optional(),
          issue_url: z.string().url().optional(),
          slack_thread_url: z.string().url().optional(),
        }).optional(),
        code_refs: z.array(
          z.object({
            file_path: z.string(),
            symbol_id: z.string().optional(),
            symbol_name: z.string().optional(),
          })
        ).optional(),
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
        category: z.string().optional().describe('Optional category filter. If not specified, returns all decisions regardless of category.'),
        limit: z.number().optional(),
      }),
    },
    async (input) => {
      const result = await client.memoryDecisions(input);
      return { content: [{ type: 'text' as const, text: formatContent(result) }], structuredContent: toStructured(result) };
    }
  );

  registerTool(
    'decision_trace',
    {
      title: 'Decision trace',
      description: 'Trace decisions to provenance, code references, and impact',
      inputSchema: z.object({
        query: z.string(),
        workspace_id: z.string().uuid().optional(),
        project_id: z.string().uuid().optional(),
        limit: z.number().optional(),
        include_impact: z.boolean().optional().describe('Include impact analysis when graph data is available'),
      }),
    },
    async (input) => {
      const result = await client.decisionTrace(input);
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
      const gate = await gateIfGraphTool('graph_related', input);
      if (gate) return gate;
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
        source_id: z.string().uuid(),
        target_id: z.string().uuid(),
        workspace_id: z.string().uuid().optional(),
        project_id: z.string().uuid().optional(),
      }),
    },
    async (input) => {
      const gate = await gateIfGraphTool('graph_path', input);
      if (gate) return gate;
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
      const gate = await gateIfGraphTool('graph_decisions', input);
      if (gate) return gate;
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
        target: z.object({
          type: z.string().describe('Code element type. Accepted values: module (aliases: file, path), function (alias: method), type (aliases: struct, enum, trait, class), variable (aliases: data, const, constant). For knowledge/memory nodes, use graph_path with UUID ids instead.'),
          id: z.string().describe('Element identifier. For module type, use file path (e.g., "src/auth.rs"). For function/type/variable, use the element id.'),
        }),
        max_depth: z.number().optional(),
        include_transitive: z.boolean().optional(),
      }),
    },
    async (input) => {
      const gate = await gateIfGraphTool('graph_dependencies', input);
      if (gate) return gate;
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
        source: z.object({
          type: z.string().describe('Must be "function" (alias: method). Only function types are supported for call path analysis. For knowledge/memory nodes, use graph_path with UUID ids instead.'),
          id: z.string().describe('Source function identifier.'),
        }),
        target: z.object({
          type: z.string().describe('Must be "function" (alias: method). Only function types are supported for call path analysis.'),
          id: z.string().describe('Target function identifier.'),
        }),
        max_depth: z.number().optional(),
      }),
    },
    async (input) => {
      const gate = await gateIfGraphTool('graph_call_path', input);
      if (gate) return gate;
      const result = await client.graphCallPath(input);
      return { content: [{ type: 'text' as const, text: formatContent(result) }], structuredContent: toStructured(result) };
    }
  );

  registerTool(
    'graph_impact',
    {
      title: 'Impact analysis',
      description: 'Analyze impact of a target node',
      inputSchema: z.object({
        target: z.object({
          type: z.string().describe('Code element type. Accepted values: module (aliases: file, path), function (alias: method), type (aliases: struct, enum, trait, class), variable (aliases: data, const, constant). For knowledge/memory nodes, use graph_path with UUID ids instead.'),
          id: z.string().describe('Element identifier. For module type, use file path (e.g., "src/auth.rs"). For function/type/variable, use the element id.'),
        }),
        max_depth: z.number().optional(),
      }),
    },
    async (input) => {
      const gate = await gateIfGraphTool('graph_impact', input);
      if (gate) return gate;
      const result = await client.graphImpact(input);
      return { content: [{ type: 'text' as const, text: formatContent(result) }], structuredContent: toStructured(result) };
    }
  );

  registerTool(
    'graph_ingest',
    {
      title: 'Ingest code graph',
      description: 'Build and persist the dependency graph for a project. Runs async by default (wait=false) and can take a few minutes for larger repos.',
      inputSchema: z.object({
        project_id: z.string().uuid().optional(),
        wait: z.boolean().optional().describe('If true, wait for ingestion to finish before returning. Defaults to false (async).'),
      }),
    },
    async (input) => {
      const gate = await gateIfGraphTool('graph_ingest', input);
      if (gate) return gate;
      const projectId = resolveProjectId(input.project_id);
      if (!projectId) {
        return errorResult('Error: project_id is required. Please call session_init first or provide project_id explicitly.');
      }

      const wait = input.wait ?? false;
      let estimate: { min: number; max: number; basis?: string } | null = null;

      try {
        const stats = await client.projectStatistics(projectId);
        estimate = estimateGraphIngestMinutes(stats);
      } catch (error) {
        console.error('[ContextStream] Failed to fetch project statistics for graph ingest estimate:', error);
      }

      const result = await client.graphIngest({ project_id: projectId, wait });
      const estimateText = estimate
        ? `Estimated time: ${estimate.min}-${estimate.max} min${estimate.basis ? ` (based on ${estimate.basis})` : ''}.`
        : 'Estimated time varies with repo size.';
      const note = `Graph ingestion is running ${wait ? 'synchronously' : 'asynchronously'} and can take a few minutes. ${estimateText}`;
      const structured = toStructured(result);
      const structuredContent = structured && typeof structured === 'object'
        ? { ...structured, wait, note, ...(estimate ? { estimate_minutes: { min: estimate.min, max: estimate.max }, estimate_basis: estimate.basis } : {}) }
        : { wait, note, ...(estimate ? { estimate_minutes: { min: estimate.min, max: estimate.max }, estimate_basis: estimate.basis } : {}) };

      return {
        content: [{
          type: 'text' as const,
          text: `${note}\n${formatContent(result)}`
        }],
        structuredContent
      };
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
        write_to_disk: z.boolean().optional().describe('When true, write files to disk under QA_FILE_WRITE_ROOT before indexing (for testing/QA)'),
        overwrite: z.boolean().optional().describe('Allow overwriting existing files when write_to_disk is enabled'),
      }),
    },
    async (input) => {
      const projectId = resolveProjectId(input.project_id);
      if (!projectId) {
        return errorResult('Error: project_id is required. Please call session_init first or provide project_id explicitly.');
      }

      const pathCheck = await validateReadableDirectory(input.path);
      if (!pathCheck.ok) {
        return errorResult(pathCheck.error);
      }

      // Quick check: does directory contain any indexable files?
      const fileCheck = await countIndexableFiles(pathCheck.resolvedPath, { maxFiles: 1 });
      if (fileCheck.count === 0) {
        return errorResult(
          `Error: no indexable files found in directory: ${input.path}. ` +
          `The directory may be empty or contain only ignored files/directories. ` +
          `Supported file types include: .ts, .js, .py, .rs, .go, .java, .md, .json, etc.`
        );
      }

      // Capture ingest options for passing to API
      const ingestOptions = {
        ...(input.write_to_disk !== undefined && { write_to_disk: input.write_to_disk }),
        ...(input.overwrite !== undefined && { overwrite: input.overwrite }),
      };

      // Start ingestion in background to avoid blocking the agent
      (async () => {
        try {
          let totalIndexed = 0;
          let batchCount = 0;

          console.error(`[ContextStream] Starting background ingestion for project ${projectId} from ${pathCheck.resolvedPath}`);

          for await (const batch of readAllFilesInBatches(pathCheck.resolvedPath, { batchSize: 50 })) {
            const result = await client.ingestFiles(projectId, batch, ingestOptions) as { data?: { files_indexed: number } };
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
        ...(input.write_to_disk && { write_to_disk: input.write_to_disk }),
        ...(input.overwrite && { overwrite: input.overwrite }),
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
      description: 'List content in a workspace (paginated list: items, total, page, per_page, has_next, has_prev).',
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
      description: 'Get a specific memory event by ID with FULL content (not truncated). Use this when you need the complete content of a memory event, not just the preview returned by search/recall.',
      inputSchema: z.object({ event_id: z.string().uuid().describe('The UUID of the memory event to retrieve') }),
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
      const gate = await gateIfGraphTool('graph_circular_dependencies', input);
      if (gate) return gate;
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
      const gate = await gateIfGraphTool('graph_unused_code', input);
      if (gate) return gate;
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
      const gate = await gateIfGraphTool('graph_contradictions', input);
      if (gate) return gate;
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
        allow_no_workspace: z.boolean().optional().describe('If true, allow session_init to return connected even if no workspace is resolved (workspace-level tools may not work).'),
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

      // Add compact tool reference to help AI know available tools
      result.tools_hint = getCoreToolsHint();

      // Mark session as initialized to prevent auto-init on subsequent tool calls
      if (sessionManager) {
        sessionManager.markInitialized(result);
      }

      const status = typeof result.status === 'string' ? (result.status as string) : '';
      const workspaceWarning = typeof (result as any).workspace_warning === 'string'
        ? ((result as any).workspace_warning as string)
        : '';

      let text = formatContent(result);

      if (status === 'requires_workspace_name') {
        const folderPath = typeof (result as any).folder_path === 'string'
          ? ((result as any).folder_path as string)
          : (typeof input.folder_path === 'string' ? input.folder_path : '');

        text = [
          'Action required: no workspaces found for this account.',
          'Ask the user for a name for the new workspace (recommended), then run `workspace_bootstrap`.',
          folderPath
            ? `Recommended: workspace_bootstrap(workspace_name: \"<name>\", folder_path: \"${folderPath}\")`
            : 'Recommended: workspace_bootstrap(workspace_name: \"<name>\", folder_path: \"<your repo folder>\")',
          '',
          'If you want to continue without a workspace for now, re-run:',
          folderPath
            ? `  session_init(folder_path: \"${folderPath}\", allow_no_workspace: true)`
            : '  session_init(folder_path: \"<your repo folder>\", allow_no_workspace: true)',
          '',
          '--- Raw Response ---',
          '',
          formatContent(result),
        ].join('\n');
      } else if (status === 'requires_workspace_selection') {
        const folderName = typeof (result as any).folder_name === 'string'
          ? ((result as any).folder_name as string)
          : (typeof input.folder_path === 'string' ? (path.basename(input.folder_path) || 'this folder') : 'this folder');

        const candidates = Array.isArray((result as any).workspace_candidates)
          ? ((result as any).workspace_candidates as Array<{ id?: string; name?: string; description?: string }>)
          : [];

        const lines: string[] = [];
        lines.push(`Action required: select a workspace for "${folderName}" (or create a new one).`);
        if (candidates.length > 0) {
          lines.push('');
          lines.push('Available workspaces:');
          candidates.slice(0, 25).forEach((w, i) => {
            const name = w.name || 'Untitled';
            const id = w.id ? ` (${w.id})` : '';
            const desc = w.description ? ` - ${w.description}` : '';
            lines.push(`  ${i + 1}. ${name}${id}${desc}`);
          });
        }
        lines.push('');
        lines.push('Then run `workspace_associate` with the selected workspace_id and your folder_path.');
        lines.push('');
        lines.push('If you want to continue without a workspace for now, re-run:');
        if (typeof input.folder_path === 'string' && input.folder_path) {
          lines.push(`  session_init(folder_path: \"${input.folder_path}\", allow_no_workspace: true)`);
        } else {
          lines.push('  session_init(folder_path: \"<your repo folder>\", allow_no_workspace: true)');
        }
        lines.push('');
        lines.push('--- Raw Response ---');
        lines.push('');
        lines.push(formatContent(result));
        text = lines.join('\n');
      } else if (workspaceWarning) {
        text = [`Warning: ${workspaceWarning}`, '', formatContent(result)].join('\n');
      }

      return { content: [{ type: 'text' as const, text }], structuredContent: toStructured(result) };
    }
  );

  registerTool(
    'session_tools',
    {
      title: 'Get available ContextStream tools',
      description: `Get an ultra-compact list of all available ContextStream MCP tools.
Use this when you need to know what tools are available without reading full descriptions.

Returns a token-efficient tool catalog (~120 tokens) organized by category.

Format options:
- 'grouped' (default): Category: tool(hint) tool(hint) - best for quick reference
- 'minimal': Category:tool|tool|tool - most compact
- 'full': Detailed list with descriptions

Example output (grouped):
Session: init(start-conv) smart(each-msg) capture(save) recall(find) remember(quick)
Search: semantic(meaning) hybrid(combo) keyword(exact)
Memory: events(crud) nodes(knowledge) search(find) decisions(choices)`,
      inputSchema: z.object({
        format: z.enum(['grouped', 'minimal', 'full']).optional().default('grouped')
          .describe('Output format: grouped (default, ~120 tokens), minimal (~80 tokens), or full (~200 tokens)'),
        category: z.string().optional()
          .describe('Filter to specific category: Session, Search, Memory, Knowledge, Graph, Workspace, Project, AI'),
      }),
    },
    async (input) => {
      const format = (input.format || 'grouped') as CatalogFormat;
      const catalog = generateToolCatalog(format, input.category);
      return {
        content: [{ type: 'text' as const, text: catalog }],
        structuredContent: { format, catalog },
      };
    }
  );

  registerTool(
    'session_get_user_context',
    {
      title: 'Get user context and preferences',
      description: `Retrieve user preferences, coding style, and persona from memory.
Use this to understand how the user likes to work and adapt your responses accordingly.`,
      inputSchema: z.object({
        workspace_id: z.string().optional().describe('Workspace ID (UUID). Invalid values are ignored.'),
      }),
    },
    async (input) => {
      const workspaceId = resolveWorkspaceId(input.workspace_id);
      const result = await client.getUserContext({ workspace_id: workspaceId });
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

      const folderName = path.basename(folderPath) || 'My Project';

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
        provenance: z.object({
          repo: z.string().optional(),
          branch: z.string().optional(),
          commit_sha: z.string().optional(),
          pr_url: z.string().url().optional(),
          issue_url: z.string().url().optional(),
          slack_thread_url: z.string().url().optional(),
        }).optional(),
        code_refs: z.array(
          z.object({
            file_path: z.string(),
            symbol_id: z.string().optional(),
            symbol_name: z.string().optional(),
          })
        ).optional(),
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

      const lessonSignature = buildLessonSignature({
        title: input.title,
        category: input.category,
        trigger: input.trigger,
        impact: input.impact,
        prevention: input.prevention,
      }, workspaceId, projectId);

      if (isDuplicateLessonCapture(lessonSignature)) {
        return {
          content: [{
            type: 'text' as const,
            text: `ℹ️ Duplicate lesson capture ignored: "${input.title}" was already recorded recently.`
          }],
          structuredContent: {
            deduped: true,
            title: input.title,
          },
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
        workspace_id: z.string().optional().describe('Workspace ID (UUID). Invalid values are ignored.'),
        project_id: z.string().optional().describe('Project ID (UUID). Invalid values are ignored.'),
        query: z.string().optional().describe('Search for relevant lessons (e.g., "git push images")'),
        category: z.enum(['workflow', 'code_quality', 'verification', 'communication', 'project_specific']).optional()
          .describe('Filter by category'),
        severity: z.enum(['low', 'medium', 'high', 'critical']).optional()
          .describe('Filter by minimum severity'),
        limit: z.number().default(10).describe('Maximum lessons to return'),
      }),
    },
    async (input) => {
      const workspaceId = resolveWorkspaceId(input.workspace_id);
      const projectId = resolveProjectId(input.project_id);

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
        await_indexing: z.boolean().optional().describe('If true, wait for indexing to complete before returning. This ensures the content is immediately searchable.'),
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

      const result = await client.sessionRemember({
        content: input.content,
        workspace_id: workspaceId,
        project_id: projectId,
        importance: input.importance,
        await_indexing: input.await_indexing,
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
        editors: z.array(z.enum(['codex', 'windsurf', 'cursor', 'cline', 'kilo', 'roo', 'claude', 'aider', 'all']))
          .optional()
          .describe('Which editors to generate rules for. Defaults to all.'),
        workspace_name: z.string().optional().describe('Workspace name to include in rules'),
        workspace_id: z.string().uuid().optional().describe('Workspace ID to include in rules'),
        project_name: z.string().optional().describe('Project name to include in rules'),
        additional_rules: z.string().optional().describe('Additional project-specific rules to append'),
        mode: z.enum(['minimal', 'full']).optional().describe('Rule verbosity mode (default: minimal)'),
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
          mode: input.mode,
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
      const versionNoticeLine = result.version_notice?.behind
        ? `\n[VERSION_NOTICE] current=${result.version_notice.current} latest=${result.version_notice.latest} upgrade="${result.version_notice.upgrade_command}"`
        : '';

      return {
        content: [{ type: 'text' as const, text: result.context + footer + versionNoticeLine }],
        structuredContent: toStructured(result),
      };
    }
  );

  registerTool(
    'context_feedback',
    {
      title: 'Submit smart context feedback',
      description: 'Send relevance feedback (relevant/irrelevant/pin) for context_smart items.',
      inputSchema: z.object({
        workspace_id: z.string().uuid().optional(),
        project_id: z.string().uuid().optional(),
        item_id: z.string().describe('Item ID returned by context_smart'),
        item_type: z.enum(['memory_event', 'knowledge_node', 'code_chunk']),
        feedback_type: z.enum(['relevant', 'irrelevant', 'pin']),
        query_text: z.string().optional(),
        metadata: z.record(z.any()).optional(),
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

      const result = await client.submitContextFeedback({
        ...input,
        workspace_id: workspaceId,
        project_id: projectId,
      });

      return {
        content: [{ type: 'text' as const, text: formatContent(result) }],
        structuredContent: toStructured(result),
      };
    }
  );

  // ============================================
  // Slack Integration Tools
  // ============================================

  registerTool(
    'slack_stats',
    {
      title: 'Slack overview stats',
      description: `Get Slack integration statistics and overview for a workspace.
Returns: total messages, threads, active users, channel stats, activity trends, and sync status.
Use this to understand Slack activity and engagement patterns.`,
      inputSchema: z.object({
        workspace_id: z.string().uuid().optional(),
        days: z.number().optional().describe('Number of days to include in stats (default: 30)'),
      }),
    },
    async (input) => {
      const workspaceId = resolveWorkspaceId(input.workspace_id);
      if (!workspaceId) {
        return errorResult('Error: workspace_id is required. Please call session_init first or provide workspace_id explicitly.');
      }

      const result = await client.slackStats({ workspace_id: workspaceId, days: input.days });
      return { content: [{ type: 'text' as const, text: formatContent(result) }], structuredContent: toStructured(result) };
    }
  );

  registerTool(
    'slack_channels',
    {
      title: 'List Slack channels',
      description: `Get synced Slack channels with statistics for a workspace.
Returns: channel names, message counts, thread counts, and last activity timestamps.`,
      inputSchema: z.object({
        workspace_id: z.string().uuid().optional(),
      }),
    },
    async (input) => {
      const workspaceId = resolveWorkspaceId(input.workspace_id);
      if (!workspaceId) {
        return errorResult('Error: workspace_id is required. Please call session_init first or provide workspace_id explicitly.');
      }

      const result = await client.slackChannels({ workspace_id: workspaceId });
      return { content: [{ type: 'text' as const, text: formatContent(result) }], structuredContent: toStructured(result) };
    }
  );

  registerTool(
    'slack_contributors',
    {
      title: 'Slack top contributors',
      description: `Get top Slack contributors for a workspace.
Returns: user profiles with message counts, sorted by activity level.`,
      inputSchema: z.object({
        workspace_id: z.string().uuid().optional(),
        limit: z.number().optional().describe('Maximum contributors to return (default: 20)'),
      }),
    },
    async (input) => {
      const workspaceId = resolveWorkspaceId(input.workspace_id);
      if (!workspaceId) {
        return errorResult('Error: workspace_id is required. Please call session_init first or provide workspace_id explicitly.');
      }

      const result = await client.slackContributors({ workspace_id: workspaceId, limit: input.limit });
      return { content: [{ type: 'text' as const, text: formatContent(result) }], structuredContent: toStructured(result) };
    }
  );

  registerTool(
    'slack_activity',
    {
      title: 'Slack activity feed',
      description: `Get recent Slack activity feed for a workspace.
Returns: messages with user info, reactions, replies, and timestamps.
Can filter by channel.`,
      inputSchema: z.object({
        workspace_id: z.string().uuid().optional(),
        limit: z.number().optional().describe('Maximum messages to return (default: 50)'),
        offset: z.number().optional().describe('Pagination offset'),
        channel_id: z.string().optional().describe('Filter by specific channel ID'),
      }),
    },
    async (input) => {
      const workspaceId = resolveWorkspaceId(input.workspace_id);
      if (!workspaceId) {
        return errorResult('Error: workspace_id is required. Please call session_init first or provide workspace_id explicitly.');
      }

      const result = await client.slackActivity({
        workspace_id: workspaceId,
        limit: input.limit,
        offset: input.offset,
        channel_id: input.channel_id,
      });
      return { content: [{ type: 'text' as const, text: formatContent(result) }], structuredContent: toStructured(result) };
    }
  );

  registerTool(
    'slack_discussions',
    {
      title: 'Slack key discussions',
      description: `Get high-engagement Slack discussions/threads for a workspace.
Returns: threads with high reply/reaction counts, sorted by engagement.
Useful for finding important conversations and decisions.`,
      inputSchema: z.object({
        workspace_id: z.string().uuid().optional(),
        limit: z.number().optional().describe('Maximum discussions to return (default: 20)'),
      }),
    },
    async (input) => {
      const workspaceId = resolveWorkspaceId(input.workspace_id);
      if (!workspaceId) {
        return errorResult('Error: workspace_id is required. Please call session_init first or provide workspace_id explicitly.');
      }

      const result = await client.slackDiscussions({ workspace_id: workspaceId, limit: input.limit });
      return { content: [{ type: 'text' as const, text: formatContent(result) }], structuredContent: toStructured(result) };
    }
  );

  registerTool(
    'slack_search',
    {
      title: 'Search Slack messages',
      description: `Search Slack messages for a workspace.
Returns: matching messages with channel, user, and engagement info.
Use this to find specific conversations or topics.`,
      inputSchema: z.object({
        workspace_id: z.string().uuid().optional(),
        q: z.string().describe('Search query'),
        limit: z.number().optional().describe('Maximum results (default: 50)'),
      }),
    },
    async (input) => {
      const workspaceId = resolveWorkspaceId(input.workspace_id);
      if (!workspaceId) {
        return errorResult('Error: workspace_id is required. Please call session_init first or provide workspace_id explicitly.');
      }

      const result = await client.slackSearch({ workspace_id: workspaceId, q: input.q, limit: input.limit });
      return { content: [{ type: 'text' as const, text: formatContent(result) }], structuredContent: toStructured(result) };
    }
  );

  registerTool(
    'slack_sync_users',
    {
      title: 'Sync Slack users',
      description: `Trigger a sync of Slack user profiles for a workspace.
This fetches the latest user info from Slack and updates local profiles.
Also auto-maps Slack users to ContextStream users by email.`,
      inputSchema: z.object({
        workspace_id: z.string().uuid().optional(),
      }),
    },
    async (input) => {
      const workspaceId = resolveWorkspaceId(input.workspace_id);
      if (!workspaceId) {
        return errorResult('Error: workspace_id is required. Please call session_init first or provide workspace_id explicitly.');
      }

      const result = await client.slackSyncUsers({ workspace_id: workspaceId });
      return {
        content: [{
          type: 'text' as const,
          text: `✅ Synced ${result.synced_users} Slack users, auto-mapped ${result.auto_mapped} by email.`
        }],
        structuredContent: toStructured(result)
      };
    }
  );

  // ============================================
  // GitHub Integration Tools
  // ============================================

  registerTool(
    'github_stats',
    {
      title: 'GitHub overview stats',
      description: `Get GitHub integration statistics and overview for a workspace.
Returns: total issues, PRs, releases, comments, repository stats, activity trends, and sync status.
Use this to understand GitHub activity and engagement patterns across synced repositories.`,
      inputSchema: z.object({
        workspace_id: z.string().uuid().optional(),
      }),
    },
    async (input) => {
      const workspaceId = resolveWorkspaceId(input.workspace_id);
      if (!workspaceId) {
        return errorResult('Error: workspace_id is required. Please call session_init first or provide workspace_id explicitly.');
      }

      const result = await client.githubStats({ workspace_id: workspaceId });
      return { content: [{ type: 'text' as const, text: formatContent(result) }], structuredContent: toStructured(result) };
    }
  );

  registerTool(
    'github_repos',
    {
      title: 'List GitHub repositories',
      description: `Get synced GitHub repositories with statistics for a workspace.
Returns: repository names with issue, PR, release, and comment counts, plus last activity timestamps.`,
      inputSchema: z.object({
        workspace_id: z.string().uuid().optional(),
      }),
    },
    async (input) => {
      const workspaceId = resolveWorkspaceId(input.workspace_id);
      if (!workspaceId) {
        return errorResult('Error: workspace_id is required. Please call session_init first or provide workspace_id explicitly.');
      }

      const result = await client.githubRepos({ workspace_id: workspaceId });
      return { content: [{ type: 'text' as const, text: formatContent(result) }], structuredContent: toStructured(result) };
    }
  );

  registerTool(
    'github_contributors',
    {
      title: 'GitHub top contributors',
      description: `Get top GitHub contributors for a workspace.
Returns: usernames with contribution counts, sorted by activity level.`,
      inputSchema: z.object({
        workspace_id: z.string().uuid().optional(),
        limit: z.number().optional().describe('Maximum contributors to return (default: 20)'),
      }),
    },
    async (input) => {
      const workspaceId = resolveWorkspaceId(input.workspace_id);
      if (!workspaceId) {
        return errorResult('Error: workspace_id is required. Please call session_init first or provide workspace_id explicitly.');
      }

      const result = await client.githubContributors({ workspace_id: workspaceId, limit: input.limit });
      return { content: [{ type: 'text' as const, text: formatContent(result) }], structuredContent: toStructured(result) };
    }
  );

  registerTool(
    'github_activity',
    {
      title: 'GitHub activity feed',
      description: `Get recent GitHub activity feed for a workspace.
Returns: issues, PRs, releases, and comments with details like state, author, labels.
Can filter by repository or type (issue, pull_request, release, comment).`,
      inputSchema: z.object({
        workspace_id: z.string().uuid().optional(),
        limit: z.number().optional().describe('Maximum items to return (default: 50)'),
        offset: z.number().optional().describe('Pagination offset'),
        repo: z.string().optional().describe('Filter by repository name'),
        type: z.enum(['issue', 'pull_request', 'release', 'comment']).optional().describe('Filter by item type'),
      }),
    },
    async (input) => {
      const workspaceId = resolveWorkspaceId(input.workspace_id);
      if (!workspaceId) {
        return errorResult('Error: workspace_id is required. Please call session_init first or provide workspace_id explicitly.');
      }

      const result = await client.githubActivity({
        workspace_id: workspaceId,
        limit: input.limit,
        offset: input.offset,
        repo: input.repo,
        type: input.type,
      });
      return { content: [{ type: 'text' as const, text: formatContent(result) }], structuredContent: toStructured(result) };
    }
  );

  registerTool(
    'github_issues',
    {
      title: 'GitHub issues and PRs',
      description: `Get GitHub issues and pull requests for a workspace.
Returns: issues/PRs with title, state, author, labels, comment count.
Can filter by state (open/closed) or repository.`,
      inputSchema: z.object({
        workspace_id: z.string().uuid().optional(),
        limit: z.number().optional().describe('Maximum items to return (default: 50)'),
        offset: z.number().optional().describe('Pagination offset'),
        state: z.enum(['open', 'closed']).optional().describe('Filter by state'),
        repo: z.string().optional().describe('Filter by repository name'),
      }),
    },
    async (input) => {
      const workspaceId = resolveWorkspaceId(input.workspace_id);
      if (!workspaceId) {
        return errorResult('Error: workspace_id is required. Please call session_init first or provide workspace_id explicitly.');
      }

      const result = await client.githubIssues({
        workspace_id: workspaceId,
        limit: input.limit,
        offset: input.offset,
        state: input.state,
        repo: input.repo,
      });
      return { content: [{ type: 'text' as const, text: formatContent(result) }], structuredContent: toStructured(result) };
    }
  );

  registerTool(
    'github_search',
    {
      title: 'Search GitHub content',
      description: `Search GitHub issues, PRs, and comments for a workspace.
Returns: matching items with repository, title, state, and content preview.
Use this to find specific issues, PRs, or discussions.`,
      inputSchema: z.object({
        workspace_id: z.string().uuid().optional(),
        q: z.string().describe('Search query'),
        limit: z.number().optional().describe('Maximum results (default: 50)'),
      }),
    },
    async (input) => {
      const workspaceId = resolveWorkspaceId(input.workspace_id);
      if (!workspaceId) {
        return errorResult('Error: workspace_id is required. Please call session_init first or provide workspace_id explicitly.');
      }

      const result = await client.githubSearch({ workspace_id: workspaceId, q: input.q, limit: input.limit });
      return { content: [{ type: 'text' as const, text: formatContent(result) }], structuredContent: toStructured(result) };
    }
  );

  registerTool(
    'github_knowledge',
    {
      title: 'GitHub extracted knowledge',
      description: `Get knowledge extracted from GitHub issues and PRs.
Returns: decisions, lessons, and insights automatically distilled from GitHub conversations.
This surfaces key decisions and learnings from your repository discussions.

Example queries:
- "What decisions were made about authentication?"
- "What lessons learned from production incidents?"
- "Show recent architectural decisions"`,
      inputSchema: z.object({
        workspace_id: z.string().uuid().optional(),
        limit: z.number().optional().describe('Maximum items to return (default: 20)'),
        node_type: z.enum(['decision', 'lesson', 'fact', 'insight']).optional().describe('Filter by knowledge type'),
      }),
    },
    async (input) => {
      const workspaceId = resolveWorkspaceId(input.workspace_id);
      if (!workspaceId) {
        return errorResult('Error: workspace_id is required. Please call session_init first or provide workspace_id explicitly.');
      }

      const result = await client.githubKnowledge({ workspace_id: workspaceId, limit: input.limit, node_type: input.node_type });
      if (result.length === 0) {
        return { content: [{ type: 'text' as const, text: 'No knowledge extracted from GitHub yet. Knowledge is distilled from issues/PRs after sync.' }] };
      }
      return { content: [{ type: 'text' as const, text: formatContent(result) }], structuredContent: toStructured(result) };
    }
  );

  registerTool(
    'slack_knowledge',
    {
      title: 'Slack extracted knowledge',
      description: `Get knowledge extracted from Slack conversations.
Returns: decisions, lessons, and insights automatically distilled from Slack discussions.
This surfaces key decisions and learnings from your team conversations.

Example queries:
- "What decisions were made in #engineering this week?"
- "Show lessons learned from outages"
- "What architectural insights came from Slack?"`,
      inputSchema: z.object({
        workspace_id: z.string().uuid().optional(),
        limit: z.number().optional().describe('Maximum items to return (default: 20)'),
        node_type: z.enum(['decision', 'lesson', 'fact', 'insight']).optional().describe('Filter by knowledge type'),
      }),
    },
    async (input) => {
      const workspaceId = resolveWorkspaceId(input.workspace_id);
      if (!workspaceId) {
        return errorResult('Error: workspace_id is required. Please call session_init first or provide workspace_id explicitly.');
      }

      const result = await client.slackKnowledge({ workspace_id: workspaceId, limit: input.limit, node_type: input.node_type });
      if (result.length === 0) {
        return { content: [{ type: 'text' as const, text: 'No knowledge extracted from Slack yet. Knowledge is distilled from high-engagement threads after sync.' }] };
      }
      return { content: [{ type: 'text' as const, text: formatContent(result) }], structuredContent: toStructured(result) };
    }
  );

  registerTool(
    'github_summary',
    {
      title: 'GitHub activity summary',
      description: `Get a high-level summary of GitHub activity for a workspace.
Returns: overview of issues, PRs, commits, releases, and highlights for the specified period.
Use this for weekly/monthly reports or to get a quick overview of repository activity.

Example prompts:
- "Give me a weekly GitHub summary"
- "What happened in GitHub last month?"
- "Show me the GitHub summary for repo X"`,
      inputSchema: z.object({
        workspace_id: z.string().uuid().optional(),
        days: z.number().optional().describe('Number of days to summarize (default: 7)'),
        repo: z.string().optional().describe('Filter by repository name'),
      }),
    },
    async (input) => {
      const workspaceId = resolveWorkspaceId(input.workspace_id);
      if (!workspaceId) {
        return errorResult('Error: workspace_id is required. Please call session_init first or provide workspace_id explicitly.');
      }

      const result = await client.githubSummary({
        workspace_id: workspaceId,
        days: input.days,
        repo: input.repo,
      });
      return { content: [{ type: 'text' as const, text: formatContent(result) }], structuredContent: toStructured(result) };
    }
  );

  registerTool(
    'slack_summary',
    {
      title: 'Slack activity summary',
      description: `Get a high-level summary of Slack activity for a workspace.
Returns: overview of messages, threads, top channels, and highlights for the specified period.
Use this for weekly/monthly reports or to get a quick overview of team discussions.

Example prompts:
- "Give me a weekly Slack summary"
- "What was discussed in Slack last month?"
- "Show me the Slack summary for #engineering"`,
      inputSchema: z.object({
        workspace_id: z.string().uuid().optional(),
        days: z.number().optional().describe('Number of days to summarize (default: 7)'),
        channel: z.string().optional().describe('Filter by channel name'),
      }),
    },
    async (input) => {
      const workspaceId = resolveWorkspaceId(input.workspace_id);
      if (!workspaceId) {
        return errorResult('Error: workspace_id is required. Please call session_init first or provide workspace_id explicitly.');
      }

      const result = await client.slackSummary({
        workspace_id: workspaceId,
        days: input.days,
        channel: input.channel,
      });
      return { content: [{ type: 'text' as const, text: formatContent(result) }], structuredContent: toStructured(result) };
    }
  );

  registerTool(
    'integrations_search',
    {
      title: 'Cross-source search',
      description: `Search across all connected integrations (GitHub, Slack, etc.) with a single query.
Returns: unified results from all sources, ranked by relevance or recency.
Use this to find related discussions, issues, and content across all your tools.

Example prompts:
- "Search all integrations for database migration discussions"
- "Find mentions of authentication across GitHub and Slack"
- "Search for API changes in the last 30 days"`,
      inputSchema: z.object({
        workspace_id: z.string().uuid().optional(),
        query: z.string().describe('Search query'),
        limit: z.number().optional().describe('Maximum results (default: 20)'),
        sources: z.array(z.string()).optional().describe('Filter by source: github, slack'),
        days: z.number().optional().describe('Filter to results within N days'),
        sort_by: z.enum(['relevance', 'recent', 'engagement']).optional().describe('Sort by: relevance, recent, or engagement'),
      }),
    },
    async (input) => {
      const workspaceId = resolveWorkspaceId(input.workspace_id);
      if (!workspaceId) {
        return errorResult('Error: workspace_id is required. Please call session_init first or provide workspace_id explicitly.');
      }

      const result = await client.integrationsSearch({
        workspace_id: workspaceId,
        query: input.query,
        limit: input.limit,
        sources: input.sources,
        days: input.days,
        sort_by: input.sort_by,
      });
      return { content: [{ type: 'text' as const, text: formatContent(result) }], structuredContent: toStructured(result) };
    }
  );

  registerTool(
    'integrations_summary',
    {
      title: 'Cross-source activity summary',
      description: `Get a unified summary of activity across all connected integrations.
Returns: combined overview of GitHub and Slack activity, key highlights, and trends.
Use this for weekly team summaries or to understand overall activity across all tools.

Example prompts:
- "Give me a weekly team summary across all sources"
- "What happened across GitHub and Slack last week?"
- "Show me a unified activity overview"`,
      inputSchema: z.object({
        workspace_id: z.string().uuid().optional(),
        days: z.number().optional().describe('Number of days to summarize (default: 7)'),
      }),
    },
    async (input) => {
      const workspaceId = resolveWorkspaceId(input.workspace_id);
      if (!workspaceId) {
        return errorResult('Error: workspace_id is required. Please call session_init first or provide workspace_id explicitly.');
      }

      const result = await client.integrationsSummary({
        workspace_id: workspaceId,
        days: input.days,
      });
      return { content: [{ type: 'text' as const, text: formatContent(result) }], structuredContent: toStructured(result) };
    }
  );

  registerTool(
    'integrations_knowledge',
    {
      title: 'Cross-source knowledge',
      description: `Get knowledge extracted from all connected integrations (GitHub, Slack, etc.).
Returns: decisions, lessons, and insights distilled from all sources.
Use this to find key decisions and learnings from across your team's conversations.

Example prompts:
- "What decisions were made across all sources about authentication?"
- "Show me lessons learned from all integrations"
- "What insights have we gathered from GitHub and Slack?"`,
      inputSchema: z.object({
        workspace_id: z.string().uuid().optional(),
        knowledge_type: z.enum(['decision', 'lesson', 'fact', 'insight']).optional().describe('Filter by knowledge type'),
        query: z.string().optional().describe('Optional search query to filter knowledge'),
        sources: z.array(z.string()).optional().describe('Filter by source: github, slack'),
        limit: z.number().optional().describe('Maximum items to return (default: 20)'),
      }),
    },
    async (input) => {
      const workspaceId = resolveWorkspaceId(input.workspace_id);
      if (!workspaceId) {
        return errorResult('Error: workspace_id is required. Please call session_init first or provide workspace_id explicitly.');
      }

      const result = await client.integrationsKnowledge({
        workspace_id: workspaceId,
        knowledge_type: input.knowledge_type,
        query: input.query,
        sources: input.sources,
        limit: input.limit,
      });
      return { content: [{ type: 'text' as const, text: formatContent(result) }], structuredContent: toStructured(result) };
    }
  );

  registerTool(
    'integrations_status',
    {
      title: 'Integration health status',
      description: `Check the status of all integrations (GitHub, Slack, etc.) for a workspace.
Returns: connection status, last sync time, next sync time, and any errors.
Use this to verify integrations are healthy and syncing properly.`,
      inputSchema: z.object({
        workspace_id: z.string().uuid().optional(),
      }),
    },
    async (input) => {
      const workspaceId = resolveWorkspaceId(input.workspace_id);
      if (!workspaceId) {
        return errorResult('Error: workspace_id is required. Please call session_init first or provide workspace_id explicitly.');
      }

      const result = await client.integrationsStatus({ workspace_id: workspaceId });
      if (result.length === 0) {
        return { content: [{ type: 'text' as const, text: 'No integrations configured for this workspace.' }] };
      }

      const formatted = result.map(i => {
        const status = i.status === 'connected' ? '✅' : i.status === 'error' ? '❌' : '⏳';
        const lastSync = i.last_sync_at ? new Date(i.last_sync_at).toLocaleString() : 'Never';
        const error = i.error_message ? ` (Error: ${i.error_message})` : '';
        return `${status} ${i.provider}: ${i.status} | Last sync: ${lastSync} | Resources: ${i.resources_synced}${error}`;
      }).join('\n');

      return { content: [{ type: 'text' as const, text: formatted }], structuredContent: toStructured(result) };
    }
  );

  // ============================================
  // Reminder Tools
  // ============================================

  registerTool(
    'reminders_list',
    {
      title: 'List reminders',
      description: `List all reminders for the current user.
Returns: reminders with title, content, remind_at, priority, status, and keywords.
Can filter by status (pending, completed, dismissed, snoozed) and priority (low, normal, high, urgent).

Use this to see what reminders you have set.`,
      inputSchema: z.object({
        workspace_id: z.string().uuid().optional(),
        project_id: z.string().uuid().optional(),
        status: z.enum(['pending', 'completed', 'dismissed', 'snoozed']).optional().describe('Filter by status'),
        priority: z.enum(['low', 'normal', 'high', 'urgent']).optional().describe('Filter by priority'),
        limit: z.number().optional().describe('Maximum reminders to return (default: 20)'),
      }),
    },
    async (input) => {
      const result = await client.remindersList({
        workspace_id: input.workspace_id,
        project_id: input.project_id,
        status: input.status,
        priority: input.priority,
        limit: input.limit,
      });
      if (!result.reminders || result.reminders.length === 0) {
        return { content: [{ type: 'text' as const, text: 'No reminders found.' }] };
      }
      return { content: [{ type: 'text' as const, text: formatContent(result) }], structuredContent: toStructured(result) };
    }
  );

  registerTool(
    'reminders_active',
    {
      title: 'Get active reminders',
      description: `Get active reminders that are pending, overdue, or due soon.
Returns: reminders with urgency levels (overdue, due_soon, today, upcoming).
Optionally provide context (e.g., current task description) to get contextually relevant reminders.

Use this to see what reminders need attention now.`,
      inputSchema: z.object({
        workspace_id: z.string().uuid().optional(),
        project_id: z.string().uuid().optional(),
        context: z.string().optional().describe('Optional context to match relevant reminders (e.g., current task)'),
        limit: z.number().optional().describe('Maximum reminders to return (default: 10)'),
      }),
    },
    async (input) => {
      const result = await client.remindersActive({
        workspace_id: input.workspace_id,
        project_id: input.project_id,
        context: input.context,
        limit: input.limit,
      });

      if (!result.reminders || result.reminders.length === 0) {
        return { content: [{ type: 'text' as const, text: 'No active reminders.' }] };
      }

      // Format with urgency indicators
      const formatted = result.reminders.map(r => {
        const icon = r.urgency === 'overdue' ? '🔴' : r.urgency === 'due_soon' ? '🟠' : r.urgency === 'today' ? '🟡' : '🔵';
        const priority = r.priority !== 'normal' ? ` [${r.priority}]` : '';
        return `${icon} ${r.title}${priority}\n   Due: ${new Date(r.remind_at).toLocaleString()}\n   ${r.content_preview}`;
      }).join('\n\n');

      const header = result.overdue_count > 0 ? `⚠️ ${result.overdue_count} overdue reminder(s)\n\n` : '';

      return { content: [{ type: 'text' as const, text: header + formatted }], structuredContent: toStructured(result) };
    }
  );

  registerTool(
    'reminders_create',
    {
      title: 'Create a reminder',
      description: `Create a new reminder for a specific date/time.
Set reminders to be notified about tasks, follow-ups, or important dates.

Priority levels: low, normal, high, urgent
Recurrence: daily, weekly, monthly (optional)

Example: Create a reminder to "Review PR #123" for tomorrow at 10am with high priority.`,
      inputSchema: z.object({
        workspace_id: z.string().uuid().optional(),
        project_id: z.string().uuid().optional(),
        title: z.string().describe('Reminder title (brief, descriptive)'),
        content: z.string().describe('Reminder details/description'),
        remind_at: z.string().describe('When to remind (ISO 8601 datetime, e.g., "2025-01-15T10:00:00Z")'),
        priority: z.enum(['low', 'normal', 'high', 'urgent']).optional().describe('Priority level (default: normal)'),
        keywords: z.array(z.string()).optional().describe('Keywords for contextual surfacing'),
        recurrence: z.enum(['daily', 'weekly', 'monthly']).optional().describe('Recurrence pattern'),
      }),
    },
    async (input) => {
      const result = await client.remindersCreate({
        workspace_id: input.workspace_id,
        project_id: input.project_id,
        title: input.title,
        content: input.content,
        remind_at: input.remind_at,
        priority: input.priority,
        keywords: input.keywords,
        recurrence: input.recurrence,
      });

      const due = new Date(result.remind_at).toLocaleString();
      return {
        content: [{ type: 'text' as const, text: `✅ Reminder created: "${result.title}"\nDue: ${due}\nPriority: ${result.priority}\nID: ${result.id}` }],
        structuredContent: toStructured(result)
      };
    }
  );

  registerTool(
    'reminders_snooze',
    {
      title: 'Snooze a reminder',
      description: `Snooze a reminder until a later time.
Use this to postpone a reminder without dismissing it.

Common snooze durations:
- 1 hour: add 1 hour to current time
- 4 hours: add 4 hours
- Tomorrow: next day at 9am
- Next week: 7 days from now`,
      inputSchema: z.object({
        reminder_id: z.string().uuid().describe('ID of the reminder to snooze'),
        until: z.string().describe('When to resurface the reminder (ISO 8601 datetime)'),
      }),
    },
    async (input) => {
      const result = await client.remindersSnooze({
        reminder_id: input.reminder_id,
        until: input.until,
      });

      const snoozedUntil = new Date(result.snoozed_until).toLocaleString();
      return {
        content: [{ type: 'text' as const, text: `😴 Reminder snoozed until ${snoozedUntil}` }],
        structuredContent: toStructured(result)
      };
    }
  );

  registerTool(
    'reminders_complete',
    {
      title: 'Complete a reminder',
      description: `Mark a reminder as completed.
Use this when the task or action associated with the reminder is done.`,
      inputSchema: z.object({
        reminder_id: z.string().uuid().describe('ID of the reminder to complete'),
      }),
    },
    async (input) => {
      const result = await client.remindersComplete({
        reminder_id: input.reminder_id,
      });

      return {
        content: [{ type: 'text' as const, text: `✅ Reminder completed!` }],
        structuredContent: toStructured(result)
      };
    }
  );

  registerTool(
    'reminders_dismiss',
    {
      title: 'Dismiss a reminder',
      description: `Dismiss a reminder without completing it.
Use this to remove a reminder that is no longer relevant.`,
      inputSchema: z.object({
        reminder_id: z.string().uuid().describe('ID of the reminder to dismiss'),
      }),
    },
    async (input) => {
      const result = await client.remindersDismiss({
        reminder_id: input.reminder_id,
      });

      return {
        content: [{ type: 'text' as const, text: `🗑️ Reminder dismissed.` }],
        structuredContent: toStructured(result)
      };
    }
  );
}
