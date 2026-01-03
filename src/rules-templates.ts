/**
 * Editor-specific rule templates for ContextStream integration.
 * These instruct AI assistants to automatically use ContextStream for memory and context.
 */

export interface RuleTemplate {
  filename: string;
  description: string;
  build: (rules: string) => string;
}

const DEFAULT_CLAUDE_MCP_SERVER_NAME = 'contextstream';

/**
 * Complete list of all ContextStream MCP tools.
 * This list is used for Claude Code prefixing and should match tools.ts exactly.
 * Grouped by category for maintainability.
 */
const CONTEXTSTREAM_TOOL_NAMES = [
  // Session/Context (standard)
  'session_init',
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
  // Editor Rules
  'generate_editor_rules',
  // Workspaces
  'workspace_associate',
  'workspace_bootstrap',
  'workspaces_list',
  'workspaces_create',
  'workspaces_update',
  'workspaces_delete',
  'workspaces_get',
  'workspaces_overview',
  'workspaces_analytics',
  'workspaces_content',
  // Projects
  'projects_list',
  'projects_create',
  'projects_update',
  'projects_delete',
  'projects_get',
  'projects_overview',
  'projects_statistics',
  'projects_files',
  'projects_index',
  'projects_index_status',
  'projects_ingest_local',
  // Search
  'search_semantic',
  'search_hybrid',
  'search_keyword',
  'search_pattern',
  'search_suggestions',
  // Memory
  'memory_create_event',
  'memory_bulk_ingest',
  'memory_list_events',
  'memory_create_node',
  'memory_list_nodes',
  'memory_search',
  'memory_decisions',
  'decision_trace',
  'memory_get_event',
  'memory_update_event',
  'memory_delete_event',
  'memory_distill_event',
  'memory_get_node',
  'memory_update_node',
  'memory_delete_node',
  'memory_supersede_node',
  'memory_timeline',
  'memory_summary',
  // Graph
  'graph_related',
  'graph_path',
  'graph_decisions',
  'graph_dependencies',
  'graph_call_path',
  'graph_impact',
  'graph_circular_dependencies',
  'graph_unused_code',
  'graph_ingest',
  'graph_contradictions',
  // AI (PRO)
  'ai_context',
  'ai_enhanced_context',
  'ai_context_budget',
  'ai_embeddings',
  'ai_plan',
  'ai_tasks',
  // GitHub Integration (PRO)
  'github_stats',
  'github_repos',
  'github_contributors',
  'github_activity',
  'github_issues',
  'github_search',
  // Slack Integration (PRO)
  'slack_stats',
  'slack_channels',
  'slack_contributors',
  'slack_activity',
  'slack_discussions',
  'slack_search',
  'slack_sync_users',
  'slack_knowledge',
  'slack_summary',
  // GitHub additional
  'github_knowledge',
  'github_summary',
  // Cross-source integrations
  'integrations_status',
  'integrations_search',
  'integrations_summary',
  'integrations_knowledge',
  // Auth/Meta
  'auth_me',
  'mcp_server_version',
] as const;

function applyMcpToolPrefix(markdown: string, toolPrefix: string): string {
  const toolPattern = CONTEXTSTREAM_TOOL_NAMES.join('|');
  // Avoid double-prefixing tools already in the Claude format `mcp__<server>__<tool>`
  const toolRegex = new RegExp(`(?<!__)\\b(${toolPattern})\\b`, 'g');
  return markdown.replace(toolRegex, `${toolPrefix}$1`);
}

