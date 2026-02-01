/**
 * ContextStream UserPromptSubmit Hook - Injects rules reminder
 *
 * Injects a reminder about ContextStream rules on every message.
 * Supports multiple editor formats: Claude Code, Cursor, Cline/Roo/Kilo.
 *
 * For non-Claude editors (Cursor, Cline, Roo, Kilo), this hook does more work
 * to compensate for missing hooks (SessionStart, PostToolUse, PreCompact, Stop):
 * - Detects new sessions and injects init guidance
 * - Fetches context from ContextStream API
 * - Includes lessons, plans, and tasks in the reminder
 *
 * Usage:
 *   npx @contextstream/mcp-server hook user-prompt-submit
 *
 * Input (stdin): JSON hook event data
 * Output (stdout): JSON with hookSpecificOutput/contextModification
 * Exit: Always 0
 */

import * as fs from "node:fs";
import * as path from "node:path";
import { homedir } from "node:os";

const ENABLED = process.env.CONTEXTSTREAM_REMINDER_ENABLED !== "false";

let API_URL = process.env.CONTEXTSTREAM_API_URL || "https://api.contextstream.io";
let API_KEY = process.env.CONTEXTSTREAM_API_KEY || "";
let WORKSPACE_ID: string | null = null;
let PROJECT_ID: string | null = null;

const REMINDER = `[CONTEXTSTREAM RULES - MANDATORY]

1. FIRST: Call mcp__contextstream__context(user_message="...") before ANY other tool
   - Returns: dynamic rules, lessons from past mistakes, relevant context
   - Check response for: [LESSONS_WARNING], [RULES_NOTICE], preferences

2. FOR CODE SEARCH: Check index status, then search appropriately
   ⚠️ BEFORE searching: mcp__contextstream__project(action="index_status")
   ✅ IF indexed & fresh: Use mcp__contextstream__search(mode="hybrid", query="...")
   ✅ IF NOT indexed OR stale: Use local tools (Glob/Grep/Read) directly
   ✅ IF search returns 0 results: Fallback to local tools (Glob/Grep/Read)

3. WHEN LOCAL TOOLS (Glob/Grep/Read) ARE OK:
   ✅ Project is NOT indexed (index_status.indexed=false)
   ✅ Index is stale/outdated (>7 days old)
   ✅ ContextStream search returns 0 results or errors
   ✅ User explicitly requests local tools

4. FOR PLANS & TASKS: Use ContextStream, not file-based plans
   ✅ Plans: mcp__contextstream__session(action="capture_plan", ...)
   ✅ Tasks: mcp__contextstream__memory(action="create_task", ...)
   ❌ DO NOT use EnterPlanMode or write plans to markdown files

5. CHECK THESE from context() response:
   - Lessons: Past mistakes to avoid (shown as warnings)
   - Reminders: Active reminders for this project
   - Preferences: User's coding style and preferences
   - Rules: Dynamic rules matched to current task

6. SKIP CONTEXTSTREAM: If user preference says "skip contextstream", use local tools instead
[END]`;

// Enhanced reminder for non-Claude editors (compensates for missing hooks)
const ENHANCED_REMINDER_HEADER = `[CONTEXTSTREAM - ENHANCED CONTEXT]

`;

interface McpConfig {
  mcpServers?: {
    contextstream?: {
      env?: {
        CONTEXTSTREAM_API_KEY?: string;
        CONTEXTSTREAM_API_URL?: string;
        CONTEXTSTREAM_WORKSPACE_ID?: string;
      };
    };
  };
}

interface LocalConfig {
  workspace_id?: string;
  project_id?: string;
}

interface ContextResponse {
  lessons?: Array<{ title: string; trigger: string; prevention: string }>;
  recent_decisions?: Array<{ title: string; content: string }>;
  active_plans?: Array<{ title: string; status: string }>;
  pending_tasks?: Array<{ title: string; status: string }>;
  reminders?: Array<{ title: string; content: string }>;
}

