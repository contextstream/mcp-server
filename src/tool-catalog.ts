/**
 * Tool Catalog - Ultra-compact tool reference for AI assistants
 *
 * Provides token-efficient tool listings so AI always knows available tools.
 * Default format: ~120 tokens for complete catalog.
 */

export interface ToolEntry {
  name: string;
  hint: string;  // 1-2 word usage hint
}

export interface ToolCategory {
  name: string;
  tools: ToolEntry[];
}

/**
 * Complete tool catalog organized by category.
 * Hints are ultra-short usage descriptions.
 */
export const TOOL_CATALOG: ToolCategory[] = [
  {
    name: 'Session',
    tools: [
      { name: 'init', hint: 'start-conv' },
      { name: 'smart', hint: 'each-msg' },
      { name: 'capture', hint: 'save' },
      { name: 'recall', hint: 'find' },
      { name: 'remember', hint: 'quick' },
      { name: 'compress', hint: 'end' },
      { name: 'summary', hint: 'brief' },
      { name: 'delta', hint: 'changes' },
      { name: 'get_lessons', hint: 'learn' },
      { name: 'capture_lesson', hint: 'mistake' },
      { name: 'get_user_context', hint: 'prefs' },
      { name: 'smart_search', hint: 'deep-find' },
    ],
  },
  {
    name: 'Search',
    tools: [
      { name: 'semantic', hint: 'meaning' },
      { name: 'hybrid', hint: 'combo' },
      { name: 'keyword', hint: 'exact' },
      { name: 'pattern', hint: 'code' },
    ],
  },
  {
    name: 'Memory',
    tools: [
      { name: 'create_event', hint: 'new' },
      { name: 'list_events', hint: 'list' },
      { name: 'get_event', hint: 'get' },
      { name: 'update_event', hint: 'edit' },
      { name: 'delete_event', hint: 'rm' },
      { name: 'search', hint: 'find' },
      { name: 'decisions', hint: 'choices' },
      { name: 'timeline', hint: 'history' },
      { name: 'distill_event', hint: 'extract' },
    ],
  },
  {
    name: 'Knowledge',
    tools: [
      { name: 'create_node', hint: 'new' },
      { name: 'list_nodes', hint: 'list' },
      { name: 'get_node', hint: 'get' },
      { name: 'update_node', hint: 'edit' },
      { name: 'delete_node', hint: 'rm' },
      { name: 'supersede_node', hint: 'replace' },
    ],
  },
  {
    name: 'Graph',
    tools: [
      { name: 'related', hint: 'links' },
      { name: 'path', hint: 'trace' },
      { name: 'decisions', hint: 'choices' },
      { name: 'dependencies', hint: 'deps' },
      { name: 'impact', hint: 'changes' },
      { name: 'contradictions', hint: 'conflicts' },
    ],
  },
  {
    name: 'Workspace',
    tools: [
      { name: 'list', hint: '' },
      { name: 'get', hint: '' },
      { name: 'create', hint: '' },
      { name: 'associate', hint: 'link-folder' },
      { name: 'bootstrap', hint: 'new-ws' },
    ],
  },
  {
    name: 'Project',
    tools: [
      { name: 'list', hint: '' },
      { name: 'get', hint: '' },
      { name: 'create', hint: '' },
      { name: 'index', hint: 'scan-code' },
      { name: 'files', hint: 'list-files' },
      { name: 'overview', hint: 'summary' },
    ],
  },
  {
    name: 'AI',
    tools: [
      { name: 'context', hint: 'smart-ctx' },
      { name: 'plan', hint: 'generate' },
      { name: 'tasks', hint: 'breakdown' },
      { name: 'embeddings', hint: 'vectors' },
    ],
  },
];

export type CatalogFormat = 'grouped' | 'minimal' | 'full';

/**
 * Generate ultra-compact tool catalog string.
 *
 * @param format - Output format:
 *   - 'grouped' (default): Category: tool(hint) tool(hint) - ~120 tokens
 *   - 'minimal': Category:tool|tool|tool - ~80 tokens
 *   - 'full': tool_name: description - ~200 tokens
 * @param category - Optional filter to specific category
 * @returns Compact tool catalog string
 */
export function generateToolCatalog(
  format: CatalogFormat = 'grouped',
  category?: string
): string {
  let categories = TOOL_CATALOG;

  if (category) {
    const filtered = TOOL_CATALOG.filter(
      c => c.name.toLowerCase() === category.toLowerCase()
    );
    if (filtered.length > 0) {
      categories = filtered;
    }
  }

  switch (format) {
    case 'minimal':
      return generateMinimal(categories);
    case 'full':
      return generateFull(categories);
    case 'grouped':
    default:
      return generateGrouped(categories);
  }
}

/**
 * Grouped format with hints: Category: tool(hint) tool(hint)
 * ~120 tokens for full catalog
 */
function generateGrouped(categories: ToolCategory[]): string {
  return categories
    .map(cat => {
      const tools = cat.tools
        .map(t => t.hint ? `${t.name}(${t.hint})` : t.name)
        .join(' ');
      return `${cat.name}: ${tools}`;
    })
    .join('\n');
}

/**
 * Minimal format: Category:tool|tool|tool
 * ~80 tokens for full catalog
 */
function generateMinimal(categories: ToolCategory[]): string {
  return categories
    .map(cat => {
      const tools = cat.tools.map(t => t.name).join('|');
      return `${cat.name}:${tools}`;
    })
    .join('\n');
}

/**
 * Full format with descriptions
 * ~200 tokens for full catalog
 */
function generateFull(categories: ToolCategory[]): string {
  const lines: string[] = [];
  for (const cat of categories) {
    lines.push(`## ${cat.name}`);
    for (const tool of cat.tools) {
      const prefix = cat.name.toLowerCase().replace(/\s+/g, '_');
      const fullName = `${prefix}_${tool.name}`;
      lines.push(`- ${fullName}: ${tool.hint || 'standard CRUD'}`);
    }
  }
  return lines.join('\n');
}

/**
 * Get the core tools that should always be available
 */
export function getCoreToolsHint(): string {
  return `Session: init(start) smart(each-msg) capture(save) recall(find) remember(quick)`;
}
