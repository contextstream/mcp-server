/**
 * Claude Code hooks configuration for ContextStream.
 *
 * These hooks help enforce ContextStream-first search behavior:
 * 1. PreToolUse: Blocks Grep/Glob/Read/Search and redirects to ContextStream search
 * 2. UserPromptSubmit: Injects reminder about ContextStream rules on every message
 */

import * as fs from "node:fs/promises";
import * as path from "node:path";
import { homedir } from "node:os";

export interface ClaudeHook {
  type: "command";
  command: string;
  timeout?: number;
}

export interface ClaudeHookMatcher {
  matcher: string;
  hooks: ClaudeHook[];
}

export interface ClaudeHooksConfig {
  hooks?: {
    PreToolUse?: ClaudeHookMatcher[];
    UserPromptSubmit?: ClaudeHookMatcher[];
    PostToolUse?: ClaudeHookMatcher[];
    SessionStart?: ClaudeHookMatcher[];
    [key: string]: ClaudeHookMatcher[] | undefined;
  };
}

/**
 * The PreToolUse hook script that blocks discovery tools.
 * This is embedded so we can install it without network access.
 */
export const PRETOOLUSE_HOOK_SCRIPT = `#!/usr/bin/env python3
"""
ContextStream PreToolUse Hook for Claude Code
Blocks Grep/Glob/Search/Task(Explore)/EnterPlanMode and redirects to ContextStream.
"""

import json
import sys
import os

ENABLED = os.environ.get("CONTEXTSTREAM_HOOK_ENABLED", "true").lower() == "true"

DISCOVERY_PATTERNS = ["**/*", "**/", "src/**", "lib/**", "app/**", "components/**"]

def is_discovery_glob(pattern):
    pattern_lower = pattern.lower()
    for p in DISCOVERY_PATTERNS:
        if p in pattern_lower:
            return True
    if pattern_lower.startswith("**/*.") or pattern_lower.startswith("**/"):
        return True
    if "**" in pattern or "*/" in pattern:
        return True
    return False

def is_discovery_grep(file_path):
    if not file_path or file_path in [".", "./", "*", "**"]:
        return True
    if "*" in file_path or "**" in file_path:
        return True
    return False

def main():
    if not ENABLED:
        sys.exit(0)

    try:
        data = json.load(sys.stdin)
    except:
        sys.exit(0)

    tool = data.get("tool_name", "")
    inp = data.get("tool_input", {})

    if tool == "Glob":
        pattern = inp.get("pattern", "")
        if is_discovery_glob(pattern):
            print(f"STOP: Use mcp__contextstream__search(mode=\\"hybrid\\", query=\\"{pattern}\\") instead of Glob.", file=sys.stderr)
            sys.exit(2)

    elif tool == "Grep" or tool == "Search":
        # Block ALL Grep/Search operations - use ContextStream search or Read for specific files
        pattern = inp.get("pattern", "")
        path = inp.get("path", "")
        if pattern:
            if path and not is_discovery_grep(path):
                # Specific file - suggest Read instead
                print(f"STOP: Use Read(\\"{path}\\") to view file content, or mcp__contextstream__search(mode=\\"keyword\\", query=\\"{pattern}\\") for codebase search.", file=sys.stderr)
            else:
                print(f"STOP: Use mcp__contextstream__search(mode=\\"hybrid\\", query=\\"{pattern}\\") instead of {tool}.", file=sys.stderr)
            sys.exit(2)

    elif tool == "Task":
        if inp.get("subagent_type", "").lower() == "explore":
            print("STOP: Use mcp__contextstream__search(mode=\\"hybrid\\") instead of Task(Explore).", file=sys.stderr)
            sys.exit(2)
        if inp.get("subagent_type", "").lower() == "plan":
            print("STOP: Use mcp__contextstream__session(action=\\"capture_plan\\") for planning. ContextStream plans persist across sessions.", file=sys.stderr)
            sys.exit(2)

    elif tool == "EnterPlanMode":
        print("STOP: Use mcp__contextstream__session(action=\\"capture_plan\\", title=\\"...\\", steps=[...]) instead of EnterPlanMode. ContextStream plans persist across sessions and are searchable.", file=sys.stderr)
        sys.exit(2)

    sys.exit(0)

if __name__ == "__main__":
    main()
`;

