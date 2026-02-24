/**
 * ContextStream PreToolUse Hook - Blocks discovery tools
 *
 * Blocks Grep/Glob/Search/Explore/Task(Explore|Plan)/EnterPlanMode and redirects to ContextStream search.
 * Only blocks if the current project is indexed in ContextStream.
 *
 * Usage:
 *   npx @contextstream/mcp-server hook pre-tool-use
 *
 * Input (stdin): JSON with tool_name, tool_input, cwd
 * Output (stdout): JSON with hookSpecificOutput.permissionDecision
 * Exit: 0 always (decision is in JSON response)
 */

import * as fs from "node:fs";
import * as path from "node:path";
import { homedir } from "node:os";
import {
  cleanupStale,
  clearContextRequired,
  clearInitRequired,
  isContextFreshAndClean,
  isContextRequired,
  isInitRequired,
  markStateChanged,
} from "./prompt-state.js";

const ENABLED = process.env.CONTEXTSTREAM_HOOK_ENABLED !== "false";
const INDEX_STATUS_FILE = path.join(homedir(), ".contextstream", "indexed-projects.json");
const DEBUG_FILE = "/tmp/pretooluse-hook-debug.log";
const STALE_THRESHOLD_DAYS = 7;
const CONTEXT_FRESHNESS_SECONDS = 120;

const DISCOVERY_PATTERNS = ["**/*", "**/", "src/**", "lib/**", "app/**", "components/**"];

interface HookInput {
  // Claude Code format
  tool_name?: string;
  tool_input?: {
    pattern?: string;
    path?: string;
    subagent_type?: string;
  };
  cwd?: string;

  // Cursor format
  hook_event_name?: string;
  parameters?: {
    path?: string;
    pattern?: string;
    regex?: string;
  };
  workspace_roots?: string[];

  // Cline/Roo/Kilo format
  hookName?: string;
  toolName?: string;
  toolParameters?: {
    path?: string;
    regex?: string;
  };
  workspaceRoots?: string[];
}

interface IndexedProjectInfo {
  indexed_at: string;
  project_id?: string;
  project_name?: string;
}

interface IndexStatusFile {
  version: number;
  projects: Record<string, IndexedProjectInfo>;
}

function isDiscoveryGlob(pattern: string): boolean {
  const patternLower = pattern.toLowerCase();

  for (const p of DISCOVERY_PATTERNS) {
    if (patternLower.includes(p)) {
      return true;
    }
  }

  if (patternLower.startsWith("**/*.") || patternLower.startsWith("**/")) {
    return true;
  }

  if (patternLower.includes("**") || patternLower.includes("*/")) {
    return true;
  }

  return false;
}

function isDiscoveryGrep(filePath: string | undefined): boolean {
  if (!filePath || filePath === "." || filePath === "./" || filePath === "*" || filePath === "**") {
    return true;
  }
  if (filePath.includes("*") || filePath.includes("**")) {
    return true;
  }
  return false;
}

function isProjectIndexed(cwd: string): { isIndexed: boolean; isStale: boolean } {
  if (!fs.existsSync(INDEX_STATUS_FILE)) {
    return { isIndexed: false, isStale: false };
  }

  let data: IndexStatusFile;
  try {
    const content = fs.readFileSync(INDEX_STATUS_FILE, "utf-8");
    data = JSON.parse(content);
  } catch {
    return { isIndexed: false, isStale: false };
  }

  const projects = data.projects || {};
  const cwdPath = path.resolve(cwd);

  for (const [projectPath, info] of Object.entries(projects)) {
    try {
      const indexedPath = path.resolve(projectPath);

      // Check if cwd is the project or a subdirectory
      if (cwdPath === indexedPath || cwdPath.startsWith(indexedPath + path.sep)) {
        // Check if stale
        const indexedAt = info.indexed_at;
        if (indexedAt) {
          try {
            const indexedTime = new Date(indexedAt);
            const now = new Date();
            const diffDays = (now.getTime() - indexedTime.getTime()) / (1000 * 60 * 60 * 24);
            if (diffDays > STALE_THRESHOLD_DAYS) {
              return { isIndexed: true, isStale: true };
            }
          } catch {
            // Ignore date parsing errors
          }
        }
        return { isIndexed: true, isStale: false };
      }
    } catch {
      continue;
    }
  }

  return { isIndexed: false, isStale: false };
}

function extractCwd(input: HookInput): string {
  if (input.cwd) return input.cwd;
  if (input.workspace_roots?.length) return input.workspace_roots[0];
  if (input.workspaceRoots?.length) return input.workspaceRoots[0];
  return process.cwd();
}

function extractToolName(input: HookInput): string {
  return input.tool_name || input.toolName || "";
}

function extractToolInput(input: HookInput): HookInput["tool_input"] {
  return input.tool_input || input.parameters || input.toolParameters || {};
}

