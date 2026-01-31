/**
 * ContextStream PostCompact Hook - Restores context after compaction
 *
 * Runs AFTER conversation context is compacted. Fetches the saved transcript/snapshot
 * and injects context to help Claude restore state.
 *
 * Usage:
 *   npx @contextstream/mcp-server hook post-compact
 *
 * Input (stdin): JSON with session_id, cwd
 * Output (stdout): JSON with hookSpecificOutput containing restored context
 * Exit: Always 0
 */

import * as fs from "node:fs";
import * as path from "node:path";
import { homedir } from "node:os";

const ENABLED = process.env.CONTEXTSTREAM_POSTCOMPACT_ENABLED !== "false";

let API_URL = process.env.CONTEXTSTREAM_API_URL || "https://api.contextstream.io";
let API_KEY = process.env.CONTEXTSTREAM_API_KEY || "";
let WORKSPACE_ID: string | null = null;

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
      };
    };
  };
}

interface LocalConfig {
  workspace_id?: string;
  project_id?: string;
}

interface TranscriptResponse {
  id: string;
  session_id: string;
  messages: Array<{
    role: string;
    content: string;
    timestamp?: string;
  }>;
  metadata?: {
    active_files?: string[];
    tool_call_count?: number;
  };
  created_at: string;
}

function loadConfigFromMcpJson(cwd: string): void {
  let searchDir = path.resolve(cwd);

  for (let i = 0; i < 5; i++) {
    // Load API config from .mcp.json
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
          // Continue searching
        }
      }
    }

    // Load workspace_id from .contextstream/config.json
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
          // Continue searching
        }
      }
    }

    const parentDir = path.dirname(searchDir);
    if (parentDir === searchDir) break;
    searchDir = parentDir;
  }

  // Also check home directory for .mcp.json
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

async function fetchLastTranscript(sessionId: string): Promise<TranscriptResponse | null> {
  if (!API_KEY) {
    return null;
  }

  try {
    const controller = new AbortController();
    const timeoutId = setTimeout(() => controller.abort(), 5000);

    const url = new URL(`${API_URL}/api/v1/transcripts`);
    url.searchParams.set("session_id", sessionId);
    url.searchParams.set("limit", "1");
    url.searchParams.set("sort", "created_at:desc");
    if (WORKSPACE_ID) {
      url.searchParams.set("workspace_id", WORKSPACE_ID);
    }

    const response = await fetch(url.toString(), {
      method: "GET",
      headers: {
        "X-API-Key": API_KEY,
      },
      signal: controller.signal,
    });

    clearTimeout(timeoutId);

    if (response.ok) {
      const data = (await response.json()) as { transcripts?: TranscriptResponse[] };
      if (data.transcripts && data.transcripts.length > 0) {
        return data.transcripts[0];
      }
    }
    return null;
  } catch {
    return null;
  }
}

function formatTranscriptSummary(transcript: TranscriptResponse): string {
  const messages = transcript.messages || [];
  const activeFiles = transcript.metadata?.active_files || [];
  const toolCallCount = transcript.metadata?.tool_call_count || 0;

  // Get last few user messages for context
  const userMessages = messages
    .filter((m) => m.role === "user")
    .slice(-3)
    .map((m) => `- "${m.content.slice(0, 100)}${m.content.length > 100 ? "..." : ""}"`)
    .join("\n");

  // Get last assistant response
  const lastAssistant = messages
    .filter((m) => m.role === "assistant" && !m.content.startsWith("[Tool:"))
    .slice(-1)[0];
  const lastWork = lastAssistant
    ? lastAssistant.content.slice(0, 300) + (lastAssistant.content.length > 300 ? "..." : "")
    : "None recorded";

  return `## Pre-Compaction State Restored

### Active Files (${activeFiles.length})
${activeFiles.slice(0, 10).map((f) => `- ${f}`).join("\n") || "None tracked"}

### Recent User Requests
${userMessages || "None recorded"}

### Last Work in Progress
${lastWork}

### Session Stats
- Tool calls: ${toolCallCount}
- Messages: ${messages.length}
- Saved at: ${transcript.created_at}`;
}

export async function runPostCompactHook(): Promise<void> {
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

  // Load config from .mcp.json if env vars not set
  const cwd = input.cwd || process.cwd();
  loadConfigFromMcpJson(cwd);

  const sessionId = input.session_id || "";

  // Try to fetch the last saved transcript for this session
  let restoredContext = "";
  if (sessionId && API_KEY) {
    const transcript = await fetchLastTranscript(sessionId);
    if (transcript) {
      restoredContext = formatTranscriptSummary(transcript);
    }
  }

  // Build context injection
  const context = `[POST-COMPACTION - Context Restored]

${restoredContext || "No saved state found. Starting fresh."}

**IMPORTANT:** Call \`mcp__contextstream__context(user_message="resuming after compaction")\` to get full context and any pending tasks.

The conversation was compacted to save memory. The above summary was automatically restored from ContextStream.`;

  // Output Claude Code format
  console.log(
    JSON.stringify({
      hookSpecificOutput: {
        hookEventName: "PostCompact",
        additionalContext: context,
      },
    })
  );

  process.exit(0);
}

// Auto-run if executed directly
const isDirectRun = process.argv[1]?.includes("post-compact") || process.argv[2] === "post-compact";
if (isDirectRun) {
  runPostCompactHook().catch(() => process.exit(0));
}
