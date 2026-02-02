import { VERSION } from "./version.js";

/**
 * Editor-specific rule templates for ContextStream integration.
 * These instruct AI assistants to automatically use ContextStream for memory and context.
 */

export interface RuleTemplate {
  filename: string;
  description: string;
  build: (rules: string) => string;
}

const DEFAULT_CLAUDE_MCP_SERVER_NAME = "contextstream";
export const RULES_VERSION = VERSION === "unknown" ? "0.0.0" : VERSION;

/**
 * Complete list of all ContextStream MCP tools (v0.4.x consolidated architecture).
 * This list is used for Claude Code prefixing and should match tools.ts exactly.
 *
 * v0.4.x uses consolidated domain tools (~11 tools) by default for ~75% token reduction.
 */
const CONTEXTSTREAM_TOOL_NAMES = [
  // Standalone tools (always present)
  "init", // Renamed from session_init - initialize conversation session
  "context", // Renamed from context_smart - get relevant context every message
  "context_feedback",
  "generate_rules",

  // Consolidated domain tools (v0.4.x default)
  "search", // Modes: semantic, hybrid, keyword, pattern
  "session", // Actions: capture, capture_lesson, get_lessons, recall, remember, user_context, summary, compress, delta, smart_search, decision_trace
  "memory", // Actions: create_event, get_event, update_event, delete_event, list_events, distill_event, create_node, get_node, update_node, delete_node, list_nodes, supersede_node, search, decisions, timeline, summary
  "graph", // Actions: dependencies, impact, call_path, related, path, decisions, ingest, circular_dependencies, unused_code, contradictions
  "project", // Actions: list, get, create, update, index, overview, statistics, files, index_status, ingest_local
  "workspace", // Actions: list, get, associate, bootstrap
  "reminder", // Actions: list, active, create, snooze, complete, dismiss
  "integration", // Provider: slack, github, all; Actions: status, search, stats, activity, contributors, knowledge, summary, channels, discussions, sync_users, repos, issues
  "help", // Actions: tools, auth, version, editor_rules, enable_bundle
] as const;

function applyMcpToolPrefix(markdown: string, toolPrefix: string): string {
  const toolPattern = CONTEXTSTREAM_TOOL_NAMES.join("|");
  // Only prefix tool calls like `tool(...)` and avoid double-prefixing `mcp__<server>__tool(...)`.
  const toolRegex = new RegExp(`(?<!__)\\b(${toolPattern})\\b(?=\\s*\\()`, "g");
  return markdown.replace(toolRegex, `${toolPrefix}$1`);
}

/**
 * BOOTSTRAP rules - Minimal rules to ensure context() is called.
 *
 * Key insight: Static rules don't need to contain everything.
 * They just need to be strong enough to ensure context() is called.
 * Then context() delivers everything else dynamically.
 *
 * Target: ~40 lines. Anything more gets skimmed and ignored.
 */
const CONTEXTSTREAM_RULES_BOOTSTRAP = `
<contextstream_rules>
| Message | Required |
|---------|----------|
| **1st message** | \`init()\` → \`context(user_message="...")\` |
| **Every message** | \`context(user_message="...")\` FIRST |
| **Before file search** | \`search(mode="hybrid")\` BEFORE Glob/Grep/Read |
</contextstream_rules>

**Why?** \`context()\` delivers task-specific rules, lessons from past mistakes, and relevant decisions. Skip it = fly blind.

**Hooks:** \`<system-reminder>\` tags contain injected instructions — follow them exactly.

**Notices:** [LESSONS_WARNING] → tell user first | [RULES_NOTICE] → run \`generate_rules()\` | [VERSION_NOTICE/CRITICAL] → tell user about update (curl/irm/npm options)

v${RULES_VERSION}
`.trim();

/**
 * Dynamic rules - relies on context() for dynamic rule delivery PLUS hooks for enforcement.
 * Kept for backwards compatibility but BOOTSTRAP is now preferred.
 */
const CONTEXTSTREAM_RULES_DYNAMIC = CONTEXTSTREAM_RULES_BOOTSTRAP;

