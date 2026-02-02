/**
 * ContextStream session-end Hook - Finalize session on stop
 *
 * Stop hook that finalizes the session, saving the full transcript
 * and marking the session as complete in ContextStream.
 *
 * Usage:
 *   npx @contextstream/mcp-server hook session-end
 *
 * Input (stdin): JSON with session_id, transcript_path, cwd
 * Output (stdout): None (silent finalization)
 * Exit: Always 0
 */

import * as fs from "node:fs";
import * as path from "node:path";
import { homedir } from "node:os";

const ENABLED = process.env.CONTEXTSTREAM_SESSION_END_ENABLED !== "false";
const SAVE_TRANSCRIPT = process.env.CONTEXTSTREAM_SESSION_END_SAVE_TRANSCRIPT !== "false";

let API_URL = process.env.CONTEXTSTREAM_API_URL || "https://api.contextstream.io";
let API_KEY = process.env.CONTEXTSTREAM_API_KEY || "";
let WORKSPACE_ID: string | null = null;
let PROJECT_ID: string | null = null;

interface HookInput {
  session_id?: string;
  transcript_path?: string;
  cwd?: string;
  hook_event_name?: string;
  reason?: string; // Why session ended (user_exit, timeout, etc.)
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

interface TranscriptMessage {
  role: string;
  content: string;
  timestamp: string;
  tool_calls?: unknown;
  tool_results?: unknown;
}

interface TranscriptStats {
  messageCount: number;
  toolCallCount: number;
  duration: number; // seconds
  filesModified: string[];
  messages: TranscriptMessage[];
  startedAt: string;
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

function parseTranscriptStats(transcriptPath: string): TranscriptStats {
  const stats: TranscriptStats = {
    messageCount: 0,
    toolCallCount: 0,
    duration: 0,
    filesModified: [],
    messages: [],
    startedAt: new Date().toISOString(),
  };

  if (!transcriptPath || !fs.existsSync(transcriptPath)) {
    return stats;
  }

  try {
    const content = fs.readFileSync(transcriptPath, "utf-8");
    const lines = content.split("\n");
    let firstTimestamp: Date | null = null;
    let lastTimestamp: Date | null = null;
    const modifiedFiles = new Set<string>();

    for (const line of lines) {
      if (!line.trim()) continue;

      try {
        const entry = JSON.parse(line) as {
          type?: string;
          role?: string;
          name?: string;
          content?: string;
          input?: { file_path?: string; notebook_path?: string };
          timestamp?: string;
        };

        const msgType = entry.type || "";
        const timestamp = entry.timestamp || new Date().toISOString();

        // Track timestamps
        if (entry.timestamp) {
          const ts = new Date(entry.timestamp);
          if (!firstTimestamp || ts < firstTimestamp) {
            firstTimestamp = ts;
            stats.startedAt = entry.timestamp;
          }
          if (!lastTimestamp || ts > lastTimestamp) {
            lastTimestamp = ts;
          }
        }

        // Count and capture messages
        if (msgType === "user" || entry.role === "user") {
          stats.messageCount++;
          const userContent = typeof entry.content === "string" ? entry.content : "";
          if (userContent) {
            stats.messages.push({
              role: "user",
              content: userContent,
              timestamp,
            });
          }
        } else if (msgType === "assistant" || entry.role === "assistant") {
          stats.messageCount++;
          const assistantContent = typeof entry.content === "string" ? entry.content : "";
          if (assistantContent) {
            stats.messages.push({
              role: "assistant",
              content: assistantContent,
              timestamp,
            });
          }
        } else if (msgType === "tool_use") {
          stats.toolCallCount++;
          const toolName = entry.name || "";
          const toolInput = entry.input || {};

          // Track file modifications
          if (["Write", "Edit", "NotebookEdit"].includes(toolName)) {
            const filePath = toolInput.file_path || toolInput.notebook_path;
            if (filePath) {
              modifiedFiles.add(filePath);
            }
          }

          // Add tool call as message
          stats.messages.push({
            role: "assistant",
            content: `[Tool: ${toolName}]`,
            timestamp,
            tool_calls: { name: toolName, input: toolInput },
          });
        } else if (msgType === "tool_result") {
          // Add tool result as message (truncated for storage)
          const resultContent = typeof entry.content === "string"
            ? entry.content.slice(0, 2000)
            : JSON.stringify(entry.content || {}).slice(0, 2000);
          stats.messages.push({
            role: "tool",
            content: resultContent,
            timestamp,
            tool_results: { name: entry.name },
          });
        }
      } catch {
        continue;
      }
    }

    // Calculate duration
    if (firstTimestamp && lastTimestamp) {
      stats.duration = Math.round((lastTimestamp.getTime() - firstTimestamp.getTime()) / 1000);
    }

    stats.filesModified = Array.from(modifiedFiles);
  } catch {
    // Ignore parsing errors
  }

  return stats;
}

async function saveFullTranscript(
  sessionId: string,
  stats: TranscriptStats,
  reason: string
): Promise<{ success: boolean; message: string }> {
  if (!API_KEY) {
    return { success: false, message: "No API key configured" };
  }

  if (stats.messages.length === 0) {
    return { success: false, message: "No messages to save" };
  }

  const payload: Record<string, unknown> = {
    session_id: sessionId,
    messages: stats.messages,
    started_at: stats.startedAt,
    source_type: "session_end",
    title: `Session transcript (${reason})`,
    metadata: {
      reason,
      tool_call_count: stats.toolCallCount,
      files_modified: stats.filesModified.slice(0, 20),
      duration_seconds: stats.duration,
    },
    tags: ["session_end", reason],
  };

  if (WORKSPACE_ID) {
    payload.workspace_id = WORKSPACE_ID;
  }
  if (PROJECT_ID) {
    payload.project_id = PROJECT_ID;
  }

  try {
    const controller = new AbortController();
    const timeoutId = setTimeout(() => controller.abort(), 15000); // 15s for larger payload

    const response = await fetch(`${API_URL}/api/v1/transcripts`, {
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
      return { success: true, message: `Transcript saved (${stats.messages.length} messages)` };
    }
    return { success: false, message: `API error: ${response.status}` };
  } catch (error) {
    return { success: false, message: String(error) };
  }
}

async function finalizeSession(
  sessionId: string,
  stats: TranscriptStats,
  reason: string
): Promise<void> {
  if (!API_KEY) return;

  // Save full transcript first (if enabled)
  if (SAVE_TRANSCRIPT && stats.messages.length > 0) {
    await saveFullTranscript(sessionId, stats, reason);
  }

  // Then save the summary event
  const payload: Record<string, unknown> = {
    event_type: "manual_note",
    title: `Session Ended: ${reason}`,
    content: JSON.stringify({
      session_id: sessionId,
      reason,
      stats: {
        messages: stats.messageCount,
        tool_calls: stats.toolCallCount,
        duration_seconds: stats.duration,
        files_modified: stats.filesModified.length,
      },
      files_modified: stats.filesModified.slice(0, 20),
      ended_at: new Date().toISOString(),
    }),
    importance: "low",
    tags: ["session", "end", reason],
    source_type: "hook",
  };

  if (WORKSPACE_ID) {
    payload.workspace_id = WORKSPACE_ID;
  }

  try {
    const controller = new AbortController();
    const timeoutId = setTimeout(() => controller.abort(), 5000);

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
    // Ignore errors - session is ending anyway
  }
}

export async function runSessionEndHook(): Promise<void> {
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

  const sessionId = input.session_id || "unknown";
  const transcriptPath = input.transcript_path || "";
  const reason = input.reason || "user_exit";

  // Parse transcript for stats
  const stats = parseTranscriptStats(transcriptPath);

  // Finalize session (async, don't wait too long)
  await finalizeSession(sessionId, stats, reason);

  // No output - silent finalization
  process.exit(0);
}

// Auto-run if executed directly
const isDirectRun = process.argv[1]?.includes("session-end") || process.argv[2] === "session-end";
if (isDirectRun) {
  runSessionEndHook().catch(() => process.exit(0));
}