const CONTEXTSTREAM_RULES_FULL = `
## ContextStream Integration (Enhanced)

You have access to ContextStream MCP tools for persistent memory and context.

## TL;DR - REQUIRED EVERY MESSAGE

| Message | What to Call |
|---------|--------------|
| **1st message** | \`session_init(folder_path="...", context_hint="<user's message>")\` |
| **2nd+ messages** | \`context_smart(user_message="<user's message>", format="minified", max_tokens=400)\` |
| **Before risky/non-trivial work** | \`session_get_lessons(query="<topic>")\` |
| **After completing task** | \`session_capture(...)\` - MUST capture decisions/insights |
| **User frustration/correction** | \`session_capture_lesson(...)\` - MUST capture lessons |
| **Command/tool error + fix** | \`session_capture_lesson(...)\` - MUST capture lessons |

**NO EXCEPTIONS.** Do not skip even if you think you have enough context.

---

### Why context_smart is Required (Even After session_init)

**Common mistake:** "session_init already gave me context, I don't need context_smart"

**This is WRONG. Here's why:**
- \`session_init\` returns the last ~10 items **BY TIME** (chronological)
- \`context_smart\` **SEARCHES** for items **RELEVANT to THIS message** (semantic)

**Example failure:**
- User asks: "how should I implement authentication?"
- Auth decisions were made 20 conversations ago
- \`session_init\` won't have it (too old, not in recent 10)
- \`context_smart\` FINDS it via semantic search

**Without context_smart, you WILL miss relevant older context.**

---

### Recommended Token Budgets

- For trivial/local edits: \`context_smart(..., max_tokens=200)\`
- Default: \`context_smart(..., max_tokens=400)\`
- Deep debugging/architecture or heavy "what did we decide?": \`context_smart(..., max_tokens=800)\`
- Keep \`format="minified"\` (default) unless you're debugging tool output

If context still feels missing, increase \`max_tokens\` and/or call \`session_recall\` for a focused deep lookup.

---

### Preferences & Lessons (Use Early)

- If preferences or style matter, call \`session_get_user_context\`.
- Before risky changes or when past mistakes may apply, call \`session_get_lessons(query="<topic>")\`.
- When frustration, corrections, or tool mistakes occur, immediately call \`session_capture_lesson\`.

---

### Search, Graphs, and Code Intelligence (ContextStream-first)

- Default order: \`session_smart_search\` -> \`search_hybrid\`/\`search_keyword\`/\`search_semantic\` -> graph tools -> local repo scans (rg/ls/find) only if ContextStream returns no results.
- Use \`session_smart_search\` before scanning the repo or grepping.
- Use \`search_semantic\`/\`search_hybrid\`/\`search_keyword\` for targeted queries.
- For dependencies/impact/call paths, use \`graph_dependencies\`, \`graph_impact\`, and \`graph_call_path\`.
- If the toolset is complete (Elite), prefer \`graph_call_path\` and \`graph_path\` for call relationships instead of manual searches.
- If the graph is missing or stale, run \`graph_ingest\` (async by default with \`wait: false\`). Tell the user it can take a few minutes; optionally call \`projects_statistics\` to estimate time.

---

### Distillation & Memory Hygiene

- Use \`session_summary\` for a fast workspace snapshot.
- Use \`session_compress\` when the chat is long or context limits are near.
- Use \`memory_summary\` for recent work summaries and \`memory_distill_event\` to condense noisy memory entries.

---

### When to Capture (MANDATORY)

| When | Tool | Example |
|------|------|---------|
| User makes a decision | \`session_capture\` | "Let's use PostgreSQL" -> capture as decision |
| User states preference | \`session_capture\` | "I prefer TypeScript" -> capture as preference |
| You complete a task | \`session_capture\` | Capture what was done, decisions made |
| Need past context | \`session_recall\` | "What did we decide about X?" |

**You MUST capture after completing any significant task.** This ensures future sessions have context.

---

### Full Tool Catalog

To expose all tools below, set \`CONTEXTSTREAM_TOOLSET=complete\` in your MCP config. The default (\`standard\`) includes the essential session, search, memory, and graph tools above.

**To enable the complete toolset in your MCP config:**
\`\`\`json
{
  "env": {
    "CONTEXTSTREAM_TOOLSET": "complete"
  }
}
\`\`\`

**Available tool categories (when \`CONTEXTSTREAM_TOOLSET=complete\`):**

**Session/Context** (included in standard):
\`session_init\`, \`context_smart\`, \`context_feedback\`, \`session_summary\`, \`session_capture\`, \`session_capture_lesson\`, \`session_get_lessons\`, \`session_recall\`, \`session_remember\`, \`session_get_user_context\`, \`session_smart_search\`, \`session_compress\`, \`session_delta\`, \`generate_editor_rules\`, \`workspace_associate\`, \`workspace_bootstrap\`

**Workspaces**:
\`workspaces_list\`, \`workspaces_create\`, \`workspaces_update\`, \`workspaces_delete\`, \`workspaces_get\`, \`workspaces_overview\`, \`workspaces_analytics\`, \`workspaces_content\`

**Projects**:
\`projects_list\`, \`projects_create\`, \`projects_update\`, \`projects_delete\`, \`projects_get\`, \`projects_overview\`, \`projects_statistics\`, \`projects_files\`, \`projects_index\`, \`projects_index_status\`, \`projects_ingest_local\`

**Search**:
\`search_semantic\`, \`search_hybrid\`, \`search_keyword\`, \`search_pattern\`, \`search_suggestions\`

**Memory**:
\`memory_create_event\`, \`memory_bulk_ingest\`, \`memory_list_events\`, \`memory_create_node\`, \`memory_list_nodes\`, \`memory_search\`, \`memory_decisions\`, \`decision_trace\`, \`memory_get_event\`, \`memory_update_event\`, \`memory_delete_event\`, \`memory_distill_event\`, \`memory_get_node\`, \`memory_update_node\`, \`memory_delete_node\`, \`memory_supersede_node\`, \`memory_timeline\`, \`memory_summary\`

**Graph** (code analysis):
\`graph_related\`, \`graph_path\`, \`graph_decisions\`, \`graph_dependencies\`, \`graph_call_path\`, \`graph_impact\`, \`graph_circular_dependencies\`, \`graph_unused_code\`, \`graph_ingest\`, \`graph_contradictions\`

**AI** (PRO):
\`ai_context\`, \`ai_enhanced_context\`, \`ai_context_budget\`, \`ai_embeddings\`, \`ai_plan\`, \`ai_tasks\`

**GitHub Integration** (PRO):
\`github_stats\`, \`github_repos\`, \`github_contributors\`, \`github_activity\`, \`github_issues\`, \`github_search\`, \`github_knowledge\`, \`github_summary\`

**Slack Integration** (PRO):
\`slack_stats\`, \`slack_channels\`, \`slack_contributors\`, \`slack_activity\`, \`slack_discussions\`, \`slack_search\`, \`slack_sync_users\`, \`slack_knowledge\`, \`slack_summary\`

**Cross-Source Integrations** (PRO):
\`integrations_status\`, \`integrations_search\`, \`integrations_summary\`, \`integrations_knowledge\`

See full documentation: https://contextstream.io/docs/mcp/tools
`.trim();

