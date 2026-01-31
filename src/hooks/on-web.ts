/**
 * ContextStream on-web Hook - Captures web research
 *
 * PostToolUse hook for WebFetch/WebSearch tools. Captures URLs visited,
 * search queries, and results for research continuity.
 *
 * Usage:
 *   npx @contextstream/mcp-server hook on-web
 *
 * Input (stdin): JSON with tool_name, tool_input, tool_result, cwd
 * Output (stdout): JSON with hookSpecificOutput (optional)
 * Exit: Always 0
 */

import * as fs from "node:fs";
import * as path from "node:path";
import { homedir } from "node:os";

const ENABLED = process.env.CONTEXTSTREAM_WEB_HOOK_ENABLED !== "false";

let API_URL = process.env.CONTEXTSTREAM_API_URL || "https://api.contextstream.io";
let API_KEY = process.env.CONTEXTSTREAM_API_KEY || "";
let WORKSPACE_ID: string | null = null;

interface HookInput {
  tool_name?: string;
  tool_input?: {
    url?: string;
    query?: string;
    prompt?: string;
  };
  tool_result?: {
    output?: string;
    content?: string;
    results?: Array<{ title?: string; url?: string; snippet?: string }>;
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

async function captureWebResearch(
  toolName: string,
  target: string,
  summary: string,
  sessionId: string
): Promise<void> {
  if (!API_KEY) return;

  const payload: Record<string, unknown> = {
    event_type: "web_research",
    title: `${toolName}: ${target.slice(0, 60)}`,
    content: JSON.stringify({
      tool: toolName,
      target,
      summary: summary.slice(0, 1000),
      timestamp: new Date().toISOString(),
    }),
    importance: "medium",
    tags: ["research", "web", toolName.toLowerCase()],
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

export async function runOnWebHook(): Promise<void> {
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

  // Only handle WebFetch/WebSearch tools
  const toolName = input.tool_name || "";
  if (!["WebFetch", "WebSearch"].includes(toolName)) {
    process.exit(0);
  }

  const cwd = input.cwd || process.cwd();
  loadConfigFromMcpJson(cwd);

  const sessionId = input.session_id || "unknown";
  let target = "";
  let summary = "";

  switch (toolName) {
    case "WebFetch":
      target = input.tool_input?.url || "";
      const prompt = input.tool_input?.prompt || "fetched content";
      const content = input.tool_result?.output || input.tool_result?.content || "";
      summary = `Fetched ${target} (${prompt}): ${content.slice(0, 300)}`;
      break;
    case "WebSearch":
      target = input.tool_input?.query || "";
      const results = input.tool_result?.results || [];
      const topResults = results
        .slice(0, 3)
        .map((r) => `- ${r.title}: ${r.url}`)
        .join("\n");
      summary = `Search: "${target}"\nTop results:\n${topResults}`;
      break;
  }

  if (target) {
    // Capture research (async, don't wait)
    captureWebResearch(toolName, target, summary, sessionId).catch(() => {});
  }

  // No output injection needed
  process.exit(0);
}

// Auto-run if executed directly
const isDirectRun = process.argv[1]?.includes("on-web") || process.argv[2] === "on-web";
if (isDirectRun) {
  runOnWebHook().catch(() => process.exit(0));
}
