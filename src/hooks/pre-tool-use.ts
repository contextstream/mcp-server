/**
 * ContextStream PreToolUse Hook - Blocks discovery tools
 *
 * Blocks Grep/Glob/Search/Task(Explore)/EnterPlanMode and redirects to ContextStream search.
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

const ENABLED = process.env.CONTEXTSTREAM_HOOK_ENABLED !== "false";
const INDEX_STATUS_FILE = path.join(homedir(), ".contextstream", "indexed-projects.json");
const DEBUG_FILE = "/tmp/pretooluse-hook-debug.log";
const STALE_THRESHOLD_DAYS = 7;

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

  fs.appendFileSync(DEBUG_FILE, `[PreToolUse] tool=${tool}, cwd=${cwd}, editorFormat=${editorFormat}\n`);

  // Check if project is indexed
  const { isIndexed } = isProjectIndexed(cwd);
  fs.appendFileSync(DEBUG_FILE, `[PreToolUse] isIndexed=${isIndexed}\n`);
  if (!isIndexed) {
    // Project not indexed - allow local tools
    fs.appendFileSync(DEBUG_FILE, `[PreToolUse] Project not indexed, allowing\n`);
    if (editorFormat === "cline") {
      outputClineAllow();
    } else if (editorFormat === "cursor") {
      outputCursorAllow();
    }
    process.exit(0);
  }

  // Check tool and block if needed
  if (tool === "Glob") {
    const pattern = toolInput?.pattern || "";
    fs.appendFileSync(DEBUG_FILE, `[PreToolUse] Glob pattern=${pattern}, isDiscovery=${isDiscoveryGlob(pattern)}\n`);
    // Only intercept broad discovery patterns (e.g., **/*.ts, src/**)
    if (isDiscoveryGlob(pattern)) {
      const msg = `STOP: Use mcp__contextstream__search(mode="hybrid", query="${pattern}") instead of Glob.`;
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
        const msg = `STOP: Use mcp__contextstream__search(mode="hybrid", query="${pattern}") instead of ${tool}.`;
        if (editorFormat === "cline") {
          outputClineBlock(msg, "[CONTEXTSTREAM] Use ContextStream search for code discovery.");
        } else if (editorFormat === "cursor") {
          outputCursorBlock(msg);
        }
        blockClaudeCode(msg);
      }
    }
  } else if (tool === "Task") {
    const subagentType = (toolInput as { subagent_type?: string })?.subagent_type?.toLowerCase() || "";
    if (subagentType === "explore") {
      const msg = 'STOP: Use mcp__contextstream__search(mode="hybrid") instead of Task(Explore).';
      if (editorFormat === "cline") {
        outputClineBlock(msg, "[CONTEXTSTREAM] Use ContextStream search for code discovery.");
      } else if (editorFormat === "cursor") {
        outputCursorBlock(msg);
      }
      blockClaudeCode(msg);
    }
    if (subagentType === "plan") {
      const msg =
        'STOP: Use mcp__contextstream__session(action="capture_plan") for planning. ContextStream plans persist across sessions.';
      if (editorFormat === "cline") {
        outputClineBlock(msg, "[CONTEXTSTREAM] Use ContextStream plans for persistence.");
      } else if (editorFormat === "cursor") {
        outputCursorBlock(msg);
      }
      blockClaudeCode(msg);
    }
  } else if (tool === "EnterPlanMode") {
    const msg =
      'STOP: Use mcp__contextstream__session(action="capture_plan", title="...", steps=[...]) instead of EnterPlanMode. ContextStream plans persist across sessions and are searchable.';
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
      const msg = `Use mcp__contextstream__search(mode="hybrid", query="${pattern}") instead of ${tool}. ContextStream search is indexed and faster.`;
      if (editorFormat === "cline") {
        outputClineBlock(msg, "[CONTEXTSTREAM] Use ContextStream search for code discovery.");
      } else if (editorFormat === "cursor") {
        outputCursorBlock(msg);
      }
    }
  }

  // Allow the tool
  if (editorFormat === "cline") {
    outputClineAllow();
  } else if (editorFormat === "cursor") {
    outputCursorAllow();
  }
  process.exit(0);
}

// Auto-run if executed directly
const isDirectRun = process.argv[1]?.includes("pre-tool-use") || process.argv[2] === "pre-tool-use";
if (isDirectRun) {
  runPreToolUseHook().catch(() => process.exit(0));
}
