import { VERSION } from './version.js';

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
export const RULES_VERSION = VERSION === 'unknown' ? '0.0.0' : VERSION;

/**
 * Complete list of all ContextStream MCP tools (v0.4.x consolidated architecture).
 * This list is used for Claude Code prefixing and should match tools.ts exactly.
 *
 * v0.4.x uses consolidated domain tools (~11 tools) by default for ~75% token reduction.
 */
const CONTEXTSTREAM_TOOL_NAMES = [
  // Standalone tools (always present)
  'session_init',
  'context_smart',
  'context_feedback',
  'generate_rules',

  // Consolidated domain tools (v0.4.x default)
  'search',       // Modes: semantic, hybrid, keyword, pattern
  'session',      // Actions: capture, capture_lesson, get_lessons, recall, remember, user_context, summary, compress, delta, smart_search, decision_trace
  'memory',       // Actions: create_event, get_event, update_event, delete_event, list_events, distill_event, create_node, get_node, update_node, delete_node, list_nodes, supersede_node, search, decisions, timeline, summary
  'graph',        // Actions: dependencies, impact, call_path, related, path, decisions, ingest, circular_dependencies, unused_code, contradictions
  'project',      // Actions: list, get, create, update, index, overview, statistics, files, index_status, ingest_local
  'workspace',    // Actions: list, get, associate, bootstrap
  'reminder',     // Actions: list, active, create, snooze, complete, dismiss
  'integration',  // Provider: slack, github, all; Actions: status, search, stats, activity, contributors, knowledge, summary, channels, discussions, sync_users, repos, issues
  'help',         // Actions: tools, auth, version, editor_rules, enable_bundle
] as const;

function applyMcpToolPrefix(markdown: string, toolPrefix: string): string {
  const toolPattern = CONTEXTSTREAM_TOOL_NAMES.join('|');
  // Only prefix tool calls like `tool(...)` and avoid double-prefixing `mcp__<server>__tool(...)`.
  const toolRegex = new RegExp(`(?<!__)\\b(${toolPattern})\\b(?=\\s*\\()`, 'g');
  return markdown.replace(toolRegex, `${toolPrefix}$1`);
}