const CONTEXTSTREAM_RULES_MINIMAL = `
## ContextStream (Standard)

- First user message: \`session_init(folder_path="<cwd>", context_hint="<user_message>")\`, then answer.
- Every user message: \`context_smart(user_message="<user_message>", format="minified", max_tokens=400)\` BEFORE answering.
  - Use \`max_tokens=200\` for trivial/local edits, \`max_tokens=800\` for deep debugging/architecture.
- Before risky/non-trivial work: check \`session_get_lessons(query="<topic>")\`; use \`session_get_user_context\` when preferences/style matter.
- For discovery: use \`session_smart_search\` first; use \`search_semantic\`/\`search_hybrid\`/\`search_keyword\` for targeted lookups; avoid local scans until ContextStream returns no results.
- For code intelligence: use \`graph_dependencies\`/\`graph_impact\`/\`graph_call_path\`; if the toolset is complete (Elite), prefer \`graph_call_path\`/\`graph_path\` for call relationships; run \`graph_ingest\` if the graph is missing (async by default, can take a few minutes).
- For distillation: use \`session_summary\` for quick context; use \`session_compress\` for long chats; use \`memory_summary\` or \`memory_distill_event\` to condense memory.
- After meaningful work/decisions/preferences: \`session_capture(event_type=decision|preference|task|insight, title="...", content="...")\`.
- On frustration/corrections/tool mistakes: \`session_capture_lesson(...)\`.

### Tool Catalog

By default, the MCP server exposes the **standard** toolset (~50 tools). To expose fewer tools, set \`CONTEXTSTREAM_TOOLSET=light\`. To expose everything (~86 tools), set \`CONTEXTSTREAM_TOOLSET=complete\` in your MCP config:

\`\`\`json
{
  "env": {
    "CONTEXTSTREAM_TOOLSET": "complete"
  }
}
\`\`\`

Full tool reference: https://contextstream.io/docs/mcp/tools
`.trim();