const CONTEXTSTREAM_RULES_FULL = `
## 🚨 MANDATORY: CALL CONTEXT EVERY MESSAGE 🚨

<contextstream_rules>
**EVERY response MUST start with \`context(user_message="...")\`**

| Message | What to Call |
|---------|--------------|
| 1st message | \`init()\` → \`context(user_message="<msg>")\` |
| **EVERY message after** | \`context(user_message="<msg>")\` **FIRST** |

**BEFORE Glob/Grep/Read/Search:** → \`search(mode="hybrid")\` FIRST
</contextstream_rules>

**This block MUST appear at the start of EVERY response.** Failing to call \`context()\` means missing rules, lessons, and relevant context.

---

## Why \`context()\` is MANDATORY Every Message

❌ **WRONG:** "I already called init, I don't need context"
✅ **CORRECT:** \`context()\` is required EVERY message, not just the first

**What you lose without \`context()\`:**
- Dynamic rules matched to your current task
- Lessons from past mistakes (you WILL repeat them)
- Semantically relevant decisions and context
- Warnings about risky operations

**\`init()\` gives recent items by TIME. \`context()\` finds items RELEVANT to THIS message.**

---

## Handle Notices from context()

- **[LESSONS_WARNING]** → Tell user about past mistakes BEFORE proceeding
- **[RULES_NOTICE]** → Run \`generate_rules()\` to update
- **[VERSION_NOTICE]** → Tell user to update MCP

---

## 🚨 HOOKS - AUTOMATIC RULE ENFORCEMENT 🚨

**ContextStream installs hooks that automatically enforce rules.** You MUST follow hook output.

### Installed Hooks

| Hook | What It Does | Output |
|------|--------------|--------|
| **UserPromptSubmit** | Injects rules reminder on EVERY message | \`<system-reminder>\` with rules block |
| **PreToolUse** | Blocks Glob/Grep/Search/Explore when ContextStream is available | Error message redirecting to \`search()\` |
| **PostToolUse** | Auto-indexes files after Edit/Write operations | Background indexing |
| **PreCompact** | Saves session state before context compaction | Snapshot creation |

### How Hooks Work

1. **\`<system-reminder>\` tags** - Injected by UserPromptSubmit hook on every message
   - These tags contain the current rules
   - **FOLLOW THE INSTRUCTIONS INSIDE** - they ARE the rules
   - Example: \`[CONTEXTSTREAM RULES] 1. BEFORE Glob/Grep... [END RULES]\`

2. **PreToolUse blocking** - If you try to use Glob/Grep/Search/Explore:
   - Hook returns error: \`STOP: Use mcp__contextstream__search(mode="hybrid") instead\`
   - **You MUST use the suggested ContextStream tool instead**
   - Local tools are only allowed if project is not indexed or ContextStream returns 0 results

3. **PostToolUse indexing** - After Edit/Write operations:
   - Changed files are automatically re-indexed
   - No action required from you

4. **PreCompact snapshots** - Before context compaction:
   - Hook reminds you to save important state
   - Call \`session(action="capture", event_type="session_snapshot", ...)\` when warned

### Disabling Hooks

Set environment variable: \`CONTEXTSTREAM_HOOK_ENABLED=false\`

**Note:** Disabling hooks removes rule enforcement. Only disable for debugging.

---

## 🚨 CRITICAL RULE #1 - CONTEXTSTREAM SEARCH FIRST 🚨

**BEFORE using Glob, Grep, Search, Read (for discovery), Explore, or ANY local file scanning:**
\`\`\`
STOP → Call search(mode="hybrid", query="...") FIRST
\`\`\`

**Note:** PreToolUse hooks block these tools when ContextStream is available.
**Claude Code users:** Your tool names are \`mcp__contextstream__search\`, \`mcp__contextstream__init\`, etc.

❌ **NEVER DO THIS:**
- \`Glob("**/*.ts")\` → Use \`search(mode="pattern", query="*.ts")\` instead
- \`Grep("functionName")\` → Use \`search(mode="keyword", query="functionName")\` instead
- \`Read(file)\` for discovery → Use \`search(mode="hybrid", query="...")\` instead
- \`Task(subagent_type="Explore")\` → Use \`search(mode="hybrid")\` instead

✅ **ALWAYS DO THIS:**
1. \`search(mode="hybrid", query="what you're looking for")\`
2. Only use local tools (Glob/Grep/Read) if ContextStream returns **0 results**
3. Use Read ONLY for exact file edits after you know the file path

This applies to **EVERY search** throughout the **ENTIRE conversation**, not just the first message.

---

## 🚨 CRITICAL RULE #2 - AUTO-INDEXING 🚨

**ContextStream auto-indexes your project on \`init\`.** You do NOT need to:
- Ask the user to index
- Manually trigger ingestion
- Check index_status before every search

**When \`init\` returns \`indexing_status: "started"\` or \`"refreshing"\`:**
- Background indexing is running automatically
- Search results will be available within seconds to minutes
- **DO NOT fall back to local tools** - wait for ContextStream search to work
- If search returns 0 results initially, try again after a moment

**Only manually trigger indexing if:**
- \`init\` returned \`ingest_recommendation.recommended: true\` (rare edge case)
- User explicitly asks to re-index

---

## 🚨 CRITICAL RULE #3 - LESSONS (PAST MISTAKES) 🚨

**Lessons are past mistakes that MUST inform your work.** Ignoring lessons leads to repeated failures.

### On \`init\`:
- Check for \`lessons\` and \`lessons_warning\` in the response
- If present, **READ THEM IMMEDIATELY** before doing any work
- These are high-priority lessons (critical/high severity) relevant to your context
- **Apply the prevention steps** from each lesson to avoid repeating mistakes

### On \`context\`:
- Check for \`[LESSONS_WARNING]\` tag in the response
- If present, you **MUST** tell the user about the lessons before proceeding
- Lessons are proactively fetched when risky actions are detected (refactor, migrate, deploy, etc.)
- **Do not skip or bury this warning** - lessons represent real past mistakes

### Before ANY Non-Trivial Work:
**ALWAYS call \`session(action="get_lessons", query="<topic>")\`** where \`<topic>\` matches what you're about to do:
- Before refactoring → \`session(action="get_lessons", query="refactoring")\`
- Before API changes → \`session(action="get_lessons", query="API changes")\`
- Before database work → \`session(action="get_lessons", query="database migrations")\`
- Before deployments → \`session(action="get_lessons", query="deployment")\`

### When Lessons Are Found:
1. **Summarize the lessons** to the user before proceeding
2. **Explicitly state how you will avoid the past mistakes**
3. If a lesson conflicts with the current approach, **warn the user**

**Failing to check lessons before risky work is a critical error.**

---

## ContextStream v0.4.x Integration (Enhanced)

You have access to ContextStream MCP tools for persistent memory and context.
v0.4.x uses **~11 consolidated domain tools** for ~75% token reduction vs previous versions.
Rules Version: ${RULES_VERSION}

## TL;DR - CONTEXT EVERY MESSAGE

| Message | Required |
|---------|----------|
| **1st message** | \`init()\` → \`context(user_message="<msg>")\` |
| **EVERY message after** | \`context(user_message="<msg>")\` **FIRST** |
| **Before file search** | \`search(mode="hybrid")\` FIRST |
| **After significant work** | \`session(action="capture", event_type="decision", ...)\` |
| **User correction** | \`session(action="capture_lesson", ...)\` |

### Why EVERY Message?

\`context()\` delivers:
- **Dynamic rules** matched to your current task
- **Lessons** from past mistakes (prevents repeating errors)
- **Relevant decisions** and context (semantic search)
- **Warnings** about risky operations

**Without \`context()\`, you are blind to relevant context and will repeat past mistakes.**

### Protocol

| Step | What to Call |
|------|--------------|
| **1st message** | \`init(folder_path="...", context_hint="<msg>")\`, then \`context(...)\` |
| **2nd+ messages** | \`context(user_message="<msg>", format="minified", max_tokens=400)\` |
| **Code search** | \`search(mode="hybrid", query="...")\` — BEFORE Glob/Grep/Read |
| **After significant work** | \`session(action="capture", event_type="decision", ...)\` |
| **User correction** | \`session(action="capture_lesson", ...)\` |
| **⚠️ When warnings received** | **STOP**, acknowledge, explain mitigation, then proceed |

**First message rule:** After \`init\`:
1. Check for \`lessons\` in response - if present, READ and SUMMARIZE them to user
2. Then call \`context\` before any other tool or response

**Context Pack (Pro+):** If enabled, use \`context(..., mode="pack", distill=true)\` for code/file queries. If unavailable or disabled, omit \`mode\` and proceed with standard \`context\` (the API will fall back).

**Tool naming:** Use the exact tool names exposed by your MCP client. Claude Code typically uses \`mcp__<server>__<tool>\` where \`<server>\` matches your MCP config (often \`contextstream\`). If a tool call fails with "No such tool available", refresh rules and match the tool list.

---

## Consolidated Domain Tools Architecture

v0.4.x consolidates ~58 individual tools into ~11 domain tools with action/mode dispatch:

### Standalone Tools
- **\`init\`** - Initialize session with workspace detection + context (skip for simple utility operations)
- **\`context\`** - Semantic search for relevant context (skip for simple utility operations)

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

### Why context is Required (Even After init)

**Common mistake:** "init already gave me context, I don't need context"

**This is WRONG. Here's why:**
- \`init\` returns the last ~10 items **BY TIME** (chronological)
- \`context\` **SEARCHES** for items **RELEVANT to THIS message** (semantic)

**Example failure:**
- User asks: "how should I implement authentication?"
- Auth decisions were made 20 conversations ago
- \`init\` won't have it (too old, not in recent 10)
- \`context\` FINDS it via semantic search

**Without context, you WILL miss relevant older context.**

---

### Recommended Token Budgets

- For trivial/local edits: \`context(..., max_tokens=200)\`
- Default: \`context(..., max_tokens=400)\`
- Deep debugging/architecture: \`context(..., max_tokens=800)\`
- Keep \`format="minified"\` (default) unless debugging

If context still feels missing, use \`session(action="recall", query="...")\` for focused deep lookup.

---

### Rules, Version & Lessons Notices

**[RULES_NOTICE]** - Update rules via \`generate_rules()\` (or rerun setup).

**[VERSION_NOTICE]** or **[VERSION_CRITICAL]** - You **MUST** tell the user (be helpful, not annoying):
  1. A new MCP server version is available with improvements
  2. Provide update commands (user chooses preferred method):
     - macOS/Linux: \`curl -fsSL https://contextstream.io/scripts/setup.sh | bash\`
     - Windows: \`irm https://contextstream.io/scripts/setup.ps1 | iex\`
     - npm: \`npm install -g @contextstream/mcp-server@latest\`
  3. Restart AI tool after updating
  **Mention once** - don't nag. Users often miss stderr warnings so this helps.

**[LESSONS_WARNING]** - You **MUST** before proceeding:
  1. Read all lessons listed
  2. Tell the user about relevant lessons
  3. Explain how you will avoid each past mistake
  **This is critical** - ignoring lessons leads to repeated failures.

---

### Preferences & Lessons (Use Early)

- If preferences/style matter: \`session(action="user_context")\`
- Before risky changes: \`session(action="get_lessons", query="<topic>")\`
- On frustration/corrections: \`session(action="capture_lesson", title="...", trigger="...", impact="...", prevention="...")\`

---

### Context Pressure & Compaction Awareness

ContextStream tracks context pressure to help you stay ahead of conversation compaction:

**Automatic tracking:** Token usage is tracked automatically. \`context\` returns \`context_pressure\` when usage is high.

**When \`context\` returns \`context_pressure\` with high/critical level:**
1. Review the \`suggested_action\` field:
   - \`prepare_save\`: Start thinking about saving important state
   - \`save_now\`: Immediately call \`session(action="capture", event_type="session_snapshot")\` to preserve state

**PreCompact Hook:** Automatically saves session state before context compaction.
Installed by default. Disable with: \`CONTEXTSTREAM_HOOK_ENABLED=false\`

**Before compaction happens (when warned):**
\`\`\`
session(action="capture", event_type="session_snapshot", title="Pre-compaction snapshot", content="{
  \\"conversation_summary\\": \\"<summarize what we've been doing>\\",
  \\"current_goal\\": \\"<the main task>\\",
  \\"active_files\\": [\\"file1.ts\\", \\"file2.ts\\"],
  \\"recent_decisions\\": [{title: \\"...\\", rationale: \\"...\\"}],
  \\"unfinished_work\\": [{task: \\"...\\", status: \\"...\\", next_steps: \\"...\\"}]
}")
\`\`\`

**After compaction (when context seems lost):**
1. Call \`init(folder_path="...", is_post_compact=true)\` - this auto-restores the most recent snapshot
2. Or call \`session_restore_context()\` directly to get the saved state
3. Review the \`restored_context\` to understand prior work
4. Acknowledge to the user what was restored and continue

---

### Index Status (Auto-Managed)

**Indexing is automatic.** After \`init\`, the project is auto-indexed in the background.

**You do NOT need to manually check index_status before every search.** Just use \`search()\`.

**If search returns 0 results and you expected matches:**
1. Check if \`init\` returned \`indexing_status: "started"\` - indexing may still be in progress
2. Wait a moment and retry \`search()\`
3. Only as a last resort: \`project(action="index_status")\` to check

**Graph data:** If graph queries (\`dependencies\`, \`impact\`) return empty, run \`graph(action="ingest")\` once.

**NEVER fall back to local tools (Glob/Grep/Read) just because search returned 0 results on first try.** Retry first.

### Enhanced Context (Server-Side Warnings)

\`context\` now includes **intelligent server-side filtering** that proactively surfaces relevant warnings:

**Response fields:**
- \`warnings\`: Array of warning strings (displayed with ⚠️ prefix)

**What triggers warnings:**
- **Lessons**: Past mistakes relevant to the current query (via semantic matching)
- **Risky actions**: Detected high-risk operations (deployments, migrations, destructive commands)
- **Breaking changes**: When modifications may impact other parts of the codebase

**When you receive warnings:**
1. **STOP** and read each warning carefully
2. **Acknowledge** the warning to the user
3. **Explain** how you will avoid the issue
4. Only proceed after addressing the warnings

### Search & Code Intelligence (ContextStream-first)

⚠️ **STOP: Before using Search/Glob/Grep/Read/Explore** → Call \`search(mode="hybrid")\` FIRST. Use local tools ONLY if ContextStream returns 0 results.

**❌ WRONG workflow (wastes tokens, slow):**
\`\`\`
Grep "function" → Read file1.ts → Read file2.ts → Read file3.ts → finally understand
\`\`\`

**✅ CORRECT workflow (fast, complete):**
\`\`\`
search(mode="hybrid", query="function implementation") → done (results include context)
\`\`\`

**Why?** ContextStream search returns semantic matches + context + file locations in ONE call. Local tools require multiple round-trips.

**Search order:**
1. \`session(action="smart_search", query="...")\` - context-enriched
2. \`search(mode="hybrid", query="...", limit=3)\` or \`search(mode="keyword", query="<filename>", limit=3)\`
3. \`project(action="files")\` - file tree/list (only when needed)
4. \`graph(action="dependencies", ...)\` - code structure
5. Local repo scans (rg/ls/find) - ONLY if ContextStream returns no results, errors, or the user explicitly asks

**Search Mode Selection:**

| Need | Mode | Example |
|------|------|---------|
| Find code by meaning | \`hybrid\` | "authentication logic", "error handling" |
| Exact string/symbol | \`keyword\` | "UserAuthService", "API_KEY" |
| File patterns | \`pattern\` | "*.sql", "test_*.py" |
| ALL matches (grep-like) | \`exhaustive\` | "TODO", "FIXME" (find all occurrences) |
| Symbol renaming | \`refactor\` | "oldFunctionName" (word-boundary matching) |
| Conceptual search | \`semantic\` | "how does caching work" |

**Token Efficiency:** Use \`output_format\` to reduce response size:
- \`full\` (default): Full content for understanding code
- \`paths\`: File paths only (80% token savings) - use for file listings
- \`minimal\`: Compact format (60% savings) - use for refactoring
- \`count\`: Match counts only (90% savings) - use for quick checks

**When to use \`output_format=count\`:**
- User asks "how many X" or "count of X" → \`search(..., output_format="count")\`
- Checking if something exists → count > 0 is sufficient
- Large exhaustive searches → get count first, then fetch if needed

**Auto-suggested formats:** Search responses include \`query_interpretation.suggested_output_format\` when the API detects an optimal format:
- Symbol queries (e.g., "authOptions") → suggests \`minimal\` (path + line + snippet)
- Count queries (e.g., "how many") → suggests \`count\`
**USE the suggested format** on subsequent searches for best token efficiency.

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

### When to Capture

| When | Call | Example |
|------|------|---------|
| User makes decision | \`session(action="capture", event_type="decision", ...)\` | "Let's use PostgreSQL" |
| User states preference | \`session(action="capture", event_type="preference", ...)\` | "I prefer TypeScript" |
| Complete significant task | \`session(action="capture", event_type="task", ...)\` | Capture what was done |
| Need past context | \`session(action="recall", query="...")\` | "What did we decide about X?" |

**DO NOT capture utility operations:**
- ❌ "Listed workspaces" - not meaningful context
- ❌ "Showed version" - not a decision
- ❌ "Listed projects" - just data retrieval

**DO capture meaningful work:**
- ✅ Decisions, preferences, completed features
- ✅ Lessons from mistakes
- ✅ Insights about architecture or patterns

---

### 🚨 Plans & Tasks - USE CONTEXTSTREAM, NOT FILE-BASED PLANS 🚨

**CRITICAL: When the user requests planning, implementation plans, roadmaps, task breakdowns, or step-by-step approaches:**

❌ **DO NOT** use built-in plan mode (EnterPlanMode tool)
❌ **DO NOT** write plans to markdown files or plan documents
❌ **DO NOT** ask "should I create a plan file?"

✅ **ALWAYS** use ContextStream's plan/task system instead

**Trigger phrases to detect (use ContextStream immediately):**
- "create a plan", "make a plan", "plan this", "plan for"
- "implementation plan", "roadmap", "milestones"
- "break down", "breakdown", "break this into steps"
- "what are the steps", "step by step", "outline the approach"
- "task list", "todo list", "action items"
- "how should we approach", "implementation strategy"

**When detected, immediately:**

1. **Create the plan in ContextStream:**
\`\`\`
session(action="capture_plan", title="<descriptive title>", description="<what this plan accomplishes>", goals=["goal1", "goal2"], steps=[{id: "1", title: "Step 1", order: 1, description: "..."}, ...])
\`\`\`

2. **Create tasks for each step:**
\`\`\`
memory(action="create_task", title="<task title>", plan_id="<plan_id from step 1>", priority="high|medium|low", description="<detailed task description>")
\`\`\`

**Why ContextStream plans are better:**
- Plans persist across sessions and are searchable
- Tasks track status (pending/in_progress/completed/blocked)
- Context is preserved with workspace/project association
- Can be retrieved with \`session(action="get_plan", plan_id="...", include_tasks=true)\`
- Future sessions can continue from where you left off

**Managing plans/tasks:**
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
# ContextStream Rules (MANDATORY)

## 🚨 CRITICAL: These Rules Are Non-Negotiable 🚨

<contextstream_protocol>
**EVERY coding response MUST start with:**
1. \`init()\` (1st message only) → then \`context(user_message="<msg>")\`
2. \`context(user_message="<msg>")\` (EVERY subsequent message)

**BEFORE Glob/Grep/Read/Search/Explore:**
→ \`search(mode="hybrid", query="...")\` FIRST — local tools ONLY if 0 results

**HOOKS: \`<system-reminder>\` tags contain instructions — FOLLOW THEM**
</contextstream_protocol>

Rules Version: ${RULES_VERSION}

---

## Why ContextStream First?

| ❌ WRONG | ✅ CORRECT |
|----------|-----------|
| \`Grep → Read → Read → Read\` (4+ calls) | \`search(mode="hybrid")\` (1 call) |
| Missing past decisions & lessons | \`context()\` = rules + lessons + memory |
| Ignoring \`<system-reminder>\` hooks | Hooks enforce ContextStream-first |

**\`context()\` delivers: rules, lessons from past mistakes, relevant decisions, semantic matches**
**\`search()\` is indexed: faster, returns context, one call vs many**
**Hooks inject \`<system-reminder>\` tags: these ARE the rules, follow them**

---

## Protocol

| When | Call |
|------|------|
| 1st message | \`init()\` → \`context(user_message="...")\` |
| Every message after | \`context(user_message="...")\` |
| Before ANY file discovery | \`search(mode="hybrid", query="...")\` |
| On \`<system-reminder>\` | **Follow instructions inside** |
| Save important decisions | \`session(action="capture", event_type="decision", ...)\` |
| Check past mistakes | \`session(action="get_lessons", query="...")\` |

## Search Modes

| Mode | When |
|------|------|
| \`hybrid\` | Default — semantic + keyword |
| \`keyword\` | Exact symbol match |
| \`exhaustive\` | Find ALL occurrences |
| \`semantic\` | Conceptual questions |

## Handle Notices from context()

- **[LESSONS_WARNING]** → Tell user about past mistakes BEFORE proceeding
- **[RULES_NOTICE]** → Run \`generate_rules()\`
- **[VERSION_NOTICE]** → Tell user to update MCP

## Fast Path (Simple Utilities Only)

Skip init/context ONLY for: "list workspaces", "show version", "list reminders"
→ Just call: \`workspace(action="list")\`, \`help(action="version")\`, etc.

Everything else = full protocol (init → context → search → work)

### Lessons (Past Mistakes)

- After \`init\`: Check for \`lessons\` field and apply before work
- Before risky work: \`session(action="get_lessons", query="<topic>")\`
- On mistakes: \`session(action="capture_lesson", title="...", trigger="...", impact="...", prevention="...")\`

### Context Pressure & Compaction

- If \`context\` returns high/critical \`context_pressure\`: call \`session(action="capture", ...)\` to save state
- PreCompact hooks automatically save snapshots before compaction

### Enhanced Context (Warnings)

\`context\` returns server-side \`warnings\` for lessons, risky actions, and breaking changes.
When warnings are present: **STOP**, acknowledge them, explain mitigation, then proceed.

### Automatic Context Restoration

**Context restoration is now enabled by default.** Every \`init\` call automatically:
- Restores context from recent snapshots (if available)
- Returns \`restored_context\` field with snapshot data
- Sets \`is_post_compact=true\` in response when restoration occurs

**No special handling needed after compaction** - just call \`init\` normally.

To disable automatic restoration:
- Pass \`is_post_compact=false\` in the API call
- Or set \`CONTEXTSTREAM_RESTORE_CONTEXT=false\` environment variable

### Notices - MUST HANDLE IMMEDIATELY

- **[VERSION_NOTICE]** or **[VERSION_CRITICAL]**: Tell user about the update in a helpful, non-annoying way. Provide update commands:
  - macOS/Linux: \`curl -fsSL https://contextstream.io/scripts/setup.sh | bash\`
  - Windows: \`irm https://contextstream.io/scripts/setup.ps1 | iex\`
  - npm: \`npm install -g @contextstream/mcp-server@latest\`
- **[RULES_NOTICE]**: Run \`generate_rules(overwrite_existing=true)\` to update
- **[LESSONS_WARNING]**: Read lessons, tell user about them, explain how you'll avoid past mistakes

### Plans & Tasks

When user asks for a plan, use ContextStream (not EnterPlanMode):
1. \`session(action="capture_plan", title="...", steps=[...])\`
2. \`memory(action="create_task", title="...", plan_id="<id>")\`

### Workspace-Only Mode (Multi-Project Folders)

If working in a parent folder containing multiple projects:
\`\`\`
init(folder_path="...", skip_project_creation=true)
\`\`\`

This enables workspace-level memory and context without project-specific indexing.
Use for monorepos or folders with multiple independent projects.

Full docs: https://contextstream.io/docs/mcp/tools
`.trim();

