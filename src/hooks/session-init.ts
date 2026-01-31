/**
 * ContextStream session-init Hook - Full context injection on session start
 *
 * SessionStart hook that injects full context when a new Claude Code session begins.
 * Fetches workspace context, recent decisions, and active plans.
 *
 * Usage:
 *   npx @contextstream/mcp-server hook session-init
 *
 * Input (stdin): JSON with session_id, cwd
 * Output (stdout): JSON with hookSpecificOutput containing context
 * Exit: Always 0
 */

import * as fs from "node:fs";
import * as path from "node:path";
import { homedir } from "node:os";

const ENABLED = process.env.CONTEXTSTREAM_SESSION_INIT_ENABLED !== "false";

let API_URL = process.env.CONTEXTSTREAM_API_URL || "https://api.contextstream.io";
let API_KEY = process.env.CONTEXTSTREAM_API_KEY || "";
let WORKSPACE_ID: string | null = null;
let PROJECT_ID: string | null = null;

interface HookInput {
  session_id?: string;
  cwd?: string;
  hook_event_name?: string;
}

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
  rules?: string;
  lessons?: Array<{ title: string; trigger: string; prevention: string }>;
  recent_decisions?: Array<{ title: string; content: string }>;
  active_plans?: Array<{ title: string; status: string }>;
  pending_tasks?: Array<{ title: string; status: string }>;
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
    const timeoutId = setTimeout(() => controller.abort(), 5000);

    const url = new URL(`${API_URL}/api/v1/context`);
    if (WORKSPACE_ID) url.searchParams.set("workspace_id", WORKSPACE_ID);
    if (PROJECT_ID) url.searchParams.set("project_id", PROJECT_ID);
    url.searchParams.set("include_rules", "true");
    url.searchParams.set("include_lessons", "true");
    url.searchParams.set("include_decisions", "true");
    url.searchParams.set("include_plans", "true");
    url.searchParams.set("limit", "5");

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

function formatContext(ctx: ContextResponse | null): string {
  if (!ctx) {
    return `[ContextStream Session Start]

No stored context found. Call \`mcp__contextstream__context(user_message="starting new session")\` to initialize.`;
  }

  const parts: string[] = ["[ContextStream Session Start]"];

  // Lessons (most important)
  if (ctx.lessons && ctx.lessons.length > 0) {
    parts.push("\n## ⚠️ Lessons from Past Mistakes");
    for (const lesson of ctx.lessons.slice(0, 3)) {
      parts.push(`- **${lesson.title}**: ${lesson.prevention}`);
    }
  }

  // Active plans
  if (ctx.active_plans && ctx.active_plans.length > 0) {
    parts.push("\n## 📋 Active Plans");
    for (const plan of ctx.active_plans.slice(0, 3)) {
      parts.push(`- ${plan.title} (${plan.status})`);
    }
  }

  // Pending tasks
  if (ctx.pending_tasks && ctx.pending_tasks.length > 0) {
    parts.push("\n## ✅ Pending Tasks");
    for (const task of ctx.pending_tasks.slice(0, 5)) {
      parts.push(`- ${task.title}`);
    }
  }

  // Recent decisions
  if (ctx.recent_decisions && ctx.recent_decisions.length > 0) {
    parts.push("\n## 📝 Recent Decisions");
    for (const decision of ctx.recent_decisions.slice(0, 3)) {
      parts.push(`- **${decision.title}**`);
    }
  }

  parts.push("\n---");
  parts.push("Call `mcp__contextstream__context(user_message=\"...\")` for task-specific context.");

  return parts.join("\n");
}

export async function runSessionInitHook(): Promise<void> {
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

  const cwd = input.cwd || process.cwd();
  loadConfigFromMcpJson(cwd);

  // Fetch context from ContextStream
  const context = await fetchSessionContext();
  const formattedContext = formatContext(context);

  // Output Claude Code format
  console.log(
    JSON.stringify({
      hookSpecificOutput: {
        hookEventName: "SessionStart",
        additionalContext: formattedContext,
      },
    })
  );

  process.exit(0);
}

// Auto-run if executed directly
const isDirectRun = process.argv[1]?.includes("session-init") || process.argv[2] === "session-init";
if (isDirectRun) {
  runSessionInitHook().catch(() => process.exit(0));
}
