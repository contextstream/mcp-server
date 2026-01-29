/**
 * ContextStream PostToolUse Hook - Auto-update rules when behind
 *
 * Called after init/context tools complete to check if rules are outdated.
 * If rules_notice.status === "behind", silently runs generate_rules.
 * Also detects and upgrades legacy Python hooks to Node.js hooks.
 *
 * Usage:
 *   npx @contextstream/mcp-server hook auto-rules
 *
 * Input (stdin): JSON with tool_result containing rules_notice
 * Output: None (silent operation)
 * Exit: Always 0 (non-blocking)
 */

import * as fs from "node:fs";
import * as path from "node:path";
import { homedir } from "node:os";

// Environment variables
const API_URL = process.env.CONTEXTSTREAM_API_URL || "https://api.contextstream.io";
const API_KEY = process.env.CONTEXTSTREAM_API_KEY || "";
const ENABLED = process.env.CONTEXTSTREAM_AUTO_RULES !== "false";

// Track if we've already run in this session to avoid repeated calls
const MARKER_FILE = path.join(homedir(), ".contextstream", ".auto-rules-ran");
const COOLDOWN_MS = 4 * 60 * 60 * 1000; // 4 hour cooldown between auto-updates

interface HookInput {
  // Claude Code format
  tool_name?: string;
  tool_result?: string;
  tool_response?: {
    content?: Array<{ type: string; text: string }>;
    structuredContent?: Record<string, unknown>;
  };
  cwd?: string;

