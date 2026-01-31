/**
 * ContextStream on-read Hook - Tracks file exploration
 *
 * PostToolUse hook for Read/Glob/Grep tools. Tracks which files are being
 * explored to build context about the codebase.
 *
 * Usage:
 *   npx @contextstream/mcp-server hook on-read
 *
 * Input (stdin): JSON with tool_name, tool_input, tool_result, cwd
 * Output (stdout): JSON with hookSpecificOutput (optional)
 * Exit: Always 0
 */

import * as fs from "node:fs";
import * as path from "node:path";
import { homedir } from "node:os";

const ENABLED = process.env.CONTEXTSTREAM_READ_HOOK_ENABLED !== "false";

let API_URL = process.env.CONTEXTSTREAM_API_URL || "https://api.contextstream.io";
let API_KEY = process.env.CONTEXTSTREAM_API_KEY || "";
let WORKSPACE_ID: string | null = null;

interface HookInput {
  tool_name?: string;
  tool_input?: {
    file_path?: string;
    pattern?: string;
    path?: string;
    glob?: string;
  };
  tool_result?: {
    output?: string;
    files?: string[];
    matches?: number;
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

// Track recent reads to avoid duplicate captures
const recentCaptures = new Set<string>();
const CAPTURE_WINDOW_MS = 60000; // 1 minute

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

async function captureExploration(
  toolName: string,
  target: string,
  resultSummary: string,
  sessionId: string
): Promise<void> {
  if (!API_KEY) return;

  // Deduplicate within window
  const cacheKey = `${toolName}:${target}`;
  if (recentCaptures.has(cacheKey)) {
    return;
  }
  recentCaptures.add(cacheKey);
  setTimeout(() => recentCaptures.delete(cacheKey), CAPTURE_WINDOW_MS);

  const payload: Record<string, unknown> = {
    event_type: "file_exploration",
    title: `${toolName}: ${target.slice(0, 50)}`,
    content: JSON.stringify({
      tool: toolName,
      target,
      result_summary: resultSummary.slice(0, 500),
      timestamp: new Date().toISOString(),
    }),
    importance: "low",
    tags: ["exploration", toolName.toLowerCase()],
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

export async function runOnReadHook(): Promise<void> {
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

  // Only handle Read/Glob/Grep tools
  const toolName = input.tool_name || "";
  if (!["Read", "Glob", "Grep"].includes(toolName)) {
    process.exit(0);
  }

  const cwd = input.cwd || process.cwd();
  loadConfigFromMcpJson(cwd);

  const sessionId = input.session_id || "unknown";
  let target = "";
  let resultSummary = "";

  switch (toolName) {
    case "Read":
      target = input.tool_input?.file_path || "";
      resultSummary = `Read file: ${target}`;
      break;
    case "Glob":
      target = input.tool_input?.pattern || "";
      const globFiles = input.tool_result?.files || [];
      resultSummary = `Found ${globFiles.length} files matching ${target}`;
      break;
    case "Grep":
      target = input.tool_input?.pattern || "";
      const matches = input.tool_result?.matches || 0;
      resultSummary = `Found ${matches} matches for "${target}"`;
      break;
  }

  if (target) {
    // Capture exploration (async, don't wait)
    captureExploration(toolName, target, resultSummary, sessionId).catch(() => {});
  }

  // No output injection needed
  process.exit(0);
}

// Auto-run if executed directly
const isDirectRun = process.argv[1]?.includes("on-read") || process.argv[2] === "on-read";
if (isDirectRun) {
  runOnReadHook().catch(() => process.exit(0));
}
