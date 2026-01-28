/**
 * ContextStream PreCompact Hook - Saves state before context compaction
 *
 * Runs BEFORE conversation context is compacted (manual via /compact or automatic).
 * Automatically saves conversation state to ContextStream by parsing the transcript.
 *
 * Usage:
 *   npx @contextstream/mcp-server hook pre-compact
 *
 * Input (stdin): JSON with session_id, transcript_path, trigger
 * Output (stdout): JSON with hookSpecificOutput containing context injection
 * Exit: Always 0
 */

import * as fs from "node:fs";
import * as path from "node:path";
import { homedir } from "node:os";

const ENABLED = process.env.CONTEXTSTREAM_PRECOMPACT_ENABLED !== "false";
const AUTO_SAVE = process.env.CONTEXTSTREAM_PRECOMPACT_AUTO_SAVE !== "false";

let API_URL = process.env.CONTEXTSTREAM_API_URL || "https://api.contextstream.io";
let API_KEY = process.env.CONTEXTSTREAM_API_KEY || "";
let WORKSPACE_ID: string | null = null;

interface HookInput {
  session_id?: string;
  transcript_path?: string;
  permission_mode?: string;
  hook_event_name?: string;
  trigger?: string;
  custom_instructions?: string;
  cwd?: string;
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

interface TranscriptEntry {
  type?: string;
  name?: string;
  input?: Record<string, unknown>;
  content?: string;
  role?: string;
}

interface TranscriptData {
  activeFiles: string[];
  toolCallCount: number;
  messageCount: number;
  lastTools: string[];
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

function parseTranscript(transcriptPath: string): TranscriptData {
  const activeFiles = new Set<string>();
  const recentMessages: string[] = [];
  const toolCalls: Array<{ name: string; input: Record<string, unknown> }> = [];

  try {
    const content = fs.readFileSync(transcriptPath, "utf-8");
    const lines = content.split("\n");

    for (const line of lines) {
      if (!line.trim()) continue;

      try {
        const entry = JSON.parse(line) as TranscriptEntry;
        const msgType = entry.type || "";

        // Extract files from tool calls
        if (msgType === "tool_use") {
          const toolName = entry.name || "";
          const toolInput = entry.input || {};
          toolCalls.push({ name: toolName, input: toolInput });

          // Extract file paths from common tools
          if (["Read", "Write", "Edit", "NotebookEdit"].includes(toolName)) {
            const filePath =
              (toolInput.file_path as string) || (toolInput.notebook_path as string);
            if (filePath) {
              activeFiles.add(filePath);
            }
          } else if (toolName === "Glob") {
            const pattern = toolInput.pattern as string;
            if (pattern) {
              activeFiles.add(`[glob:${pattern}]`);
            }
          }
        }

        // Collect recent assistant messages for summary
        if (msgType === "assistant" && entry.content) {
          const content = entry.content;
          if (typeof content === "string" && content.length > 50) {
            recentMessages.push(content.slice(0, 500));
          }
        }
      } catch {
        continue;
      }
    }
  } catch {
    // Ignore transcript parsing errors
  }

  return {
    activeFiles: Array.from(activeFiles).slice(-20), // Last 20 files
    toolCallCount: toolCalls.length,
    messageCount: recentMessages.length,
    lastTools: toolCalls.slice(-10).map((t) => t.name), // Last 10 tool names
  };
}

async function saveSnapshot(
  sessionId: string,
  transcriptData: TranscriptData,
  trigger: string
): Promise<{ success: boolean; message: string }> {
  if (!API_KEY) {
    return { success: false, message: "No API key configured" };
  }

  const snapshotContent = {
    session_id: sessionId,
    trigger,
    captured_at: null, // API will set timestamp
    active_files: transcriptData.activeFiles,
    tool_call_count: transcriptData.toolCallCount,
    last_tools: transcriptData.lastTools,
    auto_captured: true,
  };

  const payload: Record<string, unknown> = {
    event_type: "session_snapshot",
    title: `Auto Pre-compaction Snapshot (${trigger})`,
    content: JSON.stringify(snapshotContent),
    importance: "high",
    tags: ["session_snapshot", "pre_compaction", "auto_captured"],
    source_type: "hook",
  };

  // Add workspace_id if available
  if (WORKSPACE_ID) {
    payload.workspace_id = WORKSPACE_ID;
  }

  try {
    const controller = new AbortController();
    const timeoutId = setTimeout(() => controller.abort(), 5000);

    const response = await fetch(`${API_URL}/api/v1/memory/events`, {
      method: "POST",
      headers: {
        "Content-Type": "application/json",
        "X-API-Key": API_KEY,
      },
      body: JSON.stringify(payload),
      signal: controller.signal,
    });

    clearTimeout(timeoutId);

    if (response.ok) {
      return { success: true, message: "Snapshot saved" };
    }
    return { success: false, message: `API error: ${response.status}` };
  } catch (error) {
    return { success: false, message: String(error) };
  }
}

export async function runPreCompactHook(): Promise<void> {
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

  const sessionId = input.session_id || "unknown";
  const transcriptPath = input.transcript_path || "";
  const trigger = input.trigger || "unknown";
  const customInstructions = input.custom_instructions || "";

  // Parse transcript for context
  let transcriptData: TranscriptData = {
    activeFiles: [],
    toolCallCount: 0,
    messageCount: 0,
    lastTools: [],
  };

  if (transcriptPath && fs.existsSync(transcriptPath)) {
    transcriptData = parseTranscript(transcriptPath);
  }

  // Auto-save snapshot if enabled
  let autoSaveStatus = "";
  if (AUTO_SAVE && API_KEY) {
    const { success, message } = await saveSnapshot(sessionId, transcriptData, trigger);
    if (success) {
      autoSaveStatus = `\n[ContextStream: Auto-saved snapshot with ${transcriptData.activeFiles.length} active files]`;
    } else {
      autoSaveStatus = `\n[ContextStream: Auto-save failed - ${message}]`;
    }
  }

  // Build context injection for the AI (backup in case auto-save fails)
  const filesList = transcriptData.activeFiles.slice(0, 5).join(", ") || "none detected";
  const context = `[CONTEXT COMPACTION - ${trigger.toUpperCase()}]${autoSaveStatus}

Active files detected: ${filesList}
Tool calls in session: ${transcriptData.toolCallCount}

After compaction, call session_init(is_post_compact=true) to restore context.${customInstructions ? `\nUser instructions: ${customInstructions}` : ""}`;

  // Output Claude Code format
  console.log(
    JSON.stringify({
      hookSpecificOutput: {
        hookEventName: "PreCompact",
        additionalContext: context,
      },
    })
  );

  process.exit(0);
}

// Auto-run if executed directly
const isDirectRun = process.argv[1]?.includes("pre-compact") || process.argv[2] === "pre-compact";
if (isDirectRun) {
  runPreCompactHook().catch(() => process.exit(0));
}
