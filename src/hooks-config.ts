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

Only blocks if the current project is indexed in ContextStream.
If not indexed, allows local tools through with a suggestion to index.
"""

import json
import sys
import os
from pathlib import Path
from datetime import datetime, timedelta

ENABLED = os.environ.get("CONTEXTSTREAM_HOOK_ENABLED", "true").lower() == "true"
INDEX_STATUS_FILE = Path.home() / ".contextstream" / "indexed-projects.json"
# Consider index stale after 7 days
STALE_THRESHOLD_DAYS = 7

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

def is_project_indexed(cwd: str) -> tuple[bool, bool]:
    """
    Check if the current directory is in an indexed project.
    Returns (is_indexed, is_stale).
    """
    if not INDEX_STATUS_FILE.exists():
        return False, False

    try:
        with open(INDEX_STATUS_FILE, "r") as f:
            data = json.load(f)
    except:
        return False, False

    projects = data.get("projects", {})
    cwd_path = Path(cwd).resolve()

    # Check if cwd is within any indexed project
    for project_path, info in projects.items():
        try:
            indexed_path = Path(project_path).resolve()
            # Check if cwd is the project or a subdirectory
            if cwd_path == indexed_path or indexed_path in cwd_path.parents:
                # Check if stale
                indexed_at = info.get("indexed_at")
                if indexed_at:
                    try:
                        indexed_time = datetime.fromisoformat(indexed_at.replace("Z", "+00:00"))
                        if datetime.now(indexed_time.tzinfo) - indexed_time > timedelta(days=STALE_THRESHOLD_DAYS):
                            return True, True  # Indexed but stale
                    except:
                        pass
                return True, False  # Indexed and fresh
        except:
            continue

    return False, False

def main():
    if not ENABLED:
        sys.exit(0)

    try:
        data = json.load(sys.stdin)
    except:
        sys.exit(0)

    tool = data.get("tool_name", "")
    inp = data.get("tool_input", {})
    cwd = data.get("cwd", os.getcwd())

    # Check if project is indexed
    is_indexed, is_stale = is_project_indexed(cwd)

    if not is_indexed:
        # Project not indexed - allow local tools but suggest indexing
        # Don't block, just exit successfully
        sys.exit(0)

    if is_stale:
        # Index is stale - allow with warning (printed but not blocking)
        # Still allow the tool but remind about re-indexing
        pass  # Continue to blocking logic but could add warning

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
 * The PreCompact hook script that saves conversation state before compaction.
 * This runs before Claude Code compacts the context (either manual /compact or auto).
 */
export const PRECOMPACT_HOOK_SCRIPT = `#!/usr/bin/env python3
"""
ContextStream PreCompact Hook for Claude Code

Runs BEFORE conversation context is compacted (manual via /compact or automatic).
Automatically saves conversation state to ContextStream by parsing the transcript.

Input (via stdin):
{
  "session_id": "...",
  "transcript_path": "/path/to/transcript.jsonl",
  "permission_mode": "default",
  "hook_event_name": "PreCompact",
  "trigger": "manual" | "auto",
  "custom_instructions": "..."
}