function normalizeContextstreamToolName(toolName: string): string | null {
  const trimmed = toolName.trim();
  if (!trimmed) return null;
  const lower = trimmed.toLowerCase();

  const prefixed = "mcp__contextstream__";
  if (lower.startsWith(prefixed)) {
    return lower.slice(prefixed.length);
  }
  if (lower.startsWith("contextstream__")) {
    return lower.slice("contextstream__".length);
  }
  if (lower === "init" || lower === "context") {
    return lower;
  }
  return null;
}

function actionFromToolInput(toolInput: HookInput["tool_input"]): string {
  const maybeAction = (toolInput as any)?.action;
  return typeof maybeAction === "string" ? maybeAction.trim().toLowerCase() : "";
}

function isContextstreamReadOnlyOperation(
  toolName: string,
  toolInput: HookInput["tool_input"]
): boolean {
  const action = actionFromToolInput(toolInput);
  switch (toolName) {
    case "workspace":
      return action === "list" || action === "get";
    case "memory":
      return (
        action === "list_docs" ||
        action === "list_events" ||
        action === "list_todos" ||
        action === "list_tasks" ||
        action === "list_transcripts" ||
        action === "list_nodes" ||
        action === "decisions" ||
        action === "get_doc" ||
        action === "get_event" ||
        action === "get_task" ||
        action === "get_todo" ||
        action === "get_transcript"
      );
    case "session":
      return action === "get_lessons" || action === "get_plan" || action === "list_plans" || action === "recall";
    case "help":
      return action === "version" || action === "tools" || action === "auth";
    case "project":
      return action === "list" || action === "get" || action === "index_status";
    case "reminder":
      return action === "list" || action === "active";
    case "context":
    case "init":
      return true;
    default:
      return false;
  }
}

function isLikelyStateChangingTool(
  toolLower: string,
  toolInput: HookInput["tool_input"],
  isContextstreamCall: boolean,
  normalizedContextstreamTool: string | null
): boolean {
  if (isContextstreamCall && normalizedContextstreamTool) {
    return !isContextstreamReadOnlyOperation(normalizedContextstreamTool, toolInput);
  }

  if (
    [
      "read",
      "read_file",
      "grep",
      "glob",
      "search",
      "grep_search",
      "code_search",
      "semanticsearch",
      "codebase_search",
      "list_files",
      "search_files",
      "search_files_content",
      "find_files",
      "find_by_name",
      "ls",
      "cat",
      "view",
    ].includes(toolLower)
  ) {
    return false;
  }

  const writeMarkers = [
    "write",
    "edit",
    "create",
    "delete",
    "remove",
    "rename",
    "move",
    "patch",
    "apply",
    "insert",
    "append",
    "replace",
    "update",
    "commit",
    "push",
    "install",
    "exec",
    "run",
    "bash",
    "shell",
  ];
  return writeMarkers.some((marker) => toolLower.includes(marker));
}

/**
 * Output for Claude Code format (JSON decision based)
 * Must use exit 0 with JSON containing hookSpecificOutput.permissionDecision
 */
function blockClaudeCode(message: string): never {
  // Use additionalContext approach - inject guidance without hard blocking
  // This avoids the ugly "blocking error" display while still guiding Claude
  const response = {
    hookSpecificOutput: {
      hookEventName: "PreToolUse",
      // Use additionalContext instead of deny - tool runs but Claude sees the message
      additionalContext: `[CONTEXTSTREAM] ${message}`,
    },
  };
  fs.appendFileSync(DEBUG_FILE, `[PreToolUse] REDIRECT (additionalContext): ${JSON.stringify(response)}\n`);
  console.log(JSON.stringify(response));
  process.exit(0);
}

/**
 * Output for Cline/Roo/Kilo format (JSON based)
 */
function outputClineBlock(errorMessage: string, contextMod?: string): never {
  const result: { cancel: boolean; errorMessage?: string; contextModification?: string } = {
    cancel: true,
    errorMessage,
  };
  if (contextMod) {
    result.contextModification = contextMod;
  }
  console.log(JSON.stringify(result));
  process.exit(0);
}

function outputClineAllow(): never {
  console.log(JSON.stringify({ cancel: false }));
  process.exit(0);
}

/**
 * Output for Cursor format (JSON decision based)
 */
function outputCursorBlock(reason: string): never {
  console.log(JSON.stringify({ decision: "deny", reason }));
  process.exit(0);
}

function outputCursorAllow(): never {
  console.log(JSON.stringify({ decision: "allow" }));
  process.exit(0);
}

function blockWithMessage(editorFormat: "claude" | "cline" | "cursor", message: string): never {
  if (editorFormat === "cline") {
    outputClineBlock(message, "[CONTEXTSTREAM] Follow ContextStream startup requirements.");
  } else if (editorFormat === "cursor") {
    outputCursorBlock(message);
  }
  blockClaudeCode(message);
}