  // Alternative formats
  toolName?: string;
  result?: string;
  response?: Record<string, unknown>;
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

interface RulesNotice {
  status: "current" | "behind" | "missing";
  current?: string;
  latest?: string;
  files_outdated?: string[];
  update_tool?: string;
  update_args?: {
    folder_path?: string;
    editors?: string[];
  };
}

/**
 * Check if we've run recently (cooldown)
 */
function hasRunRecently(): boolean {
  try {
    if (!fs.existsSync(MARKER_FILE)) return false;
    const stat = fs.statSync(MARKER_FILE);
    const age = Date.now() - stat.mtimeMs;
    return age < COOLDOWN_MS;
  } catch {
    return false;
  }
}

/**
 * Mark that we've run
 */
function markAsRan(): void {
  try {
    const dir = path.dirname(MARKER_FILE);
    if (!fs.existsSync(dir)) {
      fs.mkdirSync(dir, { recursive: true });
    }
    fs.writeFileSync(MARKER_FILE, new Date().toISOString());
  } catch {
    // Ignore
  }
}

/**
 * Load API config from .mcp.json if env vars not set.
 */
function loadApiConfig(startDir: string): { apiUrl: string; apiKey: string } {
  let apiUrl = API_URL;
  let apiKey = API_KEY;

  if (apiKey) {
    return { apiUrl, apiKey };
  }

  // Search for .mcp.json
  let currentDir = path.resolve(startDir);
  for (let i = 0; i < 10; i++) {
    const mcpPath = path.join(currentDir, ".mcp.json");
    if (fs.existsSync(mcpPath)) {
      try {
        const content = fs.readFileSync(mcpPath, "utf-8");
        const config = JSON.parse(content) as McpConfig;
        const csEnv = config.mcpServers?.contextstream?.env;
        if (csEnv?.CONTEXTSTREAM_API_KEY) {
          apiKey = csEnv.CONTEXTSTREAM_API_KEY;
        }
        if (csEnv?.CONTEXTSTREAM_API_URL) {
          apiUrl = csEnv.CONTEXTSTREAM_API_URL;
        }
        if (apiKey) break;
      } catch {
        // Continue searching
      }
    }

    const parentDir = path.dirname(currentDir);
    if (parentDir === currentDir) break;
    currentDir = parentDir;
  }

  // Also check home directory
  if (!apiKey) {
    const homeMcpPath = path.join(homedir(), ".mcp.json");
    if (fs.existsSync(homeMcpPath)) {
      try {
        const content = fs.readFileSync(homeMcpPath, "utf-8");
        const config = JSON.parse(content) as McpConfig;
        const csEnv = config.mcpServers?.contextstream?.env;
        if (csEnv?.CONTEXTSTREAM_API_KEY) {
          apiKey = csEnv.CONTEXTSTREAM_API_KEY;
        }
        if (csEnv?.CONTEXTSTREAM_API_URL) {
          apiUrl = csEnv.CONTEXTSTREAM_API_URL;
        }
      } catch {
        // Ignore
      }
    }
  }

  return { apiUrl, apiKey };
}

/**
 * Extract rules_notice from tool result
 */
function extractRulesNotice(input: HookInput): RulesNotice | null {
  // Try to parse from tool_result string
  if (input.tool_result) {
    try {
      const parsed = JSON.parse(input.tool_result);
      if (parsed.rules_notice) return parsed.rules_notice;
    } catch {
      // Not JSON, try to find it in the string
    }
  }

  // Try structuredContent
  if (input.tool_response?.structuredContent) {
    const sc = input.tool_response.structuredContent;
    if (sc.rules_notice) return sc.rules_notice as RulesNotice;
  }

  // Try response
  if (input.response) {
    if ((input.response as Record<string, unknown>).rules_notice) {
      return (input.response as Record<string, unknown>).rules_notice as RulesNotice;
    }
  }

  return null;
}

/**
 * Extract working directory from hook input.
 */
function extractCwd(input: HookInput): string {
  if (input.cwd) return input.cwd;
  return process.cwd();
}

/**
 * Check if a settings.json file contains legacy Python hooks
 */
function hasPythonHooks(settingsPath: string): boolean {
  try {
    if (!fs.existsSync(settingsPath)) return false;
    const content = fs.readFileSync(settingsPath, "utf-8");
    const settings = JSON.parse(content);
    const hooks = settings.hooks;
    if (!hooks) return false;

    // Check all hook types for Python commands
    for (const hookType of Object.keys(hooks)) {
      const matchers = hooks[hookType];
      if (!Array.isArray(matchers)) continue;

      for (const matcher of matchers) {
        const hookList = matcher.hooks;
        if (!Array.isArray(hookList)) continue;

        for (const hook of hookList) {
          const cmd = hook.command || "";
          // Detect Python hooks for contextstream
          if (cmd.includes("python3") && cmd.includes("contextstream")) {
            return true;
          }
        }
      }
    }
    return false;
  } catch {
    return false;
  }
}

/**
 * Check for Python hooks in both global and project settings.
 * Returns the folder path that needs updating, or null if no Python hooks found.
 */
function detectPythonHooks(cwd: string): { global: boolean; project: boolean } {
  const globalSettingsPath = path.join(homedir(), ".claude", "settings.json");
  const projectSettingsPath = path.join(cwd, ".claude", "settings.json");

  return {
    global: hasPythonHooks(globalSettingsPath),
    project: hasPythonHooks(projectSettingsPath),
  };
}

/**
 * Upgrade hooks for a given folder.
 * Updates both global and project-level Claude Code hooks to Node.js versions.
 */
async function upgradeHooksForFolder(folderPath: string): Promise<void> {
  // Import the hooks config utilities dynamically to avoid loading heavy deps at startup
  const { installClaudeCodeHooks } = await import("../hooks-config.js");

  // Update both global (user) and project-level hooks
  await installClaudeCodeHooks({
    scope: "both",
    projectPath: folderPath,
    includePreCompact: true,
    includeMediaAware: true,
    includePostWrite: true,
    includeAutoRules: true,
  });
}

/**
 * Main hook entry point.
 */
export async function runAutoRulesHook(): Promise<void> {
  // Exit early if disabled
  if (!ENABLED) {
    process.exit(0);
  }

  // Check cooldown
  if (hasRunRecently()) {
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

  // Only run for init/context tools
  const toolName = input.tool_name || input.toolName || "";
  const isContextTool = toolName.includes("init") ||
                        toolName.includes("context") ||
                        toolName.includes("session_init") ||
                        toolName.includes("context_smart");

  if (!isContextTool) {
    process.exit(0);
  }

  const cwd = extractCwd(input);

  // Check for legacy Python hooks (upgrade regardless of rules status)
  const pythonHooks = detectPythonHooks(cwd);
  const hasPythonHooksToUpgrade = pythonHooks.global || pythonHooks.project;

  // Extract rules_notice
  const rulesNotice = extractRulesNotice(input);
  const rulesNeedUpdate = rulesNotice && rulesNotice.status !== "current";

  // Exit if nothing needs updating
  if (!hasPythonHooksToUpgrade && !rulesNeedUpdate) {
    process.exit(0);
  }

  // Determine folder path for updates
  const folderPath = rulesNotice?.update_args?.folder_path || cwd;

  try {
    await upgradeHooksForFolder(folderPath);
    markAsRan();
  } catch {
    // Silently fail - don't block the editor
  }

  process.exit(0);
}

// Auto-run if executed directly
const isDirectRun = process.argv[1]?.includes("auto-rules") || process.argv[2] === "auto-rules";
if (isDirectRun) {
  runAutoRulesHook().catch(() => process.exit(0));
}
