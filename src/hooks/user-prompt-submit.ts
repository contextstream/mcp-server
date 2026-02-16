/**
 * ContextStream UserPromptSubmit Hook - Injects rules reminder + captures transcripts
 *
 * Injects a reminder about ContextStream rules on every message.
 * Supports multiple editor formats: Claude Code, Cursor, Cline/Roo/Kilo.
 *
 * TRANSCRIPT CAPTURE (Lagging):
 * - Extracts the previous user+assistant exchange from session history
 * - Saves that exchange to ContextStream when the next user message arrives
 * - Works for Claude Code (session.messages) and Cursor (history)
 * - The final exchange is captured by the session-end hook
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
import { VERSION, getUpdateNotice, getVersionNoticeForHook, type VersionNotice } from "../version.js";
import { cleanupStale, markContextRequired, markInitRequired } from "./prompt-state.js";


const ENABLED = process.env.CONTEXTSTREAM_REMINDER_ENABLED !== "false";

let API_URL = process.env.CONTEXTSTREAM_API_URL || "https://api.contextstream.io";
let API_KEY = process.env.CONTEXTSTREAM_API_KEY || "";
let WORKSPACE_ID: string | null = null;
let PROJECT_ID: string | null = null;

// Compact reminder for Claude Code (full rules already in CLAUDE.md)
const REMINDER = `[CONTEXTSTREAM] On the first message in every session call mcp__contextstream__init(...), then call mcp__contextstream__context(user_message="...", save_exchange=true, session_id="<session-id>") FIRST before any other tool. On subsequent messages, default to context first. Narrow bypass is allowed only for immediate read-only ContextStream calls when prior context is fresh and no state-changing tool has run. Response contains dynamic rules, lessons, preferences.
COMMON MEMORY CALLS: list docs via memory(action="list_docs"), list lessons via session(action="get_lessons"), list plans via session(action="list_plans"), list tasks/todos via memory(action="list_tasks"|"list_todos").
[END]`;

// Full reminder for non-Claude editors that don't have CLAUDE.md context
const FULL_REMINDER = `[CONTEXTSTREAM RULES - MANDATORY]

1. FIRST MESSAGE IN SESSION: Call mcp__contextstream__init(...) then mcp__contextstream__context(user_message="...", save_exchange=true, session_id="<session-id>")
   - Returns: dynamic rules, lessons from past mistakes, relevant context
   - Check response for: [LESSONS_WARNING], [RULES_NOTICE], preferences
   - save_exchange=true saves each conversation turn for later retrieval
   - Use a consistent session_id for the entire conversation (generate once on first message)
   - On subsequent messages, default to context() first. Narrow bypass: immediate read-only ContextStream calls when context is fresh and no state-changing tool has run.

2. FOR CODE SEARCH: Check index status, then search appropriately
   ⚠️ BEFORE searching: mcp__contextstream__project(action="index_status")
   ✅ IF indexed & fresh: Use mcp__contextstream__search(mode="auto", query="...")
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
const ENHANCED_REMINDER_HEADER = `⬡ ContextStream — Smart Context & Memory

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
  preferences?: Array<{ title: string; content: string; importance: string }>;
}

interface TranscriptMessage {
  role: string;
  content: string;
  timestamp: string;
}

interface LastExchange {
  userMessage: TranscriptMessage;
  assistantMessage: TranscriptMessage;
  sessionId?: string;
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

/**
 * Read messages from Claude Code's JSONL transcript file.
 * Each line is a JSON object with {type, message, ...}
 */
