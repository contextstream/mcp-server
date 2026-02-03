/**
 * ContextStream session-init Hook - Full context injection on session start
 *
 * SessionStart hook that injects full context when a new Claude Code session begins.
 * Fetches workspace context, recent decisions, and active plans.
 *
 * Usage:
 *   npx @contextstream/mcp-server hook session-init
 *
 * Input (stdin): JSON with session_id, cwd
 * Output (stdout): JSON with hookSpecificOutput containing context
 * Exit: Always 0
 */

import * as fs from "node:fs";
import * as path from "node:path";
import { homedir } from "node:os";
import {
  attemptAutoUpdate,
  checkUpdateMarker,
  clearUpdateMarker,
  getUpdateNotice,
  getVersionNoticeForHook,
  isAutoUpdateEnabled,
  VERSION,
  type AutoUpdateResult,
} from "../version.js";
import { generateRuleContent, getAvailableEditors, RULES_VERSION } from "../rules-templates.js";

const ENABLED = process.env.CONTEXTSTREAM_SESSION_INIT_ENABLED !== "false";

let API_URL = process.env.CONTEXTSTREAM_API_URL || "https://api.contextstream.io";
let API_KEY = process.env.CONTEXTSTREAM_API_KEY || "";
let WORKSPACE_ID: string | null = null;
let PROJECT_ID: string | null = null;

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
        CONTEXTSTREAM_WORKSPACE_ID?: string;
      };
    };
  };
}

interface LocalConfig {
  workspace_id?: string;
  project_id?: string;
}