Output (to stdout):
{
  "hookSpecificOutput": {
    "hookEventName": "PreCompact",
    "additionalContext": "... status message ..."
  }
}
"""

import json
import sys
import os
import re
import urllib.request
import urllib.error

ENABLED = os.environ.get("CONTEXTSTREAM_PRECOMPACT_ENABLED", "true").lower() == "true"
AUTO_SAVE = os.environ.get("CONTEXTSTREAM_PRECOMPACT_AUTO_SAVE", "true").lower() == "true"
API_URL = os.environ.get("CONTEXTSTREAM_API_URL", "https://api.contextstream.io")
API_KEY = os.environ.get("CONTEXTSTREAM_API_KEY", "")

def parse_transcript(transcript_path):
    """Parse transcript to extract active files, decisions, and context."""
    active_files = set()
    recent_messages = []
    tool_calls = []

    try:
        with open(transcript_path, 'r') as f:
            for line in f:
                try:
                    entry = json.loads(line.strip())
                    msg_type = entry.get("type", "")

                    # Extract files from tool calls
                    if msg_type == "tool_use":
                        tool_name = entry.get("name", "")
                        tool_input = entry.get("input", {})
                        tool_calls.append({"name": tool_name, "input": tool_input})

                        # Extract file paths from common tools
                        if tool_name in ["Read", "Write", "Edit", "NotebookEdit"]:
                            file_path = tool_input.get("file_path") or tool_input.get("notebook_path")
                            if file_path:
                                active_files.add(file_path)
                        elif tool_name == "Glob":
                            pattern = tool_input.get("pattern", "")
                            if pattern:
                                active_files.add(f"[glob:{pattern}]")

                    # Collect recent assistant messages for summary
                    if msg_type == "assistant" and entry.get("content"):
                        content = entry.get("content", "")
                        if isinstance(content, str) and len(content) > 50:
                            recent_messages.append(content[:500])

                except json.JSONDecodeError:
                    continue
    except Exception as e:
        pass

    return {
        "active_files": list(active_files)[-20:],  # Last 20 files
        "tool_call_count": len(tool_calls),
        "message_count": len(recent_messages),
        "last_tools": [t["name"] for t in tool_calls[-10:]],  # Last 10 tool names
    }

def save_snapshot(session_id, transcript_data, trigger):
    """Save snapshot to ContextStream API."""
    if not API_KEY:
        return False, "No API key configured"

    snapshot_content = {
        "session_id": session_id,
        "trigger": trigger,
        "captured_at": None,  # API will set timestamp
        "active_files": transcript_data.get("active_files", []),
        "tool_call_count": transcript_data.get("tool_call_count", 0),
        "last_tools": transcript_data.get("last_tools", []),
        "auto_captured": True,
    }

    payload = {
        "event_type": "session_snapshot",
        "title": f"Auto Pre-compaction Snapshot ({trigger})",
        "content": json.dumps(snapshot_content),
        "importance": "high",
        "tags": ["session_snapshot", "pre_compaction", "auto_captured"],
        "source_type": "hook",
    }

    try:
        req = urllib.request.Request(
            f"{API_URL}/api/v1/memory/events",
            data=json.dumps(payload).encode('utf-8'),
            headers={
                "Content-Type": "application/json",
                "X-API-Key": API_KEY,
            },
            method="POST"
        )
        with urllib.request.urlopen(req, timeout=5) as resp:
            return True, "Snapshot saved"
    except urllib.error.URLError as e:
        return False, str(e)
    except Exception as e:
        return False, str(e)

def main():
    if not ENABLED:
        sys.exit(0)

    try:
        data = json.load(sys.stdin)
    except:
        sys.exit(0)

    session_id = data.get("session_id", "unknown")
    transcript_path = data.get("transcript_path", "")
    trigger = data.get("trigger", "unknown")
    custom_instructions = data.get("custom_instructions", "")

    # Parse transcript for context
    transcript_data = {}
    if transcript_path and os.path.exists(transcript_path):
        transcript_data = parse_transcript(transcript_path)

    # Auto-save snapshot if enabled
    auto_save_status = ""
    if AUTO_SAVE and API_KEY:
        success, msg = save_snapshot(session_id, transcript_data, trigger)
        if success:
            auto_save_status = f"\\n[ContextStream: Auto-saved snapshot with {len(transcript_data.get('active_files', []))} active files]"
        else:
            auto_save_status = f"\\n[ContextStream: Auto-save failed - {msg}]"

    # Build context injection for the AI (backup in case auto-save fails)
    files_list = ", ".join(transcript_data.get("active_files", [])[:5]) or "none detected"
    context = f"""[CONTEXT COMPACTION - {trigger.upper()}]{auto_save_status}

Active files detected: {files_list}
Tool calls in session: {transcript_data.get('tool_call_count', 0)}