function readTranscriptFile(transcriptPath: string): Array<{ role: string; content: string }> {
  try {
    const content = fs.readFileSync(transcriptPath, "utf-8");
    const lines = content.trim().split("\n");
    const messages: Array<{ role: string; content: string }> = [];

    for (const line of lines) {
      try {
        const entry = JSON.parse(line);
        // Only include user and assistant messages (skip progress, summary, etc.)
        if (entry.type === "user" || entry.type === "assistant") {
          const msg = entry.message;
          if (msg?.role && msg?.content) {
            // Extract text content from array format
            let textContent = "";
            if (typeof msg.content === "string") {
              textContent = msg.content;
            } else if (Array.isArray(msg.content)) {
              textContent = msg.content
                .filter((c: { type: string; text?: string }) => c.type === "text" && c.text)
                .map((c: { text: string }) => c.text)
                .join("\n");
            }
            if (textContent) {
              messages.push({ role: msg.role, content: textContent });
            }
          }
        }
      } catch {
        // Skip malformed lines
      }
    }
    return messages;
  } catch {
    return [];
  }
}

/**
 * Extract the last complete exchange (user message + assistant response) from session history.
 * This enables "lagging" transcript capture - we save the previous exchange when the next user message arrives.
 */
function extractLastExchange(input: HookInput, editorFormat: string): LastExchange | null {
  try {
    // Claude Code with transcript_path (newer format)
    if (editorFormat === "claude" && input.transcript_path) {
      const messages = readTranscriptFile(input.transcript_path);
      if (messages.length < 2) return null;

      // Find the last assistant message and its preceding user message
      let lastAssistantIdx = -1;
      for (let i = messages.length - 1; i >= 0; i--) {
        if (messages[i].role === "assistant") {
          lastAssistantIdx = i;
          break;
        }
      }

      if (lastAssistantIdx < 1) return null;

      let lastUserIdx = -1;
      for (let i = lastAssistantIdx - 1; i >= 0; i--) {
        if (messages[i].role === "user") {
          lastUserIdx = i;
          break;
        }
      }

      if (lastUserIdx < 0) return null;

      const now = new Date().toISOString();
      return {
        userMessage: { role: "user", content: messages[lastUserIdx].content, timestamp: now },
        assistantMessage: { role: "assistant", content: messages[lastAssistantIdx].content, timestamp: now },
        sessionId: input.session_id,
      };
    }

    // Claude Code with session.messages (older format, kept for compatibility)
    if (editorFormat === "claude" && input.session?.messages) {
      const messages = input.session.messages;
      if (messages.length < 2) return null;

      // Find the last assistant message and its preceding user message
      let lastAssistantIdx = -1;
      for (let i = messages.length - 1; i >= 0; i--) {
        if (messages[i].role === "assistant") {
          lastAssistantIdx = i;
          break;
        }
      }

      if (lastAssistantIdx < 1) return null;

      // Find the user message before this assistant message
      let lastUserIdx = -1;
      for (let i = lastAssistantIdx - 1; i >= 0; i--) {
        if (messages[i].role === "user") {
          lastUserIdx = i;
          break;
        }
      }

      if (lastUserIdx < 0) return null;

      const userMsg = messages[lastUserIdx];
      const assistantMsg = messages[lastAssistantIdx];

      // Extract text content (handles both string and array formats)
      const extractContent = (content: string | Array<{ type: string; text?: string }>): string => {
        if (typeof content === "string") return content;
        if (Array.isArray(content)) {
          return content
            .filter((c) => c.type === "text" && c.text)
            .map((c) => c.text)
            .join("\n");
        }
        return "";
      };

      const userContent = extractContent(userMsg.content);
      const assistantContent = extractContent(assistantMsg.content);

      if (!userContent || !assistantContent) return null;

      const now = new Date().toISOString();
      return {
        userMessage: { role: "user", content: userContent, timestamp: now },
        assistantMessage: { role: "assistant", content: assistantContent, timestamp: now },
        sessionId: input.session_id,
      };
    }

    if ((editorFormat === "cursor" || editorFormat === "antigravity") && input.history) {
      const history = input.history;
      if (history.length < 2) return null;

      // Find the last assistant message and its preceding user message
      let lastAssistantIdx = -1;
      for (let i = history.length - 1; i >= 0; i--) {
        if (history[i].role === "assistant") {
          lastAssistantIdx = i;
          break;
        }
      }

      if (lastAssistantIdx < 1) return null;

      let lastUserIdx = -1;
      for (let i = lastAssistantIdx - 1; i >= 0; i--) {
        if (history[i].role === "user") {
          lastUserIdx = i;
          break;
        }
      }

      if (lastUserIdx < 0) return null;

      const now = new Date().toISOString();
      return {
        userMessage: { role: "user", content: history[lastUserIdx].content, timestamp: now },
        assistantMessage: { role: "assistant", content: history[lastAssistantIdx].content, timestamp: now },
        sessionId: input.conversationId || input.session_id,
      };
    }

    // Cline/Roo/Kilo - check if they provide history in a different format
    // For now, return null - can be extended when we discover their format
    return null;
  } catch {
    return null;
  }
}