interface ContextResponse {
  rules?: string;
  lessons?: Array<{ title: string; trigger: string; prevention: string }>;
  recent_decisions?: Array<{ title: string; content: string }>;
  active_plans?: Array<{ title: string; status: string }>;
  pending_tasks?: Array<{ title: string; status: string }>;
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
          if (csEnv?.CONTEXTSTREAM_WORKSPACE_ID) {
            WORKSPACE_ID = csEnv.CONTEXTSTREAM_WORKSPACE_ID;
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

async function fetchSessionContext(): Promise<ContextResponse | null> {
  if (!API_KEY) return null;

  try {
    const controller = new AbortController();
    const timeoutId = setTimeout(() => controller.abort(), 5000);

    const url = new URL(`${API_URL}/api/v1/context`);
    if (WORKSPACE_ID) url.searchParams.set("workspace_id", WORKSPACE_ID);
    if (PROJECT_ID) url.searchParams.set("project_id", PROJECT_ID);
    url.searchParams.set("include_rules", "true");
    url.searchParams.set("include_lessons", "true");
    url.searchParams.set("include_decisions", "true");
    url.searchParams.set("include_plans", "true");
    url.searchParams.set("limit", "5");

    const response = await fetch(url.toString(), {
      method: "GET",
      headers: {
        "X-API-Key": API_KEY,
      },
      signal: controller.signal,
    });

    clearTimeout(timeoutId);

    if (response.ok) {
      return (await response.json()) as ContextResponse;
    }
    return null;
  } catch {
    return null;
  }
}

interface FormatOptions {
  autoUpdateResult?: AutoUpdateResult | null;
  versionNotice?: { current: string; latest: string; behind: true; upgrade_command: string } | null;
  updateMarker?: { previousVersion: string; newVersion: string } | null;
}

function formatContext(ctx: ContextResponse | null, options: FormatOptions = {}): string {
  const parts: string[] = ["⬡ ContextStream — Smart Context & Memory", ""];

  // Show restart message if update was performed
  if (options.updateMarker) {
    parts.push(`\n## 🎉 UPDATE COMPLETE - RESTART CHAT SESSION`);
    parts.push(`ContextStream was auto-updated from v${options.updateMarker.previousVersion} to v${options.updateMarker.newVersion}.`);
    parts.push(`**Tell the user:** "ContextStream has been updated! Please start a new chat session (or restart your AI tool) to use the new version with the latest features and fixes."`);
    parts.push("");
  }
  // Show auto-update result if update was just attempted
  else if (options.autoUpdateResult?.attempted) {
    if (options.autoUpdateResult.success) {
      parts.push(`\n## 🔄 AUTO-UPDATE INITIATED`);
      parts.push(`ContextStream is updating from v${options.autoUpdateResult.previousVersion} to v${options.autoUpdateResult.newVersion}.`);
      parts.push(`**Tell the user:** "ContextStream is updating in the background. Please start a new chat session (or restart your AI tool) in a moment to use the new version."`);
      parts.push("");
    } else if (options.autoUpdateResult.error) {
      // Update failed - show manual instructions
      parts.push(`\n## ⚠️ AUTO-UPDATE FAILED`);
      parts.push(`Automatic update failed: ${options.autoUpdateResult.error}`);
      const versionInfo = getVersionNoticeForHook(options.versionNotice || null);
      if (versionInfo) {
        parts.push(versionInfo);
      }
      parts.push("");
    }
  }
  // Show version notice if behind and auto-update is disabled
  else if (options.versionNotice?.behind && !isAutoUpdateEnabled()) {
    const versionInfo = getVersionNoticeForHook(options.versionNotice);
    if (versionInfo) {
      parts.push(`\n## 🔄 UPDATE AVAILABLE (auto-update disabled)`);
      parts.push(versionInfo);
      parts.push("");
    }
  }

  if (!ctx) {
    parts.push("\nNo stored context found. Call `mcp__contextstream__context(user_message=\"starting new session\")` to initialize.");
    return parts.join("\n");
  }

  // Lessons (most important)
  if (ctx.lessons && ctx.lessons.length > 0) {
    parts.push("\n## ⚠️ Lessons from Past Mistakes");
    for (const lesson of ctx.lessons.slice(0, 3)) {
      parts.push(`- **${lesson.title}**: ${lesson.prevention}`);
    }
  }

  // Active plans
  if (ctx.active_plans && ctx.active_plans.length > 0) {
    parts.push("\n## 📋 Active Plans");
    for (const plan of ctx.active_plans.slice(0, 3)) {
      parts.push(`- ${plan.title} (${plan.status})`);
    }
  }

  // Pending tasks
  if (ctx.pending_tasks && ctx.pending_tasks.length > 0) {
    parts.push("\n## ✅ Pending Tasks");
    for (const task of ctx.pending_tasks.slice(0, 5)) {
      parts.push(`- ${task.title}`);
    }
  }

  // Recent decisions
  if (ctx.recent_decisions && ctx.recent_decisions.length > 0) {
    parts.push("\n## 📝 Recent Decisions");
    for (const decision of ctx.recent_decisions.slice(0, 3)) {
      parts.push(`- **${decision.title}**`);
    }
  }

  parts.push("\n---");
  parts.push("Call `mcp__contextstream__context(user_message=\"...\")` for task-specific context.");

  return parts.join("\n");
}

const CONTEXTSTREAM_START_MARKER = "<!-- BEGIN ContextStream -->";
const CONTEXTSTREAM_END_MARKER = "<!-- END ContextStream -->";

/**
 * Regenerate rule files in a directory after an auto-update.
 * Only updates files that already have ContextStream blocks.
 */
function regenerateRuleFiles(folderPath: string): number {
  let updated = 0;
  const editors = getAvailableEditors();

  for (const editor of editors) {
    const rule = generateRuleContent(editor, { mode: "bootstrap" });
    if (!rule) continue;

    const filePath = path.join(folderPath, rule.filename);
    if (!fs.existsSync(filePath)) continue;

    try {
      const existing = fs.readFileSync(filePath, "utf8");
      // Only update files that have ContextStream markers
      const startIdx = existing.indexOf(CONTEXTSTREAM_START_MARKER);
      const endIdx = existing.indexOf(CONTEXTSTREAM_END_MARKER);
      if (startIdx === -1 || endIdx === -1 || endIdx <= startIdx) continue;

      // Replace the ContextStream block with new content
      const before = existing.substring(0, startIdx).trimEnd();
      const after = existing.substring(endIdx + CONTEXTSTREAM_END_MARKER.length).trimStart();
      const newBlock = `${CONTEXTSTREAM_START_MARKER}\n${rule.content.trim()}\n${CONTEXTSTREAM_END_MARKER}`;
      const merged = [before, newBlock, after].filter((p) => p.length > 0).join("\n\n");
      fs.writeFileSync(filePath, merged.trim() + "\n", "utf8");
      updated++;
    } catch {
      // Ignore individual file errors
    }
  }

  return updated;
}

export async function runSessionInitHook(): Promise<void> {
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

  // Check for pending update marker (update completed, needs restart)
  const updateMarker = checkUpdateMarker();
  if (updateMarker) {
    // Regenerate rule files with new version before clearing marker
    regenerateRuleFiles(cwd);
    clearUpdateMarker(); // Clear so we only show once
  }

  // Attempt auto-update if enabled and behind (runs in parallel with context fetch)
  const [context, autoUpdateResult, versionNotice] = await Promise.all([
    fetchSessionContext(),
    updateMarker ? Promise.resolve(null) : attemptAutoUpdate(), // Skip if already updated
    getUpdateNotice(),
  ]);

  const formattedContext = formatContext(context, {
    autoUpdateResult,
    versionNotice,
    updateMarker,
  });

  // Output Claude Code format
  console.log(
    JSON.stringify({
      hookSpecificOutput: {
        hookEventName: "SessionStart",
        additionalContext: formattedContext,
      },
    })
  );

  process.exit(0);
}

// Auto-run if executed directly
const isDirectRun = process.argv[1]?.includes("session-init") || process.argv[2] === "session-init";
if (isDirectRun) {
  runSessionInitHook().catch(() => process.exit(0));
}