/**
 * The UserPromptSubmit hook script that injects reminders.
 */
export const USER_PROMPT_HOOK_SCRIPT = `#!/usr/bin/env python3
"""
ContextStream UserPromptSubmit Hook - Injects reminder on every message.
"""

import json
import sys
import os

ENABLED = os.environ.get("CONTEXTSTREAM_REMINDER_ENABLED", "true").lower() == "true"

REMINDER = """[CONTEXTSTREAM RULES]
1. BEFORE Glob/Grep/Read/Search: mcp__contextstream__search(mode="hybrid") FIRST
2. Call context_smart at start of EVERY response
3. Local tools ONLY if ContextStream returns 0 results
[END RULES]"""

def main():
    if not ENABLED:
        sys.exit(0)

    try:
        json.load(sys.stdin)
    except:
        sys.exit(0)

    print(json.dumps({"hookSpecificOutput": {"hookEventName": "UserPromptSubmit", "additionalContext": REMINDER}}))
    sys.exit(0)

if __name__ == "__main__":
    main()
`;

/**
 * Get the path to Claude Code's settings file.
 */
export function getClaudeSettingsPath(scope: "user" | "project", projectPath?: string): string {
  if (scope === "user") {
    return path.join(homedir(), ".claude", "settings.json");
  }
  if (!projectPath) {
    throw new Error("projectPath required for project scope");
  }
  return path.join(projectPath, ".claude", "settings.json");
}

/**
 * Get the path to store hook scripts.
 */
export function getHooksDir(): string {
  return path.join(homedir(), ".claude", "hooks");
}

/**
 * Build the hooks configuration for Claude Code settings.
 */
export function buildHooksConfig(): ClaudeHooksConfig["hooks"] {
  const hooksDir = getHooksDir();
  const preToolUsePath = path.join(hooksDir, "contextstream-redirect.py");
  const userPromptPath = path.join(hooksDir, "contextstream-reminder.py");

  return {
    PreToolUse: [
      {
        matcher: "Glob|Grep|Search|Task|EnterPlanMode",
        hooks: [
          {
            type: "command",
            command: `python3 "${preToolUsePath}"`,
            timeout: 5,
          },
        ],
      },
    ],
    UserPromptSubmit: [
      {
        matcher: "*",
        hooks: [
          {
            type: "command",
            command: `python3 "${userPromptPath}"`,
            timeout: 5,
          },
        ],
      },
    ],
  };
}

/**
 * Install hook scripts to ~/.claude/hooks/
 */
export async function installHookScripts(): Promise<{ preToolUse: string; userPrompt: string }> {
  const hooksDir = getHooksDir();
  await fs.mkdir(hooksDir, { recursive: true });

  const preToolUsePath = path.join(hooksDir, "contextstream-redirect.py");
  const userPromptPath = path.join(hooksDir, "contextstream-reminder.py");

  await fs.writeFile(preToolUsePath, PRETOOLUSE_HOOK_SCRIPT, { mode: 0o755 });
  await fs.writeFile(userPromptPath, USER_PROMPT_HOOK_SCRIPT, { mode: 0o755 });

  return { preToolUse: preToolUsePath, userPrompt: userPromptPath };
}

/**
 * Read existing Claude Code settings.
 */
export async function readClaudeSettings(
  scope: "user" | "project",
  projectPath?: string
): Promise<Record<string, unknown>> {
  const settingsPath = getClaudeSettingsPath(scope, projectPath);
  try {
    const content = await fs.readFile(settingsPath, "utf-8");
    return JSON.parse(content);
  } catch {
    return {};
  }
}

/**
 * Write Claude Code settings.
 */
export async function writeClaudeSettings(
  settings: Record<string, unknown>,
  scope: "user" | "project",
  projectPath?: string
): Promise<void> {
  const settingsPath = getClaudeSettingsPath(scope, projectPath);
  const dir = path.dirname(settingsPath);
  await fs.mkdir(dir, { recursive: true });
  await fs.writeFile(settingsPath, JSON.stringify(settings, null, 2));
}

/**
 * Merge hooks into existing settings without overwriting other hooks.
 */