/**
 * Save the last exchange to ContextStream transcripts API.
 * Uses the simplified /exchange endpoint - backend handles all transcript logic.
 * This runs asynchronously and doesn't block the hook response.
 */
async function saveLastExchange(exchange: LastExchange, cwd: string, clientName?: string): Promise<void> {
  if (!API_KEY) return;

  // Generate a session ID based on cwd if not provided
  const sessionId = exchange.sessionId || `hook-${Buffer.from(cwd).toString("base64").slice(0, 16)}`;

  // Simple payload - backend handles find-or-create, appending, etc.
  const payload: Record<string, unknown> = {
    session_id: sessionId,
    user_message: exchange.userMessage.content,
    assistant_message: exchange.assistantMessage.content,
    client_name: clientName,
  };

  if (WORKSPACE_ID) {
    payload.workspace_id = WORKSPACE_ID;
  }
  if (PROJECT_ID) {
    payload.project_id = PROJECT_ID;
  }

  try {
    const controller = new AbortController();
    const timeoutId = setTimeout(() => controller.abort(), 5000);

    await fetch(`${API_URL}/api/v1/transcripts/exchange`, {
      method: "POST",
      headers: {
        "Content-Type": "application/json",
        "X-API-Key": API_KEY,
      },
      body: JSON.stringify(payload),
      signal: controller.signal,
    });

    clearTimeout(timeoutId);
  } catch {
    // Silently ignore errors - don't block the hook
  }
}

/**
 * Fast hook context fetch from /api/v1/context/hook (Redis-cached, ~20-50ms).
 * Returns compact context string with preferences + lessons + core rules.
 */
async function fetchHookContext(): Promise<string | null> {
  if (!API_KEY) return null;

  try {
    const controller = new AbortController();
    const timeoutId = setTimeout(() => controller.abort(), 2000);

    const url = `${API_URL}/api/v1/context/hook`;
    const body: Record<string, unknown> = {};
    if (WORKSPACE_ID) body.workspace_id = WORKSPACE_ID;
    if (PROJECT_ID) body.project_id = PROJECT_ID;

    const response = await fetch(url, {
      method: "POST",
      headers: {
        "X-API-Key": API_KEY,
        "Content-Type": "application/json",
      },
      body: JSON.stringify(body),
      signal: controller.signal,
    });

    clearTimeout(timeoutId);

    if (response.ok) {
      const data = (await response.json()) as { success?: boolean; data?: { context?: string } };
      return data?.data?.context || null;
    }
    return null;
  } catch {
    return null;
  }
}