/**
 * NO-HOOKS SUPPLEMENT - Additional guidance for editors without hooks support.
 *
 * Editors like Codex, Aider, and Antigravity don't have hooks to enforce behavior,
 * so the rules file must contain explicit guidance for everything hooks would do:
 * - Session initialization (SessionStart hook)
 * - File indexing after changes (PostToolUse hook)
 * - Context compaction awareness (PreCompact hook)
 * - Search-first enforcement (PreToolUse hook)
 */
const NO_HOOKS_SUPPLEMENT = `

---
## ⚠️ IMPORTANT: No Hooks Available ⚠️

**This editor does NOT have hooks to enforce ContextStream behavior.**
You MUST follow these rules manually - there is no automatic enforcement.

---

## 🚀 SESSION START PROTOCOL

**On EVERY new session, you MUST:**

1. **Call \`init(folder_path="<project_path>")\`** FIRST
   - This triggers project indexing
   - Check response for \`indexing_status\`
   - If \`"started"\` or \`"refreshing"\`: wait before searching

2. **Generate a unique session_id** (e.g., \`"session-" + timestamp\` or a UUID)
   - Use this SAME session_id for ALL context() calls in this conversation
   - This groups all turns together in the transcript

3. **Call \`context(user_message="<first_message>", save_exchange=true, session_id="<your-session-id>")\`**
   - Gets task-specific rules and lessons
   - Check for [LESSONS_WARNING] - past mistakes to avoid
   - Check for [RULES_NOTICE] - update rules if needed
   - **save_exchange=true** saves each conversation turn for later retrieval

4. **NEVER skip init/context** - you will miss critical context

---

## 💾 AUTOMATIC TRANSCRIPT SAVING (CRITICAL)

**This editor does NOT have hooks to auto-save transcripts.**
You MUST save each conversation turn manually:

### On EVERY message (including the first):
\`\`\`
context(user_message="<user's message>", save_exchange=true, session_id="<session-id>")
\`\`\`

### Why save_exchange matters:
- Transcripts enable searching past conversations
- Allows context restoration after compaction
- Provides conversation history for debugging
- Required for the Transcripts page in the dashboard

### Session ID Guidelines:
- Generate ONCE at the start of the conversation
- Use a unique identifier: \`"session-" + Date.now()\` or a UUID
- Keep the SAME session_id for ALL context() calls in this session
- Different sessions = different transcripts

---

## 📁 FILE INDEXING (CRITICAL)

**There is NO automatic file indexing in this editor.**
You MUST manage indexing manually:

### After Creating/Editing Files:
\`\`\`
project(action="index")  # Re-index entire project
\`\`\`

### For Single File Updates:
\`\`\`
project(action="ingest_local", path="<file_path>")
\`\`\`

### Signs You Need to Re-index:
- Search doesn't find code you just wrote
- Search returns old versions of functions
- New files don't appear in search results

### Best Practice:
After completing a feature or making multiple file changes, ALWAYS run:
\`\`\`
project(action="index")
\`\`\`

---

## 🔍 SEARCH-FIRST (No PreToolUse Hook)

**There is NO hook to block local tools.** You MUST self-enforce:

### Before ANY Search, Check Index Status:
\`\`\`
project(action="index_status")
\`\`\`

This tells you:
- \`indexed\`: true/false - is project indexed?
- \`last_indexed_at\`: timestamp - when was it last indexed?
- \`file_count\`: number - how many files indexed?

### Search Protocol:

**IF project is indexed and fresh:**
\`\`\`
search(mode="hybrid", query="what you're looking for")
\`\`\`

**IF project is NOT indexed or very stale (>7 days):**
→ Use local tools (Glob/Grep/Read) directly
→ OR run \`project(action="index")\` first, then search

**IF ContextStream search returns 0 results or errors:**
→ Use local tools (Glob/Grep/Read) as fallback

### When Local Tools Are OK:
✅ Project is not indexed
✅ Index is stale/outdated (>7 days old)
✅ ContextStream search returns 0 results
✅ ContextStream returns errors
✅ User explicitly requests local tools

### When to Use ContextStream Search:
✅ Project is indexed and fresh
✅ Looking for code by meaning/concept
✅ Need semantic understanding

---

## 💾 CONTEXT COMPACTION (No PreCompact Hook)

**There is NO automatic state saving before compaction.**
You MUST save state manually when the conversation gets long:

### When to Save State:
- After completing a major task
- Before the conversation might be compacted
- If \`context()\` returns \`context_pressure.level: "high"\`

### How to Save State:
\`\`\`
session(action="capture", event_type="session_snapshot",
  title="Session checkpoint",
  content="{ \\"summary\\": \\"what we did\\", \\"active_files\\": [...], \\"next_steps\\": [...] }")
\`\`\`

### After Compaction (if context seems lost):
\`\`\`
init(folder_path="...", is_post_compact=true)
\`\`\`
This restores the most recent snapshot.

---

## 📋 PLANS & TASKS (No EnterPlanMode)

**Always use ContextStream for planning:**

\`\`\`
session(action="capture_plan", title="...", steps=[...])
memory(action="create_task", title="...", plan_id="...")
\`\`\`

❌ DO NOT use built-in plan mode or write plans to markdown files.

---

## 🔄 VERSION UPDATES (Check Periodically)

**This editor does NOT have hooks to check for updates automatically.**
You should check for updates using \`help(action="version")\` periodically (e.g., at session start).

### If the response includes [VERSION_NOTICE] or [VERSION_CRITICAL]:

**Tell the user** about the available update in a helpful, non-annoying way:
- Frame it as "new features and improvements available"
- Provide the update commands (user can choose their preferred method)
- Don't nag repeatedly - mention once, then only if user asks

### Update Commands (provide all options):

**macOS/Linux:**
\`\`\`bash
curl -fsSL https://contextstream.io/scripts/setup.sh | bash
\`\`\`

**Windows (PowerShell):**
\`\`\`powershell
irm https://contextstream.io/scripts/setup.ps1 | iex
\`\`\`

**npm (requires Node.js 18+):**
\`\`\`bash
npm install -g @contextstream/mcp-server@latest
\`\`\`

After updating, user should restart their AI tool.

---
`;