export function mergeHooksIntoSettings(
  existingSettings: Record<string, unknown>,
  newHooks: ClaudeHooksConfig["hooks"]
): Record<string, unknown> {
  const settings = { ...existingSettings };
  const existingHooks = (settings.hooks || {}) as ClaudeHooksConfig["hooks"];

  // Merge each hook type
  for (const [hookType, matchers] of Object.entries(newHooks || {})) {
    if (!matchers) continue;

    const existing = existingHooks?.[hookType] || [];

    // Remove any existing ContextStream hooks (by checking command path)
    const filtered = existing.filter((m) => {
      return !m.hooks?.some((h) => h.command?.includes("contextstream"));
    });

    // Add new hooks
    existingHooks![hookType] = [...filtered, ...matchers];
  }

  settings.hooks = existingHooks;
  return settings;
}

/**
 * Install ContextStream hooks for Claude Code.
 * This installs the hook scripts and updates settings.
 */
export async function installClaudeCodeHooks(options: {
  scope: "user" | "project" | "both";
  projectPath?: string;
  dryRun?: boolean;
}): Promise<{ scripts: string[]; settings: string[] }> {
  const result = { scripts: [] as string[], settings: [] as string[] };

  // Install hook scripts
  if (!options.dryRun) {
    const scripts = await installHookScripts();
    result.scripts.push(scripts.preToolUse, scripts.userPrompt);
  } else {
    const hooksDir = getHooksDir();
    result.scripts.push(
      path.join(hooksDir, "contextstream-redirect.py"),
      path.join(hooksDir, "contextstream-reminder.py")
    );
  }

  const hooksConfig = buildHooksConfig();

  // Update user settings
  if (options.scope === "user" || options.scope === "both") {
    const settingsPath = getClaudeSettingsPath("user");
    if (!options.dryRun) {
      const existing = await readClaudeSettings("user");
      const merged = mergeHooksIntoSettings(existing, hooksConfig);
      await writeClaudeSettings(merged, "user");
    }
    result.settings.push(settingsPath);
  }

  // Update project settings
  if ((options.scope === "project" || options.scope === "both") && options.projectPath) {
    const settingsPath = getClaudeSettingsPath("project", options.projectPath);
    if (!options.dryRun) {
      const existing = await readClaudeSettings("project", options.projectPath);
      const merged = mergeHooksIntoSettings(existing, hooksConfig);
      await writeClaudeSettings(merged, "project", options.projectPath);
    }
    result.settings.push(settingsPath);
  }

  return result;
}

/**
 * Generate a markdown explanation of the hooks for users.
 */
export function generateHooksDocumentation(): string {
  return `
## Claude Code Hooks (ContextStream)

ContextStream installs hooks to help enforce ContextStream-first behavior:

### PreToolUse Hook
- **File:** \`~/.claude/hooks/contextstream-redirect.py\`
- **Purpose:** Blocks Glob/Grep/Search/EnterPlanMode and redirects to ContextStream
- **Blocked tools:** Glob, Grep, Search, Task(Explore), Task(Plan), EnterPlanMode
- **Disable:** Set \`CONTEXTSTREAM_HOOK_ENABLED=false\` environment variable

### UserPromptSubmit Hook
- **File:** \`~/.claude/hooks/contextstream-reminder.py\`
- **Purpose:** Injects a reminder about ContextStream rules on every message
- **Disable:** Set \`CONTEXTSTREAM_REMINDER_ENABLED=false\` environment variable

### Why Hooks?
Claude Code has strong built-in behaviors to use its default tools (Grep, Glob, Read)
and its built-in plan mode. CLAUDE.md instructions decay over long conversations.
Hooks provide:
1. **Physical enforcement** - Blocked tools can't be used
2. **Continuous reminders** - Rules stay in recent context
3. **Better UX** - Faster searches via indexed ContextStream
4. **Persistent plans** - ContextStream plans survive across sessions

### Manual Configuration
If you prefer to configure manually, add to \`~/.claude/settings.json\`:
\`\`\`json
{
  "hooks": {
    "PreToolUse": [{
      "matcher": "Glob|Grep|Search|Task|EnterPlanMode",
      "hooks": [{"type": "command", "command": "python3 ~/.claude/hooks/contextstream-redirect.py"}]
    }],
    "UserPromptSubmit": [{
      "matcher": "*",
      "hooks": [{"type": "command", "command": "python3 ~/.claude/hooks/contextstream-reminder.py"}]
    }]
  }
}
\`\`\`
`.trim();
}