async function fetchSessionContext(): Promise<ContextResponse | null> {
  if (!API_KEY) return null;

  try {
    const controller = new AbortController();
    const timeoutId = setTimeout(() => controller.abort(), 3000);

    // Use POST /context/smart which is the main context endpoint
    const url = `${API_URL}/api/v1/context/smart`;
    const body: Record<string, unknown> = {
      user_message: "hook context fetch",
      max_tokens: 200,
      format: "readable",
    };
    if (WORKSPACE_ID) body.workspace_id = WORKSPACE_ID;
    if (PROJECT_ID) body.project_id = PROJECT_ID;

    const response = await fetch(url, {
      method: "POST",
      headers: {
        "X-API-Key": API_KEY,
        "Content-Type": "application/json",
      },
      body: JSON.stringify(body),
      signal: controller.signal,
    });

    clearTimeout(timeoutId);

    if (response.ok) {
      const data = await response.json();
      // Transform smart context response to our ContextResponse format
      // The context string may contain encoded preferences, lessons etc.
      return transformSmartContextResponse(data);
    }
    return null;
  } catch {
    return null;
  }
}

/**
 * Transform the smart context API response into our ContextResponse format.
 * The smart context endpoint returns a minified context string and metadata.
 */
function transformSmartContextResponse(data: unknown): ContextResponse | null {
  try {
    const response = data as {
      data?: {
        warnings?: string[];
        context?: string;
        items?: Array<{
          item_type: string;
          title: string;
          content: string;
          metadata?: { importance?: string };
        }>;
      };
    };

    const result: ContextResponse = {};

    // Extract lessons from warnings
    if (response.data?.warnings && response.data.warnings.length > 0) {
      result.lessons = response.data.warnings.map((w) => ({
        title: "Lesson",
        trigger: "",
        prevention: w.replace(/^\[LESSONS_WARNING\]\s*/, ""),
      }));
    }

    // Extract items by type (if available in response)
    if (response.data?.items) {
      for (const item of response.data.items) {
        if (item.item_type === "preference") {
          if (!result.preferences) result.preferences = [];
          result.preferences.push({
            title: item.title,
            content: item.content,
            importance: item.metadata?.importance || "medium",
          });
        } else if (item.item_type === "plan") {
          if (!result.active_plans) result.active_plans = [];
          result.active_plans.push({
            title: item.title,
            status: "active",
          });
        } else if (item.item_type === "task") {
          if (!result.pending_tasks) result.pending_tasks = [];
          result.pending_tasks.push({
            title: item.title,
            status: "pending",
          });
        } else if (item.item_type === "reminder") {
          if (!result.reminders) result.reminders = [];
          result.reminders.push({
            title: item.title,
            content: item.content,
          });
        }
      }
    }

    return result;
  } catch {
    return null;
  }
}

