/**
 * ContextStream on-bash Hook - Captures bash commands and learns from errors
 *
 * PostToolUse hook for Bash tool. Tracks commands executed, captures errors,
 * and can create lessons from failures.
 *
 * Usage:
 *   npx @contextstream/mcp-server hook on-bash
 *
 * Input (stdin): JSON with tool_name, tool_input, tool_result, cwd
 * Output (stdout): JSON with hookSpecificOutput (optional context injection)
 * Exit: Always 0
 */

import * as fs from "node:fs";
import * as path from "node:path";
import { homedir } from "node:os";

const ENABLED = process.env.CONTEXTSTREAM_BASH_HOOK_ENABLED !== "false";

let API_URL = process.env.CONTEXTSTREAM_API_URL || "https://api.contextstream.io";
let API_KEY = process.env.CONTEXTSTREAM_API_KEY || "";
let WORKSPACE_ID: string | null = null;

interface HookInput {
  tool_name?: string;
  tool_input?: {
    command?: string;
    description?: string;
    timeout?: number;
  };
  tool_result?: {
    output?: string;
    error?: string;
    exit_code?: number;
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

async function captureCommand(
  command: string,
  output: string,
  exitCode: number,
  isError: boolean,
  sessionId: string
): Promise<void> {
  if (!API_KEY) return;

  const payload: Record<string, unknown> = {
    event_type: isError ? "bash_error" : "bash_command",
    title: isError
      ? `Bash Error: ${command.slice(0, 50)}...`
      : `Command: ${command.slice(0, 50)}...`,
    content: JSON.stringify({
      command,
      output: output.slice(0, 2000),
      exit_code: exitCode,
      timestamp: new Date().toISOString(),
    }),
    importance: isError ? "high" : "low",
    tags: isError ? ["bash", "error", "command"] : ["bash", "command"],
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

async function suggestLesson(command: string, error: string): Promise<string | null> {
  // Common bash errors and their lessons
  const errorPatterns: Array<{ pattern: RegExp; lesson: string }> = [
    {
      pattern: /command not found/i,
      lesson: `The command "${command.split(" ")[0]}" is not installed. Check if the package needs to be installed first.`,
    },
    {
      pattern: /permission denied/i,
      lesson: "Permission denied. May need sudo or to check file permissions.",
    },
    {
      pattern: /no such file or directory/i,
      lesson: "Path does not exist. Verify the file/directory path before running commands.",
    },
    {
      pattern: /EADDRINUSE|address already in use/i,
      lesson: "Port is already in use. Kill the existing process or use a different port.",
    },
    {
      pattern: /npm ERR!|ERESOLVE/i,
      lesson: "npm dependency conflict. Try `npm install --legacy-peer-deps` or check package versions.",
    },
    {
      pattern: /ENOENT.*package\.json/i,
      lesson: "No package.json found. Make sure you're in the right directory or run `npm init`.",
    },
    {
      pattern: /git.*not a git repository/i,
      lesson: "Not in a git repository. Run `git init` or navigate to a git repo.",
    },
  ];

  for (const { pattern, lesson } of errorPatterns) {
    if (pattern.test(error)) {
      return lesson;
    }
  }

  return null;
}

export async function runOnBashHook(): Promise<void> {
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

  // Only handle Bash tool
  if (input.tool_name !== "Bash") {
    process.exit(0);
  }

  const cwd = input.cwd || process.cwd();
  loadConfigFromMcpJson(cwd);

  const command = input.tool_input?.command || "";
  const output = input.tool_result?.output || input.tool_result?.error || "";
  const exitCode = input.tool_result?.exit_code ?? 0;
  const sessionId = input.session_id || "unknown";
  const isError = exitCode !== 0 || !!input.tool_result?.error;

  // Capture command to ContextStream (async, don't wait)
  captureCommand(command, output, exitCode, isError, sessionId).catch(() => {});

  // If error, suggest a lesson
  if (isError) {
    const lesson = await suggestLesson(command, output);
    if (lesson) {
      console.log(
        JSON.stringify({
          hookSpecificOutput: {
            hookEventName: "PostToolUse",
            additionalContext: `[ContextStream Insight] ${lesson}`,
          },
        })
      );
      process.exit(0);
    }
  }

  // No output for successful commands
  process.exit(0);
}

// Auto-run if executed directly
const isDirectRun = process.argv[1]?.includes("on-bash") || process.argv[2] === "on-bash";
if (isDirectRun) {
  runOnBashHook().catch(() => process.exit(0));
}