// Editors that don't have hooks support and need enhanced rules
const NO_HOOKS_EDITORS = ["codex", "aider", "antigravity"];

export const TEMPLATES: Record<string, RuleTemplate> = {
  codex: {
    filename: "AGENTS.md",
    description: "Codex CLI agent instructions",
    build: (rules) => `# Codex CLI Instructions
${rules}
`,
  },

  cursor: {
    filename: ".cursorrules",
    description: "Cursor AI rules",
    build: (rules) => `# Cursor Rules
${rules}
`,
  },

  cline: {
    filename: ".clinerules",
    description: "Cline AI rules",
    build: (rules) => `# Cline Rules
${rules}
`,
  },

  kilo: {
    filename: ".kilocode/rules/contextstream.md",
    description: "Kilo Code AI rules",
    build: (rules) => `# Kilo Code Rules
${rules}
`,
  },

  roo: {
    filename: ".roo/rules/contextstream.md",
    description: "Roo Code AI rules",
    build: (rules) => `# Roo Code Rules
${rules}
`,
  },

  claude: {
    filename: "CLAUDE.md",
    description: "Claude Code instructions",
    build: (rules) => `# Claude Code Instructions
${rules}
`,
  },

  aider: {
    filename: ".aider.conf.yml",
    description: "Aider configuration with system prompt",
    build: (rules) => `# Aider Configuration
# Note: Aider uses different config format - this adds to the system prompt

# Add ContextStream guidance to conventions
conventions: |
${rules
        .split("\n")
        .map((line) => "  " + line)
        .join("\n")}
`,
  },

  antigravity: {
    filename: "GEMINI.md",
    description: "Google Antigravity AI rules",
    build: (rules) => `# Antigravity Agent Rules
${rules}
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
    mode?: "dynamic" | "minimal" | "full" | "bootstrap";
  }
): { filename: string; content: string } | null {
  const template = getTemplate(editor);
  if (!template) return null;

  // Default to "bootstrap" mode - minimal but effective rules that ensure context() is called
  // Full rules are delivered dynamically via context()
  const mode = options?.mode || "bootstrap";
  const rules = mode === "full"
    ? CONTEXTSTREAM_RULES_FULL
    : mode === "minimal"
      ? CONTEXTSTREAM_RULES_MINIMAL
      : mode === "bootstrap"
        ? CONTEXTSTREAM_RULES_BOOTSTRAP
        : CONTEXTSTREAM_RULES_DYNAMIC;

  let content = template.build(rules);

  // Add workspace header if provided
  if (options?.workspaceName || options?.projectName) {
    const header = `
# Workspace: ${options.workspaceName || "Unknown"}
${options.projectName ? `# Project: ${options.projectName}` : ""}
${options.workspaceId ? `# Workspace ID: ${options.workspaceId}` : ""}

`;
    content = header + content;
  }

  // Add NO_HOOKS_SUPPLEMENT for editors without hooks support
  // These editors need explicit guidance since there's no enforcement mechanism
  if (NO_HOOKS_EDITORS.includes(editor.toLowerCase())) {
    content += NO_HOOKS_SUPPLEMENT;
  }

  // Append additional rules if provided
  if (options?.additionalRules) {
    content += "\n\n## Project-Specific Rules\n\n" + options.additionalRules;
  }

  // Claude Code requires `mcp__<server>__<tool>` naming convention for MCP tools.
  // Other MCP clients typically use raw tool names.
  if (editor.toLowerCase() === "claude") {
    content = applyMcpToolPrefix(content, `mcp__${DEFAULT_CLAUDE_MCP_SERVER_NAME}__`);
  }

  return {
    filename: template.filename,
    content: content.trim() + "\n",
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
  mode?: "dynamic" | "minimal" | "full" | "bootstrap";
}): Array<{ editor: string; filename: string; content: string }> {
  return getAvailableEditors()
    .map((editor) => {
      const result = generateRuleContent(editor, options);
      if (!result) return null;
      return { editor, ...result };
    })
    .filter((r): r is { editor: string; filename: string; content: string } => r !== null);
}