After compaction, call session_init(is_post_compact=true) to restore context.
{f"User instructions: {custom_instructions}" if custom_instructions else ""}"""

    output = {
        "hookSpecificOutput": {
            "hookEventName": "PreCompact",
            "additionalContext": context
        }
    }

    print(json.dumps(output))
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
export function buildHooksConfig(options?: {
  includePreCompact?: boolean;
}): ClaudeHooksConfig["hooks"] {
  const hooksDir = getHooksDir();
  const preToolUsePath = path.join(hooksDir, "contextstream-redirect.py");
  const userPromptPath = path.join(hooksDir, "contextstream-reminder.py");
  const preCompactPath = path.join(hooksDir, "contextstream-precompact.py");

  const config: ClaudeHooksConfig["hooks"] = {
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

  // Add PreCompact hook for context compaction awareness (opt-in)
  if (options?.includePreCompact) {
    config.PreCompact = [
      {
        // Match both manual (/compact) and automatic compaction
        matcher: "*",
        hooks: [
          {
            type: "command",
            command: `python3 "${preCompactPath}"`,
            timeout: 10,
          },
        ],
      },
    ];
  }

  return config;
}

/**
 * Install hook scripts to ~/.claude/hooks/
 */
export async function installHookScripts(options?: {
  includePreCompact?: boolean;
}): Promise<{ preToolUse: string; userPrompt: string; preCompact?: string }> {
  const hooksDir = getHooksDir();
  await fs.mkdir(hooksDir, { recursive: true });

  const preToolUsePath = path.join(hooksDir, "contextstream-redirect.py");
  const userPromptPath = path.join(hooksDir, "contextstream-reminder.py");
  const preCompactPath = path.join(hooksDir, "contextstream-precompact.py");

  await fs.writeFile(preToolUsePath, PRETOOLUSE_HOOK_SCRIPT, { mode: 0o755 });
  await fs.writeFile(userPromptPath, USER_PROMPT_HOOK_SCRIPT, { mode: 0o755 });

  const result: { preToolUse: string; userPrompt: string; preCompact?: string } = {
    preToolUse: preToolUsePath,
    userPrompt: userPromptPath,
  };

  // Install PreCompact hook script if requested
  if (options?.includePreCompact) {
    await fs.writeFile(preCompactPath, PRECOMPACT_HOOK_SCRIPT, { mode: 0o755 });
    result.preCompact = preCompactPath;
  }

  return result;
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
  includePreCompact?: boolean;
}): Promise<{ scripts: string[]; settings: string[] }> {
  const result = { scripts: [] as string[], settings: [] as string[] };

  // Install hook scripts
  if (!options.dryRun) {
    const scripts = await installHookScripts({ includePreCompact: options.includePreCompact });
    result.scripts.push(scripts.preToolUse, scripts.userPrompt);
    if (scripts.preCompact) {
      result.scripts.push(scripts.preCompact);
    }
  } else {
    const hooksDir = getHooksDir();
    result.scripts.push(
      path.join(hooksDir, "contextstream-redirect.py"),
      path.join(hooksDir, "contextstream-reminder.py")
    );
    if (options.includePreCompact) {
      result.scripts.push(path.join(hooksDir, "contextstream-precompact.py"));
    }
  }

  const hooksConfig = buildHooksConfig({ includePreCompact: options.includePreCompact });

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

### PreCompact Hook (Optional)
- **File:** \`~/.claude/hooks/contextstream-precompact.py\`
- **Purpose:** Saves conversation state before context compaction
- **Triggers:** Both manual (/compact) and automatic compaction
- **Disable:** Set \`CONTEXTSTREAM_PRECOMPACT_ENABLED=false\` environment variable
- **Note:** Enable with \`generate_rules(install_hooks=true)\` to activate

When PreCompact runs, it injects instructions for the AI to:
1. Save a session_snapshot with conversation summary, active goals, and decisions
2. Use \`session_init(is_post_compact=true)\` after compaction to restore context

### Why Hooks?
Claude Code has strong built-in behaviors to use its default tools (Grep, Glob, Read)
and its built-in plan mode. CLAUDE.md instructions decay over long conversations.
Hooks provide:
1. **Physical enforcement** - Blocked tools can't be used
2. **Continuous reminders** - Rules stay in recent context
3. **Better UX** - Faster searches via indexed ContextStream
4. **Persistent plans** - ContextStream plans survive across sessions
5. **Compaction awareness** - Save state before context is compacted

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
    }],
    "PreCompact": [{
      "matcher": "*",
      "hooks": [{"type": "command", "command": "python3 ~/.claude/hooks/contextstream-precompact.py", "timeout": 10}]
    }]
  }
}
\`\`\`
`.trim();
}

/**
 * Index status file path for tracking which projects are indexed.
 * The hook script reads this to decide whether to block local tools.
 */
export function getIndexStatusPath(): string {
  return path.join(homedir(), ".contextstream", "indexed-projects.json");
}

export interface IndexedProjectInfo {
  indexed_at: string;
  project_id?: string;
  project_name?: string;
}

export interface IndexStatusFile {
  version: number;
  projects: Record<string, IndexedProjectInfo>;
}

/**
 * Read the current index status file.
 */
export async function readIndexStatus(): Promise<IndexStatusFile> {
  const statusPath = getIndexStatusPath();
  try {
    const content = await fs.readFile(statusPath, "utf-8");
    return JSON.parse(content);
  } catch {
    return { version: 1, projects: {} };
  }
}

/**
 * Write the index status file.
 */
export async function writeIndexStatus(status: IndexStatusFile): Promise<void> {
  const statusPath = getIndexStatusPath();
  const dir = path.dirname(statusPath);
  await fs.mkdir(dir, { recursive: true });
  await fs.writeFile(statusPath, JSON.stringify(status, null, 2));
}

/**
 * Mark a project as indexed. Called after successful ingest_local or index.
 */
export async function markProjectIndexed(
  projectPath: string,
  options?: { project_id?: string; project_name?: string }
): Promise<void> {
  const status = await readIndexStatus();
  const resolvedPath = path.resolve(projectPath);

  status.projects[resolvedPath] = {
    indexed_at: new Date().toISOString(),
    project_id: options?.project_id,
    project_name: options?.project_name,
  };

  await writeIndexStatus(status);
}

/**
 * Remove a project from the index status (e.g., on delete or explicit removal).
 */
export async function unmarkProjectIndexed(projectPath: string): Promise<void> {
  const status = await readIndexStatus();
  const resolvedPath = path.resolve(projectPath);

  delete status.projects[resolvedPath];

  await writeIndexStatus(status);
}
