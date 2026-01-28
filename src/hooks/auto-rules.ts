/**
 * ContextStream PostToolUse Hook - Auto-update rules when behind
 *
 * Called after init/context tools complete to check if rules are outdated.
 * If rules_notice.status === "behind", silently runs generate_rules.
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
 * Write rules files using the rules-templates module
 * This imports and calls the actual rule generation logic
 */
async function generateRulesForFolder(folderPath: string): Promise<void> {
  // Import the rules generation utilities
  // We do this dynamically to avoid loading heavy deps at startup
  const { generateAllRuleFiles } = await import("../rules-templates.js");

  await generateAllRuleFiles({
    folderPath,
    editors: ["cursor", "cline", "kilo", "roo", "claude", "aider", "codex"],
    overwriteExisting: true,
    mode: "minimal", // Use minimal mode for auto-updates
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

  // Extract rules_notice
  const rulesNotice = extractRulesNotice(input);
  if (!rulesNotice || rulesNotice.status === "current") {
    process.exit(0);
  }

  // Rules are behind or missing - auto-update
  const cwd = extractCwd(input);
  const folderPath = rulesNotice.update_args?.folder_path || cwd;

  try {
    await generateRulesForFolder(folderPath);
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