function loadConfigFromMcpJson(cwd: string): void {
  let searchDir = path.resolve(cwd);

  for (let i = 0; i < 5; i++) {
    if (!API_KEY) {
      const mcpPath = path.join(searchDir, ".mcp.json");
      if (fs.existsSync(mcpPath)) {
        try {
          const content = fs.readFileSync(mcpPath, "utf-8");
          const config = JSON.parse(content) as McpConfig;
          const csEnv = config.mcpServers?.contextstream?.env;
          if (csEnv?.CONTEXTSTREAM_API_KEY) {
            API_KEY = csEnv.CONTEXTSTREAM_API_KEY;
          }
          if (csEnv?.CONTEXTSTREAM_API_URL) {
            API_URL = csEnv.CONTEXTSTREAM_API_URL;
          }
          if (csEnv?.CONTEXTSTREAM_WORKSPACE_ID) {
            WORKSPACE_ID = csEnv.CONTEXTSTREAM_WORKSPACE_ID;
          }
        } catch {
          // Continue
        }
      }
    }

    if (!WORKSPACE_ID || !PROJECT_ID) {
      const csConfigPath = path.join(searchDir, ".contextstream", "config.json");
      if (fs.existsSync(csConfigPath)) {
        try {
          const content = fs.readFileSync(csConfigPath, "utf-8");
          const csConfig = JSON.parse(content) as LocalConfig;
          if (csConfig.workspace_id && !WORKSPACE_ID) {
            WORKSPACE_ID = csConfig.workspace_id;
          }
          if (csConfig.project_id && !PROJECT_ID) {
            PROJECT_ID = csConfig.project_id;
          }
        } catch {
          // Continue
        }
      }
    }

    const parentDir = path.dirname(searchDir);
    if (parentDir === searchDir) break;
    searchDir = parentDir;
  }

  // Check home .mcp.json
  if (!API_KEY) {
    const homeMcpPath = path.join(homedir(), ".mcp.json");
    if (fs.existsSync(homeMcpPath)) {
      try {
        const content = fs.readFileSync(homeMcpPath, "utf-8");
        const config = JSON.parse(content) as McpConfig;
        const csEnv = config.mcpServers?.contextstream?.env;
        if (csEnv?.CONTEXTSTREAM_API_KEY) {
          API_KEY = csEnv.CONTEXTSTREAM_API_KEY;
        }
        if (csEnv?.CONTEXTSTREAM_API_URL) {
          API_URL = csEnv.CONTEXTSTREAM_API_URL;
        }
      } catch {
        // Ignore
      }
    }
  }
}

async function fetchSessionContext(): Promise<ContextResponse | null> {
  if (!API_KEY) return null;

  try {
    const controller = new AbortController();
    const timeoutId = setTimeout(() => controller.abort(), 3000);

    const url = new URL(`${API_URL}/api/v1/context`);
    if (WORKSPACE_ID) url.searchParams.set("workspace_id", WORKSPACE_ID);
    if (PROJECT_ID) url.searchParams.set("project_id", PROJECT_ID);
    url.searchParams.set("include_lessons", "true");
    url.searchParams.set("include_decisions", "true");
    url.searchParams.set("include_plans", "true");
    url.searchParams.set("include_reminders", "true");
    url.searchParams.set("limit", "3");

    const response = await fetch(url.toString(), {
      method: "GET",
      headers: {
        "X-API-Key": API_KEY,
      },
      signal: controller.signal,
    });

    clearTimeout(timeoutId);

    if (response.ok) {
      return (await response.json()) as ContextResponse;
    }
    return null;
  } catch {
    return null;
  }
}