function allowTool(
  editorFormat: "claude" | "cline" | "cursor",
  cwd: string,
  recordStateChange: boolean
): never {
  if (recordStateChange) {
    markStateChanged(cwd);
  }
  if (editorFormat === "cline") {
    outputClineAllow();
  } else if (editorFormat === "cursor") {
    outputCursorAllow();
  }
  process.exit(0);
}

function detectEditorFormat(input: HookInput): "claude" | "cline" | "cursor" {
  // Cline/Roo/Kilo format uses camelCase (hookName, toolName)
  if (input.hookName !== undefined || input.toolName !== undefined) {
    return "cline";
  }
  // Claude Code uses snake_case (hook_event_name, tool_name) with specific structure
  // Cursor also uses hook_event_name but has different response expectations
  // For now, default to Claude Code format when hook_event_name is present
  // This ensures proper hookSpecificOutput.permissionDecision format
  if (input.hook_event_name !== undefined || input.tool_name !== undefined) {
    return "claude";
  }
  // Default to Claude Code format
  return "claude";
}

export async function runPreToolUseHook(): Promise<void> {
  // Debug: write to file to prove hook was invoked
    fs.appendFileSync(DEBUG_FILE, `[PreToolUse] Hook invoked at ${new Date().toISOString()}\n`);
  console.error("[PreToolUse] Hook invoked at", new Date().toISOString());

  if (!ENABLED) {
    fs.appendFileSync(DEBUG_FILE, "[PreToolUse] Hook disabled, exiting\n");
    console.error("[PreToolUse] Hook disabled, exiting");
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
  const cwd = extractCwd(input);
  const tool = extractToolName(input);
  const toolInput = extractToolInput(input);
  const toolLower = tool.toLowerCase();
  const normalizedContextstreamTool = normalizeContextstreamToolName(tool);
  const isContextstreamCall = normalizedContextstreamTool !== null;
  const recordStateChange = isLikelyStateChangingTool(
    toolLower,
    toolInput,
    isContextstreamCall,
    normalizedContextstreamTool
  );

  fs.appendFileSync(DEBUG_FILE, `[PreToolUse] tool=${tool}, cwd=${cwd}, editorFormat=${editorFormat}\n`);

  cleanupStale(180);

  if (isInitRequired(cwd)) {
    if (isContextstreamCall && normalizedContextstreamTool === "init") {
      clearInitRequired(cwd);
    } else {
      const required = "mcp__contextstream__init(...)";
      const msg = `First call required for this session: ${required}. Run it before any other MCP tool. Then call mcp__contextstream__context(user_message="...", save_exchange=true, session_id="<session-id>").`;
      blockWithMessage(editorFormat, msg);
    }
  }

  if (isContextRequired(cwd)) {
    if (isContextstreamCall && normalizedContextstreamTool === "context") {
      clearContextRequired(cwd);
    } else if (isContextstreamCall && normalizedContextstreamTool === "init") {
      // Allow init before context on the first message in a session.
    } else if (
      isContextstreamCall &&
      normalizedContextstreamTool &&
      isContextstreamReadOnlyOperation(normalizedContextstreamTool, toolInput) &&
      isContextFreshAndClean(cwd, CONTEXT_FRESHNESS_SECONDS)
    ) {
      // Narrow bypass: immediate read-only calls are allowed if context is fresh and unchanged.
    } else {
      const msg =
        'First call required for this prompt: mcp__contextstream__context(user_message="...", save_exchange=true, session_id="<session-id>"). Run it before any other MCP tool.';
      blockWithMessage(editorFormat, msg);
    }
  }

  // Check if project is indexed
  const { isIndexed } = isProjectIndexed(cwd);
  fs.appendFileSync(DEBUG_FILE, `[PreToolUse] isIndexed=${isIndexed}\n`);
  if (!isIndexed) {
    // Project not indexed - allow local tools
    fs.appendFileSync(DEBUG_FILE, `[PreToolUse] Project not indexed, allowing\n`);
    allowTool(editorFormat, cwd, recordStateChange);
  }

  // Check tool and block if needed
  if (tool === "Glob") {
    const pattern = toolInput?.pattern || "";
    fs.appendFileSync(DEBUG_FILE, `[PreToolUse] Glob pattern=${pattern}, isDiscovery=${isDiscoveryGlob(pattern)}\n`);
    // Only intercept broad discovery patterns (e.g., **/*.ts, src/**)
    if (isDiscoveryGlob(pattern)) {
      const msg = `This project index is current. Use mcp__contextstream__search(mode="auto", query="${pattern}") instead of Glob for faster, richer code results.`;
      fs.appendFileSync(DEBUG_FILE, `[PreToolUse] Intercepting discovery glob: ${msg}\n`);
      if (editorFormat === "cline") {
        outputClineBlock(msg, "[CONTEXTSTREAM] Use ContextStream search for code discovery.");
      } else if (editorFormat === "cursor") {
        outputCursorBlock(msg);
      }
      blockClaudeCode(msg);
    }
  } else if (tool === "Grep" || tool === "Search") {
    const pattern = toolInput?.pattern || "";
    const filePath = toolInput?.path || "";

    if (pattern) {
      if (filePath && !isDiscoveryGrep(filePath)) {
        const msg = `STOP: Use Read("${filePath}") to view file content, or mcp__contextstream__search(mode="keyword", query="${pattern}") for codebase search.`;
        if (editorFormat === "cline") {
          outputClineBlock(msg, "[CONTEXTSTREAM] Use ContextStream search for code discovery.");
        } else if (editorFormat === "cursor") {
          outputCursorBlock(msg);
        }
        blockClaudeCode(msg);
      } else {
        const msg = `This project index is current. Use mcp__contextstream__search(mode="auto", query="${pattern}") instead of ${tool} for faster, richer code results.`;
        if (editorFormat === "cline") {
          outputClineBlock(msg, "[CONTEXTSTREAM] Use ContextStream search for code discovery.");
        } else if (editorFormat === "cursor") {
          outputCursorBlock(msg);
        }
        blockClaudeCode(msg);
      }
    }
  } else if (tool === "Explore") {
    const msg =
      'Project index is current. Use mcp__contextstream__search(mode="auto", output_format="paths") instead of Explore for broad discovery.';
    if (editorFormat === "cline") {
      outputClineBlock(msg, "[CONTEXTSTREAM] Use ContextStream search for code discovery.");
    } else if (editorFormat === "cursor") {
      outputCursorBlock(msg);
    }
    blockClaudeCode(msg);
  } else if (tool === "Task") {
    const subagentTypeRaw =
      (toolInput as { subagent_type?: string; subagentType?: string })?.subagent_type ||
      (toolInput as { subagent_type?: string; subagentType?: string })?.subagentType ||
      "";
    const subagentType = subagentTypeRaw.toLowerCase();
    if (subagentType.includes("explore")) {
      const msg = 'Project index is current. Use mcp__contextstream__search(mode="auto") instead of Task(Explore) for broad discovery.';
      if (editorFormat === "cline") {
        outputClineBlock(msg, "[CONTEXTSTREAM] Use ContextStream search for code discovery.");
      } else if (editorFormat === "cursor") {
        outputCursorBlock(msg);
      }
      blockClaudeCode(msg);
    }
    if (subagentType.includes("plan")) {
      const msg =
        'For planning, use mcp__contextstream__search(mode="auto", output_format="paths") for discovery, then save your plan with mcp__contextstream__session(action="capture_plan"). Then create tasks with mcp__contextstream__memory(action="create_task", title="...", plan_id="...").';
      if (editorFormat === "cline") {
        outputClineBlock(msg, "[CONTEXTSTREAM] Use ContextStream plans for persistence.");
      } else if (editorFormat === "cursor") {
        outputCursorBlock(msg);
      }
      blockClaudeCode(msg);
    }
  } else if (tool === "EnterPlanMode") {
    const msg =
      'After finalizing your plan, save it to ContextStream (not a local markdown file): mcp__contextstream__session(action="capture_plan", title="...", steps=[...]). Then create tasks with mcp__contextstream__memory(action="create_task", title="...", plan_id="...").';
    if (editorFormat === "cline") {
      outputClineBlock(msg, "[CONTEXTSTREAM] Use ContextStream plans for persistence.");
    } else if (editorFormat === "cursor") {
      outputCursorBlock(msg);
    }
    blockClaudeCode(msg);
  }

  // Cline/Cursor specific tool names
  if (tool === "list_files" || tool === "search_files") {
    const pattern = toolInput?.path || (toolInput as { regex?: string })?.regex || "";
    if (isDiscoveryGlob(pattern) || isDiscoveryGrep(pattern)) {
      const msg = `Project index is current. Use mcp__contextstream__search(mode="auto", query="${pattern}") instead of ${tool} for faster, richer code results.`;
      if (editorFormat === "cline") {
        outputClineBlock(msg, "[CONTEXTSTREAM] Use ContextStream search for code discovery.");
      } else if (editorFormat === "cursor") {
        outputCursorBlock(msg);
      }
      blockClaudeCode(msg);
    }
  }

  // Allow the tool
  allowTool(editorFormat, cwd, recordStateChange);
}

// Auto-run if executed directly
const isDirectRun = process.argv[1]?.includes("pre-tool-use") || process.argv[2] === "pre-tool-use";
if (isDirectRun) {
  runPreToolUseHook().catch(() => process.exit(0));
}
