/**
 * ContextStream on-task Hook - Tracks Task agent work
 *
 * PostToolUse hook for Task tool. Captures agent invocations, their prompts,
 * and results for context continuity.
 *
 * Usage:
 *   npx @contextstream/mcp-server hook on-task
 *
 * Input (stdin): JSON with tool_name, tool_input, tool_result, cwd
 * Output (stdout): JSON with hookSpecificOutput (optional)
 * Exit: Always 0
 */

import * as fs from "node:fs";
import * as path from "node:path";
import { homedir } from "node:os";

const ENABLED = process.env.CONTEXTSTREAM_TASK_HOOK_ENABLED !== "false";

let API_URL = process.env.CONTEXTSTREAM_API_URL || "https://api.contextstream.io";
let API_KEY = process.env.CONTEXTSTREAM_API_KEY || "";
let WORKSPACE_ID: string | null = null;

interface HookInput {
  tool_name?: string;
  tool_input?: {
    description?: string;
    prompt?: string;
    subagent_type?: string;
    model?: string;
    run_in_background?: boolean;
  };
  tool_result?: {
    output?: string;
    agent_id?: string;
    status?: string;
  };
  cwd?: string;
  session_id?: string;
}

interface McpConfig {
  mcpServers?: {
    contextstream?: {
      env?: {
        CONTEXTSTREAM_API_KEY?: string;
        CONTEXTSTREAM_API_URL?: string;
      };
    };
  };
}

interface LocalConfig {
  workspace_id?: string;
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
        } catch {
          // Continue
        }
      }
    }

    if (!WORKSPACE_ID) {
      const csConfigPath = path.join(searchDir, ".contextstream", "config.json");
      if (fs.existsSync(csConfigPath)) {
        try {
          const content = fs.readFileSync(csConfigPath, "utf-8");
          const csConfig = JSON.parse(content) as LocalConfig;
          if (csConfig.workspace_id) {
            WORKSPACE_ID = csConfig.workspace_id;
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

async function captureTaskInvocation(
  description: string,
  prompt: string,
  agentType: string,
  result: string,
  sessionId: string
): Promise<void> {
  if (!API_KEY) return;

  const payload: Record<string, unknown> = {
    event_type: "task_agent",
    title: `Agent: ${agentType} - ${description}`,
    content: JSON.stringify({
      description,
      prompt: prompt.slice(0, 1000),
      agent_type: agentType,
      result: result.slice(0, 2000),
      timestamp: new Date().toISOString(),
    }),
    importance: "medium",
    tags: ["task", "agent", agentType.toLowerCase()],
    source_type: "hook",
    session_id: sessionId,
  };

  if (WORKSPACE_ID) {
    payload.workspace_id = WORKSPACE_ID;
  }

  try {
    const controller = new AbortController();
    const timeoutId = setTimeout(() => controller.abort(), 3000);

    await fetch(`${API_URL}/api/v1/memory/events`, {
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
    // Ignore capture errors
  }
}

export async function runOnTaskHook(): Promise<void> {
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

  // Only handle Task tool
  if (input.tool_name !== "Task") {
    process.exit(0);
  }

  const cwd = input.cwd || process.cwd();
  loadConfigFromMcpJson(cwd);

  const description = input.tool_input?.description || "Unknown task";
  const prompt = input.tool_input?.prompt || "";
  const agentType = input.tool_input?.subagent_type || "general-purpose";
  const result = input.tool_result?.output || "";
  const sessionId = input.session_id || "unknown";

  // Capture task invocation (async, don't wait)
  captureTaskInvocation(description, prompt, agentType, result, sessionId).catch(() => {});

  // No output injection needed
  process.exit(0);
}

// Auto-run if executed directly
const isDirectRun = process.argv[1]?.includes("on-task") || process.argv[2] === "on-task";
if (isDirectRun) {
  runOnTaskHook().catch(() => process.exit(0));
}