function buildEnhancedReminder(
  ctx: ContextResponse | null,
  isNewSession: boolean
): string {
  const parts: string[] = [ENHANCED_REMINDER_HEADER];

  // Session init guidance for new sessions
  if (isNewSession) {
    parts.push(`## 🚀 NEW SESSION DETECTED
1. Call \`init(folder_path="...")\` - this triggers project indexing
2. Wait for indexing: if \`init\` returns \`indexing_status: "started"\`, files are being indexed
3. Then call \`context(user_message="...")\` for task-specific context
4. Use \`search(mode="hybrid")\` for code discovery (not Glob/Grep/Read)

`);
  }

  // Lessons (most important - mimics what SessionStart would show)
  if (ctx?.lessons && ctx.lessons.length > 0) {
    parts.push(`## ⚠️ LESSONS FROM PAST MISTAKES`);
    for (const lesson of ctx.lessons.slice(0, 3)) {
      parts.push(`- **${lesson.title}**: ${lesson.prevention}`);
    }
    parts.push("");
  }

  // Active plans (so AI knows what's in progress)
  if (ctx?.active_plans && ctx.active_plans.length > 0) {
    parts.push(`## 📋 Active Plans`);
    for (const plan of ctx.active_plans.slice(0, 3)) {
      parts.push(`- ${plan.title} (${plan.status})`);
    }
    parts.push("");
  }

  // Pending tasks
  if (ctx?.pending_tasks && ctx.pending_tasks.length > 0) {
    parts.push(`## ✅ Pending Tasks`);
    for (const task of ctx.pending_tasks.slice(0, 5)) {
      parts.push(`- ${task.title}`);
    }
    parts.push("");
  }

  // Reminders
  if (ctx?.reminders && ctx.reminders.length > 0) {
    parts.push(`## 🔔 Reminders`);
    for (const reminder of ctx.reminders.slice(0, 3)) {
      parts.push(`- ${reminder.title}`);
    }
    parts.push("");
  }

  // Add separator and standard rules
  parts.push("---\n");
  parts.push(REMINDER);

  // Add comprehensive file indexing guidance (critical for non-Claude editors)
  parts.push(`

---
## 🚨 FILE INDEXING & SEARCH - CRITICAL (No PostToolUse Hook) 🚨

**This editor does NOT have automatic file indexing after Edit/Write.**

### ⚠️ BEFORE ANY SEARCH - Check Index Status:
\`\`\`
project(action="index_status")
\`\`\`
Returns: \`indexed\` (true/false), \`last_indexed_at\`, \`file_count\`

### 🔍 Search Decision Tree:

**IF indexed=true AND last_indexed_at is recent:**
→ Use \`search(mode="hybrid", query="...")\`

**IF indexed=false OR last_indexed_at is stale (>7 days):**
→ Use local tools (Glob/Grep/Read) directly
→ OR run \`project(action="index")\` first, then search

**IF search returns 0 results or errors:**
→ Fallback to local tools (Glob/Grep/Read)

### ✅ When Local Tools (Glob/Grep/Read) Are OK:
- Project is NOT indexed
- Index is stale/outdated (>7 days)
- ContextStream search returns 0 results
- ContextStream returns errors
- User explicitly requests local tools

### On Session Start:
1. Call \`init(folder_path="...")\` - triggers initial indexing
2. Check \`project(action="index_status")\` before searching
3. If not indexed: use local tools OR wait for indexing

### After File Changes (Edit/Write/Create):
Files are NOT auto-indexed. You MUST:
1. After significant edits: \`project(action="index")\`
2. For single file: \`project(action="ingest_local", path="<file>")\`
3. Then search will find your changes`);

  return parts.join("\n");
}

interface HookInput {
  // Claude Code format
  hook_event_name?: string;
  prompt?: string;
  cwd?: string;
  session?: {
    messages?: Array<{ role: string; content: string | Array<{ type: string; text?: string }> }>;
  };

  // Cline/Roo/Kilo format
  hookName?: string;

  // Cursor format
  history?: Array<{ role: string; content: string }>;
}

function detectEditorFormat(input: HookInput): "claude" | "cline" | "cursor" | "antigravity" {
  // Cline/Roo/Kilo format
  if (input.hookName !== undefined) {
    return "cline";
  }
  // Cursor format (uses hook_event_name with different casing/structure)
  if (input.hook_event_name === "beforeSubmitPrompt") {
    return "cursor";
  }
  // Antigravity/Gemini format (check for gemini-specific fields)
  if (input.hook_event_name === "beforeAgentAction" || input.hook_event_name === "onPromptSubmit") {
    return "antigravity";
  }
  // Default to Claude Code format
  return "claude";
}