const CONTEXTSTREAM_RULES_FULL = `
## ContextStream v0.4.x Integration (Enhanced)

You have access to ContextStream MCP tools for persistent memory and context.
v0.4.x uses **~11 consolidated domain tools** for ~75% token reduction vs previous versions.
Rules Version: ${RULES_VERSION}

## TL;DR - REQUIRED EVERY MESSAGE

| Message | What to Call |
|---------|--------------|
| **1st message** | \`session_init(folder_path="...", context_hint="<user's message>")\`, then \`context_smart(user_message="<user's message>", format="minified", max_tokens=400)\` |
| **2nd+ messages** | \`context_smart(user_message="<user's message>", format="minified", max_tokens=400)\` |
| **Before risky/non-trivial work** | \`session(action="get_lessons", query="<topic>")\` |
| **After completing task** | \`session(action="capture", event_type="decision", ...)\` - MUST capture |
| **User frustration/correction** | \`session(action="capture_lesson", ...)\` - MUST capture lessons |
| **Command/tool error + fix** | \`session(action="capture_lesson", ...)\` - MUST capture lessons |

**NO EXCEPTIONS.** Do not skip even if you think you have enough context.

**First message rule:** After \`session_init\`, always call \`context_smart\` before any other tool or response.

**Context Pack (Pro+):** If enabled, use \`context_smart(..., mode="pack", distill=true)\` for code/file queries. If unavailable or disabled, omit \`mode\` and proceed with standard \`context_smart\` (the API will fall back).

**Tool naming:** Use the exact tool names exposed by your MCP client. Claude Code typically uses \`mcp__<server>__<tool>\` where \`<server>\` matches your MCP config (often \`contextstream\`). If a tool call fails with "No such tool available", refresh rules and match the tool list.

---

## Consolidated Domain Tools Architecture

v0.4.x consolidates ~58 individual tools into ~11 domain tools with action/mode dispatch:

### Standalone Tools (Always Call)
- **\`session_init\`** - Initialize session with workspace detection + context
- **\`context_smart\`** - Semantic search for relevant context (CALL EVERY MESSAGE, including immediately after \`session_init\`)

### Domain Tools (Use action/mode parameter)

| Domain | Actions/Modes | Example |
|--------|---------------|---------|
| **\`search\`** | mode: semantic, hybrid, keyword, pattern | \`search(mode="hybrid", query="auth implementation", limit=3)\` |
| **\`session\`** | action: capture, capture_lesson, get_lessons, recall, remember, user_context, summary, compress, delta, smart_search, decision_trace | \`session(action="capture", event_type="decision", title="Use JWT", content="...")\` |
| **\`memory\`** | action: create_event, get_event, update_event, delete_event, list_events, distill_event, create_node, get_node, update_node, delete_node, list_nodes, supersede_node, search, decisions, timeline, summary | \`memory(action="list_events", limit=10)\` |
| **\`graph\`** | action: dependencies, impact, call_path, related, path, decisions, ingest, circular_dependencies, unused_code, contradictions | \`graph(action="impact", symbol_name="AuthService")\` |
| **\`project\`** | action: list, get, create, update, index, overview, statistics, files, index_status, ingest_local | \`project(action="statistics")\` |
| **\`workspace\`** | action: list, get, associate, bootstrap | \`workspace(action="list")\` |
| **\`reminder\`** | action: list, active, create, snooze, complete, dismiss | \`reminder(action="active")\` |
| **\`integration\`** | provider: slack/github/all; action: status, search, stats, activity, contributors, knowledge, summary, channels, discussions, sync_users, repos, issues | \`integration(provider="github", action="search", query="...")\` |
| **\`help\`** | action: tools, auth, version, editor_rules, enable_bundle | \`help(action="tools")\` |

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
- Deep debugging/architecture: \`context_smart(..., max_tokens=800)\`
- Keep \`format="minified"\` (default) unless debugging

If context still feels missing, use \`session(action="recall", query="...")\` for focused deep lookup.

---

### Rules Update Notices

- If you see **\[RULES_NOTICE]**, update rules via \`generate_rules()\` (or rerun setup).
- If you see **\[VERSION_NOTICE]**, tell the user to update MCP using the provided command.

---

### Preferences & Lessons (Use Early)

- If preferences/style matter: \`session(action="user_context")\`
- Before risky changes: \`session(action="get_lessons", query="<topic>")\`
- On frustration/corrections: \`session(action="capture_lesson", title="...", trigger="...", impact="...", prevention="...")\`

---

### Index & Graph Preflight (REQUIRED for code/file search)

Before searching files or code, confirm the project is indexed and the graph is available:

1. \`project(action="index_status")\` for the current project
2. If missing/stale:
   - Local repo: \`project(action="ingest_local", path="<cwd>")\`
   - Otherwise: \`project(action="index")\`
3. If graph queries are empty/unavailable: \`graph(action="ingest")\`
4. If indexing is in progress, tell the user and wait; do not fall back to local scans.

Only after this preflight, proceed with search/analysis below.

### Search & Code Intelligence (ContextStream-first)

⚠️ **STOP: Before using Glob/Grep/Read/Explore** → Call \`search(mode="hybrid")\` FIRST. Use local tools ONLY if ContextStream returns 0 results.

**Search order:**
1. \`session(action="smart_search", query="...")\` - context-enriched
2. \`search(mode="hybrid", query="...", limit=3)\` or \`search(mode="keyword", query="<filename>", limit=3)\`
3. \`project(action="files")\` - file tree/list (only when needed)
4. \`graph(action="dependencies", ...)\` - code structure
5. Local repo scans (rg/ls/find) - ONLY if ContextStream returns no results, errors, or the user explicitly asks

**Search defaults:** \`search\` returns the top 3 results with compact snippets. Use \`limit\` + \`offset\` for pagination, and \`content_max_chars\` to expand snippets when needed.

If ContextStream returns results, stop and use them. NEVER use local Search/Explore/Read unless you need exact code edits or ContextStream returned 0 results.

**Code Analysis:**
- Dependencies: \`graph(action="dependencies", file_path="...")\`
- Change impact: \`graph(action="impact", symbol_name="...")\`
- Call path: \`graph(action="call_path", from_symbol="...", to_symbol="...")\`
- Build graph: \`graph(action="ingest")\` - async, can take a few minutes

---

### Distillation & Memory Hygiene

- Quick context: \`session(action="summary")\`
- Long chat: \`session(action="compress", content="...")\`
- Memory summary: \`memory(action="summary")\`
- Condense noisy entries: \`memory(action="distill_event", event_id="...")\`

---

### When to Capture (MANDATORY)

| When | Call | Example |
|------|------|---------|
| User makes decision | \`session(action="capture", event_type="decision", ...)\` | "Let's use PostgreSQL" |
| User states preference | \`session(action="capture", event_type="preference", ...)\` | "I prefer TypeScript" |
| You complete a task | \`session(action="capture", event_type="task", ...)\` | Capture what was done |
| Need past context | \`session(action="recall", query="...")\` | "What did we decide about X?" |

**You MUST capture after completing any significant task.** This ensures future sessions have context.

---

### Plans & Tasks

When user asks to create a plan or implementation roadmap:
1. Create plan: \`session(action="capture_plan", title="Plan Title", description="...", goals=["goal1", "goal2"], steps=[{id: "1", title: "Step 1", order: 1}, ...])\`
2. Get plan_id from response, then create tasks: \`memory(action="create_task", title="Task Title", plan_id="<plan_id>", priority="high|medium|low", description="...")\`

To manage existing plans/tasks:
- List plans: \`session(action="list_plans")\`
- Get plan with tasks: \`session(action="get_plan", plan_id="<uuid>", include_tasks=true)\`
- List tasks: \`memory(action="list_tasks", plan_id="<uuid>")\` or \`memory(action="list_tasks")\` for all
- Update task status: \`memory(action="update_task", task_id="<uuid>", task_status="pending|in_progress|completed|blocked")\`
- Link task to plan: \`memory(action="update_task", task_id="<uuid>", plan_id="<plan_uuid>")\`
- Unlink task from plan: \`memory(action="update_task", task_id="<uuid>", plan_id=null)\`
- Delete: \`memory(action="delete_task", task_id="<uuid>")\` or \`memory(action="delete_event", event_id="<plan_uuid>")\`

---

### Complete Action Reference

**session actions:**
- \`capture\` - Save decision/insight/task (requires: event_type, title, content)
- \`capture_lesson\` - Save lesson from mistake (requires: title, category, trigger, impact, prevention)
- \`get_lessons\` - Retrieve relevant lessons (optional: query, category, severity)
- \`recall\` - Natural language memory recall (requires: query)
- \`remember\` - Quick save to memory (requires: content)
- \`user_context\` - Get user preferences/style
- \`summary\` - Workspace summary
- \`compress\` - Compress long conversation
- \`delta\` - Changes since timestamp
- \`smart_search\` - Context-enriched search
- \`decision_trace\` - Trace decision provenance

**memory actions:**
- Event CRUD: \`create_event\`, \`get_event\`, \`update_event\`, \`delete_event\`, \`list_events\`, \`distill_event\`
- Node CRUD: \`create_node\`, \`get_node\`, \`update_node\`, \`delete_node\`, \`list_nodes\`, \`supersede_node\`
- Query: \`search\`, \`decisions\`, \`timeline\`, \`summary\`

**graph actions:**
- Analysis: \`dependencies\`, \`impact\`, \`call_path\`, \`related\`, \`path\`
- Quality: \`circular_dependencies\`, \`unused_code\`, \`contradictions\`
- Management: \`ingest\`, \`decisions\`

See full documentation: https://contextstream.io/docs/mcp/tools
`.trim();