function buildEnhancedReminder(
  ctx: ContextResponse | null,
  isNewSession: boolean,
  versionNotice?: VersionNotice | null
): string {
  const parts: string[] = [ENHANCED_REMINDER_HEADER];

  // Add version notice prominently if outdated
  if (versionNotice?.behind) {
    const versionInfo = getVersionNoticeForHook(versionNotice);
    if (versionInfo) {
      parts.push(`## 🔄 UPDATE AVAILABLE\n`);
      parts.push(versionInfo);
      parts.push("");
    }
  }

  // Session init guidance for new sessions
  if (isNewSession) {
    parts.push(`## 🚀 NEW SESSION DETECTED
1. Call \`init(folder_path="...")\` - this triggers project indexing
2. Wait for indexing: if \`init\` returns \`indexing_status: "started"\`, files are being indexed
3. Generate a unique session_id (e.g., "session-" + timestamp or UUID) - use this for ALL context() calls
4. Call \`context(user_message="...", save_exchange=true, session_id="<your-session-id>")\` for task-specific context
5. Use \`search(mode="auto")\` for code discovery (not Glob/Grep/Read)

`);
  }

  // High-importance preferences (always respect these)
  const highImportancePrefs = ctx?.preferences?.filter(p => p.importance === "high") || [];
  if (highImportancePrefs.length > 0) {
    parts.push(`## ⚙️ USER PREFERENCES - Always respect these`);
    for (const pref of highImportancePrefs.slice(0, 5)) {
      parts.push(`- **${pref.title}**: ${pref.content}`);
    }
    parts.push("");
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

  // Add separator and standard rules (full version for non-Claude editors)
  parts.push("---\n");
  parts.push(FULL_REMINDER);

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
→ Use \`search(mode="auto", query="...")\`

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
  session_id?: string;
  transcript_path?: string; // Path to JSONL transcript file
  session?: {
    messages?: Array<{ role: string; content: string | Array<{ type: string; text?: string }> }>;
  };

  // Cline/Roo/Kilo format
  hookName?: string;

  // Cursor format
  history?: Array<{ role: string; content: string }>;

  // Common session tracking
  conversationId?: string; // Cursor/Cline may use this
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
  cleanupStale(180);
  markContextRequired(cwd);
  if (isNewSession(input, editorFormat)) {
    markInitRequired(cwd);
  }

  // Output format depends on editor
  if (editorFormat === "claude") {
    // ==========================================
    // CLAUDE CODE: FAST PATH (~20-50ms)
    // ==========================================
    // Makes a single fast HTTP call to /context/hook (Redis-cached).
    // Returns preferences + lessons + core rules.
    // Falls back to static reminder if API is unreachable.
    loadConfigFromMcpJson(cwd);

    let context = REMINDER;
    if (API_KEY) {
      try {
        const hookContext = await fetchHookContext();
        if (hookContext) {
          context = hookContext;
        }
      } catch {
        // Fallback to static reminder on any error
      }
    }

    console.log(
      JSON.stringify({
        hookSpecificOutput: {
          hookEventName: "UserPromptSubmit",
          additionalContext: context,
        },
      })
    );
  } else {
    // ==========================================
    // NON-CLAUDE EDITORS: Keep context fetch
    // ==========================================
    // Cursor/Cline/Antigravity/etc. don't have SessionStart or CLAUDE.md,
    // so they need the full reminder + context from the hook.
    // But we still skip JSONL reading (not applicable to these editors).
    loadConfigFromMcpJson(cwd);

    const versionNoticePromise = getUpdateNotice();

    // Save exchanges for editors that provide history in-memory (no JSONL)
    const lastExchange = extractLastExchange(input, editorFormat);
    const clientName = editorFormat;
    const saveExchangePromise = lastExchange ? saveLastExchange(lastExchange, cwd, clientName) : Promise.resolve();

    const newSession = isNewSession(input, editorFormat);
    const [ctx, versionNotice] = await Promise.all([fetchSessionContext(), versionNoticePromise, saveExchangePromise]);

    if (editorFormat === "cursor") {
      // Cursor has limited injection capability, so we use a shorter version
      let cursorReminder = ctx?.lessons?.length
        ? `[CONTEXTSTREAM] ⚠️ ${ctx.lessons.length} lessons from past mistakes. Call context(save_exchange=true, session_id="...") FIRST. Use search(mode="auto") before Glob/Grep. After file edits: project(action="index").`
        : `[CONTEXTSTREAM] Call context(save_exchange=true, session_id="...") FIRST. Use search(mode="auto") before Glob/Grep/Read. After file edits: project(action="index").`;

      if (versionNotice?.behind) {
        cursorReminder += ` [UPDATE v${versionNotice.current}→${versionNotice.latest}]`;
      }

      console.log(
        JSON.stringify({
          continue: true,
          user_message: cursorReminder,
        })
      );
    } else {
      // Cline/Roo/Kilo/Antigravity - full enhanced reminder
      const enhancedReminder = buildEnhancedReminder(ctx, newSession, versionNotice);
      console.log(
        JSON.stringify({
          cancel: false,
          contextModification: enhancedReminder,
        })
      );
    }
  }

  process.exit(0);
}

// Auto-run if executed directly
const isDirectRun =
  process.argv[1]?.includes("user-prompt-submit") || process.argv[2] === "user-prompt-submit";
if (isDirectRun) {
  runUserPromptSubmitHook().catch(() => process.exit(0));
}