export const TEMPLATES: Record<string, RuleTemplate> = {
  codex: {
    filename: 'AGENTS.md',
    description: 'Codex CLI agent instructions',
    build: (rules) => `# Codex CLI Instructions
${rules}
`,
  },

  windsurf: {
    filename: '.windsurfrules',
    description: 'Windsurf AI rules',
    build: (rules) => `# Windsurf Rules
${rules}
`,
  },

  cursor: {
    filename: '.cursorrules',
    description: 'Cursor AI rules', 
    build: (rules) => `# Cursor Rules
${rules}
`,
  },

  cline: {
    filename: '.clinerules',
    description: 'Cline AI rules',
    build: (rules) => `# Cline Rules
${rules}
`,
  },

  kilo: {
    filename: '.kilocode/rules/contextstream.md',
    description: 'Kilo Code AI rules',
    build: (rules) => `# Kilo Code Rules
${rules}
`,
  },

  roo: {
    filename: '.roo/rules/contextstream.md',
    description: 'Roo Code AI rules',
    build: (rules) => `# Roo Code Rules
${rules}
`,
  },

  claude: {
    filename: 'CLAUDE.md',
    description: 'Claude Code instructions',
    build: (rules) => `# Claude Code Instructions
${rules}
`,
  },

  aider: {
    filename: '.aider.conf.yml',
    description: 'Aider configuration with system prompt',
    build: (rules) => `# Aider Configuration
# Note: Aider uses different config format - this adds to the system prompt

# Add ContextStream guidance to conventions
conventions: |
${rules.split('\n').map(line => '  ' + line).join('\n')}
`,
  },
};

/**
 * Get all available editor types
 */
export function getAvailableEditors(): string[] {
  return Object.keys(TEMPLATES);
}

/**
 * Get template for a specific editor
 */
export function getTemplate(editor: string): RuleTemplate | null {
  return TEMPLATES[editor.toLowerCase()] || null;
}

/**
 * Generate rule content with workspace-specific customizations
 */
export function generateRuleContent(
  editor: string,
  options?: {
    workspaceName?: string;
    workspaceId?: string;
    projectName?: string;
    additionalRules?: string;
    mode?: 'minimal' | 'full';
  }
): { filename: string; content: string } | null {
  const template = getTemplate(editor);
  if (!template) return null;

  const mode = options?.mode || 'minimal';
  const rules = mode === 'full' ? CONTEXTSTREAM_RULES_FULL : CONTEXTSTREAM_RULES_MINIMAL;

  let content = template.build(rules);

  // Add workspace header if provided
  if (options?.workspaceName || options?.projectName) {
    const header = `
# Workspace: ${options.workspaceName || 'Unknown'}
${options.projectName ? `# Project: ${options.projectName}` : ''}
${options.workspaceId ? `# Workspace ID: ${options.workspaceId}` : ''}

`;
    content = header + content;
  }

  // Append additional rules if provided
  if (options?.additionalRules) {
    content += '\n\n## Project-Specific Rules\n\n' + options.additionalRules;
  }

  // Claude Code requires `mcp__<server>__<tool>` naming convention for MCP tools.
  // Other MCP clients typically use raw tool names.
  if (editor.toLowerCase() === 'claude') {
    content = applyMcpToolPrefix(content, `mcp__${DEFAULT_CLAUDE_MCP_SERVER_NAME}__`);
  }

  return {
    filename: template.filename,
    content: content.trim() + '\n',
  };
}

/**
 * Generate all rule files for a project
 */
export function generateAllRuleFiles(options?: {
  workspaceName?: string;
  workspaceId?: string;
  projectName?: string;
  additionalRules?: string;
  mode?: 'minimal' | 'full';
}): Array<{ editor: string; filename: string; content: string }> {
  return getAvailableEditors()
    .map(editor => {
      const result = generateRuleContent(editor, options);
      if (!result) return null;
      return { editor, ...result };
    })
    .filter((r): r is { editor: string; filename: string; content: string } => r !== null);
}