const CONTEXTSTREAM_RULES_MINIMAL = `
## ContextStream v0.4.x (Consolidated Domain Tools)

v0.4.x uses ~11 consolidated domain tools for ~75% token reduction vs previous versions.
Rules Version: ${RULES_VERSION}

### Required Every Message

| Message | What to Call |
|---------|--------------|
| **1st message** | \`session_init(folder_path="<cwd>", context_hint="<user_message>")\`, then \`context_smart(user_message="<user_message>", format="minified", max_tokens=400)\` |
| **2nd+ messages** | \`context_smart(user_message="<user_message>", format="minified", max_tokens=400)\` |
| **Capture decisions** | \`session(action="capture", event_type="decision", title="...", content="...")\` |
| **Before risky work** | \`session(action="get_lessons", query="<topic>")\` |
| **On user frustration** | \`session(action="capture_lesson", title="...", trigger="...", impact="...", prevention="...")\` |

**Context Pack (Pro+):** If enabled, use \`context_smart(..., mode="pack", distill=true)\` for code/file queries. If unavailable or disabled, omit \`mode\` and proceed with standard \`context_smart\` (the API will fall back).

**Tool naming:** Use the exact tool names exposed by your MCP client. Claude Code typically uses \`mcp__<server>__<tool>\` where \`<server>\` matches your MCP config (often \`contextstream\`). If a tool call fails with "No such tool available", refresh rules and match the tool list.

### Quick Reference: Domain Tools

| Tool | Common Usage |
|------|--------------|
| \`search\` | \`search(mode="semantic", query="...", limit=3)\` — modes: semantic, hybrid, keyword, pattern |
| \`session\` | \`session(action="capture", ...)\` — actions: capture, capture_lesson, get_lessons, recall, remember, user_context, summary, compress, delta, smart_search |
| \`memory\` | \`memory(action="list_events", ...)\` — CRUD for events/nodes, search, decisions, timeline, summary |
| \`graph\` | \`graph(action="dependencies", ...)\` — dependencies, impact, call_path, related, ingest |
| \`project\` | \`project(action="list", ...)\` - list, get, create, update, index, overview, statistics, files, index_status, ingest_local |
| \`workspace\` | \`workspace(action="list", ...)\` — list, get, associate, bootstrap |
| \`integration\` | \`integration(provider="github", action="search", ...)\` — GitHub/Slack integration |
| \`help\` | \`help(action="tools")\` — tools, auth, version, editor_rules |

### Behavior Rules

⚠️ **STOP: Before using Glob/Grep/Read/Explore** → Call \`search(mode="hybrid")\` FIRST. Use local tools ONLY if ContextStream returns 0 results.

- **First message**: Call \`session_init\` with context_hint, then call \`context_smart\` before any other tool or response
- **On [INGEST_RECOMMENDED]**: Ask the user if they want to enable semantic code search. Explain: "Indexing your codebase enables AI-powered code search, dependency analysis, and better context. This takes a few minutes." If user agrees, run the provided \`project(action="ingest_local")\` command.
- **Every message after**: Always call \`context_smart\` BEFORE responding (semantic search for relevant context)
- **Before searching files/code**: Check \`project(action="index_status")\`; if missing/stale run \`project(action="ingest_local", path="<cwd>")\` or \`project(action="index")\`, and use \`graph(action="ingest")\` if needed
- **For discovery**: Use \`session(action="smart_search")\` or \`search(mode="hybrid")\` — NEVER use local Glob/Grep/Read first
- **For file/function/config lookups**: Use \`search\`/\`graph\` first; only fall back to rg/ls/find if ContextStream returns no results
- **If ContextStream returns results**: Do NOT use local Search/Explore/Read; only open specific files when needed for exact edits
- **For code analysis**: Use \`graph(action="dependencies")\` or \`graph(action="impact")\` for call/dependency analysis
- **On [RULES_NOTICE]**: Use \`generate_rules()\` to update rules
- **After completing work**: Always capture decisions/insights with \`session(action="capture")\`
- **On mistakes/corrections**: Immediately capture lessons with \`session(action="capture_lesson")\`

### Plans & Tasks

When user asks to create a plan or implementation roadmap:
1. Create plan: \`session(action="capture_plan", title="Plan Title", description="...", goals=["goal1", "goal2"], steps=[{id: "1", title: "Step 1", order: 1}, ...])\`
2. Get plan_id from response, then create tasks: \`memory(action="create_task", title="Task Title", plan_id="<plan_id>", priority="high|medium|low", description="...")\`

To manage existing plans/tasks:
- List plans: \`session(action="list_plans")\`
- Get plan with tasks: \`session(action="get_plan", plan_id="<uuid>", include_tasks=true)\`
- List tasks: \`memory(action="list_tasks", plan_id="<uuid>")\` or \`memory(action="list_tasks")\` for all
- Update task status: \`memory(action="update_task", task_id="<uuid>", task_status="pending|in_progress|completed|blocked")\`
- Link task to plan: \`memory(action="update_task", task_id="<uuid>", plan_id="<plan_uuid>")\`
- Unlink task from plan: \`memory(action="update_task", task_id="<uuid>", plan_id=null)\`
- Delete: \`memory(action="delete_task", task_id="<uuid>")\` or \`memory(action="delete_event", event_id="<plan_uuid>")\`

Full docs: https://contextstream.io/docs/mcp/tools
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