function isNewSession(input: HookInput, editorFormat: string): boolean {
  // Check Claude Code format - no prior messages or just 1 (the current one)
  if (editorFormat === "claude" && input.session?.messages) {
    return input.session.messages.length <= 1;
  }

  // Check Cursor format - no history or empty history
  if (editorFormat === "cursor" && input.history !== undefined) {
    return input.history.length === 0;
  }

  // Check Antigravity format - no history or empty history
  if (editorFormat === "antigravity" && input.history !== undefined) {
    return input.history.length === 0;
  }

  // For Cline/Roo/Kilo, we can't easily detect new session from input
  // So we'll check if init was called recently via a simple heuristic
  // For now, assume not new session to avoid spamming
  return false;
}

export async function runUserPromptSubmitHook(): Promise<void> {
  if (!ENABLED) {
    process.exit(0);
  }

  // Read stdin
  let inputData = "";
  for await (const chunk of process.stdin) {
    inputData += chunk;
  }

  if (!inputData.trim()) {
    process.exit(0);
  }

  let input: HookInput;
  try {
    input = JSON.parse(inputData);
  } catch {
    process.exit(0);
  }

  const editorFormat = detectEditorFormat(input);
  const cwd = input.cwd || process.cwd();

  // Output format depends on editor
  if (editorFormat === "claude") {
    // Claude Code format - simple reminder (other hooks handle the rest)
    console.log(
      JSON.stringify({
        hookSpecificOutput: {
          hookEventName: "UserPromptSubmit",
          additionalContext: REMINDER,
        },
      })
    );
  } else if (editorFormat === "cline") {
    // Cline/Roo/Kilo format - ENHANCED (compensates for missing hooks)
    loadConfigFromMcpJson(cwd);
    const newSession = isNewSession(input, editorFormat);
    const ctx = await fetchSessionContext();
    const enhancedReminder = buildEnhancedReminder(ctx, newSession);

    console.log(
      JSON.stringify({
        cancel: false,
        contextModification: enhancedReminder,
      })
    );
  } else if (editorFormat === "cursor") {
    // Cursor format - ENHANCED (compensates for missing hooks)
    loadConfigFromMcpJson(cwd);
    const newSession = isNewSession(input, editorFormat);
    const ctx = await fetchSessionContext();

    // Cursor has limited injection capability, so we use a shorter version
    const cursorReminder = ctx?.lessons?.length
      ? `[CONTEXTSTREAM] ⚠️ ${ctx.lessons.length} lessons from past mistakes. Use search(mode="hybrid") before Glob/Grep. Call context() first. After file edits: project(action="index") to re-index.`
      : `[CONTEXTSTREAM] Use search(mode="hybrid") before Glob/Grep/Read. Call context() first. After file edits: project(action="index") to re-index.`;

    console.log(
      JSON.stringify({
        continue: true,
        user_message: cursorReminder,
      })
    );
  } else if (editorFormat === "antigravity") {
    // Antigravity/Gemini format - ENHANCED (compensates for missing hooks)
    loadConfigFromMcpJson(cwd);
    const newSession = isNewSession(input, editorFormat);
    const ctx = await fetchSessionContext();
    const enhancedReminder = buildEnhancedReminder(ctx, newSession);

    // Antigravity uses similar format to Cline
    console.log(
      JSON.stringify({
        cancel: false,
        contextModification: enhancedReminder,
      })
    );
  }

  process.exit(0);
}

// Auto-run if executed directly
const isDirectRun =
  process.argv[1]?.includes("user-prompt-submit") || process.argv[2] === "user-prompt-submit";
if (isDirectRun) {
  runUserPromptSubmitHook().catch(() => process.exit(0));
}
