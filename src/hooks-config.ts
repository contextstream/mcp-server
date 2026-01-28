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
 * Media-aware hook that detects media-related prompts and injects tool guidance.
 * Triggers on patterns like: video, clips, Remotion, image, audio, etc.
 */
export const MEDIA_AWARE_HOOK_SCRIPT = `#!/usr/bin/env python3
"""
ContextStream Media-Aware Hook for Claude Code

Detects media-related prompts and injects context about the media tool.
"""

import json
import sys
import os
import re

ENABLED = os.environ.get("CONTEXTSTREAM_MEDIA_HOOK_ENABLED", "true").lower() == "true"

# Media patterns (case-insensitive)
PATTERNS = [
    r"\\b(video|videos|clip|clips|footage|keyframe)s?\\b",
    r"\\b(remotion|timeline|video\\s*edit)\\b",
    r"\\b(image|images|photo|photos|picture|thumbnail)s?\\b",
    r"\\b(audio|podcast|transcript|transcription|voice)\\b",
    r"\\b(media|asset|assets|creative|b-roll)\\b",
    r"\\b(find|search|show).*(clip|video|image|audio|footage|media)\\b",
]

COMPILED = [re.compile(p, re.IGNORECASE) for p in PATTERNS]

MEDIA_CONTEXT = """[MEDIA TOOLS AVAILABLE]
Your workspace may have indexed media. Use ContextStream media tools:

- **Search**: \`mcp__contextstream__media(action="search", query="description")\`
- **Get clip**: \`mcp__contextstream__media(action="get_clip", content_id="...", start="1:34", end="2:15", output_format="remotion|ffmpeg|raw")\`
- **List assets**: \`mcp__contextstream__media(action="list")\`
- **Index**: \`mcp__contextstream__media(action="index", file_path="...", content_type="video|audio|image|document")\`

For Remotion: use \`output_format="remotion"\` to get frame-based props.
[END MEDIA TOOLS]"""

def matches(text):
    return any(p.search(text) for p in COMPILED)

def main():
    if not ENABLED:
        sys.exit(0)

    try:
        data = json.load(sys.stdin)
    except:
        sys.exit(0)

    prompt = data.get("prompt", "")
    if not prompt:
        session = data.get("session", {})
        for msg in reversed(session.get("messages", [])):
            if msg.get("role") == "user":
                content = msg.get("content", "")
                prompt = content if isinstance(content, str) else ""
                if isinstance(content, list):
                    for b in content:
                        if isinstance(b, dict) and b.get("type") == "text":
                            prompt = b.get("text", "")
                            break
                break

    if not prompt or not matches(prompt):
        sys.exit(0)

    print(json.dumps({"hookSpecificOutput": {"hookEventName": "UserPromptSubmit", "additionalContext": MEDIA_CONTEXT}}))
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

WORKSPACE_ID = None

def load_config_from_mcp_json(cwd):
    """Load API config from .mcp.json if env vars not set."""
    global API_URL, API_KEY, WORKSPACE_ID

    # Try to find .mcp.json and .contextstream/config.json in cwd or parent directories
    search_dir = cwd
    for _ in range(5):  # Search up to 5 levels
        # Load API config from .mcp.json
        if not API_KEY:
            mcp_path = os.path.join(search_dir, ".mcp.json")
            if os.path.exists(mcp_path):
                try:
                    with open(mcp_path, 'r') as f:
                        config = json.load(f)
                    servers = config.get("mcpServers", {})
                    cs_config = servers.get("contextstream", {})
                    env = cs_config.get("env", {})
                    if env.get("CONTEXTSTREAM_API_KEY"):
                        API_KEY = env["CONTEXTSTREAM_API_KEY"]
                    if env.get("CONTEXTSTREAM_API_URL"):
                        API_URL = env["CONTEXTSTREAM_API_URL"]
                except:
                    pass

        # Load workspace_id from .contextstream/config.json
        if not WORKSPACE_ID:
            cs_config_path = os.path.join(search_dir, ".contextstream", "config.json")
            if os.path.exists(cs_config_path):
                try:
                    with open(cs_config_path, 'r') as f:
                        cs_config = json.load(f)
                    if cs_config.get("workspace_id"):
                        WORKSPACE_ID = cs_config["workspace_id"]
                except:
                    pass

        parent = os.path.dirname(search_dir)
        if parent == search_dir:
            break
        search_dir = parent

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

    # Add workspace_id if available
    if WORKSPACE_ID:
        payload["workspace_id"] = WORKSPACE_ID

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

    # Load config from .mcp.json if env vars not set
    cwd = data.get("cwd", os.getcwd())
    load_config_from_mcp_json(cwd)

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
 * All hooks now run via Node.js (npx) - no Python dependency required.
 */
export function buildHooksConfig(options?: {
  includePreCompact?: boolean;
  includeMediaAware?: boolean;
  includePostWrite?: boolean;
}): ClaudeHooksConfig["hooks"] {
  // Build UserPromptSubmit hooks array - always include reminder
  const userPromptHooks: ClaudeHookMatcher[] = [
    {
      matcher: "*",
      hooks: [
        {
          type: "command",
          command: "npx @contextstream/mcp-server hook user-prompt-submit",
          timeout: 5,
        },
      ],
    },
  ];

  // Add media-aware hook (enabled by default for creative workflows)
  if (options?.includeMediaAware !== false) {
    userPromptHooks.push({
      matcher: "*",
      hooks: [
        {
          type: "command",
          command: "npx @contextstream/mcp-server hook media-aware",
          timeout: 5,
        },
      ],
    });
  }

  const config: ClaudeHooksConfig["hooks"] = {
    PreToolUse: [
      {
        matcher: "Glob|Grep|Search|Task|EnterPlanMode",
        hooks: [
          {
            type: "command",
            command: "npx @contextstream/mcp-server hook pre-tool-use",
            timeout: 5,
          },
        ],
      },
    ],
    UserPromptSubmit: userPromptHooks,
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
            command: "npx @contextstream/mcp-server hook pre-compact",
            timeout: 10,
          },
        ],
      },
    ];
  }

  // Add PostToolUse hook for real-time file indexing (default ON)
  // This indexes files immediately after Edit/Write/NotebookEdit operations
  if (options?.includePostWrite !== false) {
    config.PostToolUse = [
      {
        matcher: "Edit|Write|NotebookEdit",
        hooks: [
          {
            type: "command",
            command: "npx @contextstream/mcp-server hook post-write",
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
 *
 * NOTE: As of v0.4.46+, all hooks run via npx @contextstream/mcp-server hook <name>
 * so no Python scripts need to be written to disk. This function is kept for
 * backwards compatibility but now only ensures the hooks directory exists.
 *
 * @deprecated Hooks no longer require script files - they run via npx
 */
export async function installHookScripts(options?: {
  includePreCompact?: boolean;
  includeMediaAware?: boolean;
}): Promise<{ preToolUse: string; userPrompt: string; preCompact?: string; mediaAware?: string }> {
  // Ensure hooks directory exists (for any legacy scripts)
  const hooksDir = getHooksDir();
  await fs.mkdir(hooksDir, { recursive: true });

  // Return placeholder paths - actual hooks run via npx commands
  const result: { preToolUse: string; userPrompt: string; preCompact?: string; mediaAware?: string } = {
    preToolUse: "npx @contextstream/mcp-server hook pre-tool-use",
    userPrompt: "npx @contextstream/mcp-server hook user-prompt-submit",
  };

  if (options?.includePreCompact) {
    result.preCompact = "npx @contextstream/mcp-server hook pre-compact";
  }

  if (options?.includeMediaAware !== false) {
    result.mediaAware = "npx @contextstream/mcp-server hook media-aware";
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
 * All hooks now run via npx @contextstream/mcp-server hook <name> - no Python required.
 * This function updates settings.json with the hook configuration.
 */
export async function installClaudeCodeHooks(options: {
  scope: "user" | "project" | "both";
  projectPath?: string;
  dryRun?: boolean;
  includePreCompact?: boolean;
  includeMediaAware?: boolean;
  includePostWrite?: boolean;
}): Promise<{ scripts: string[]; settings: string[] }> {
  const result = { scripts: [] as string[], settings: [] as string[] };

  // All hooks run via npx - list the commands that will be configured
  result.scripts.push(
    "npx @contextstream/mcp-server hook pre-tool-use",
    "npx @contextstream/mcp-server hook user-prompt-submit"
  );
  if (options.includePreCompact) {
    result.scripts.push("npx @contextstream/mcp-server hook pre-compact");
  }
  if (options.includeMediaAware !== false) {
    result.scripts.push("npx @contextstream/mcp-server hook media-aware");
  }
  if (options.includePostWrite !== false) {
    result.scripts.push("npx @contextstream/mcp-server hook post-write");
  }

  const hooksConfig = buildHooksConfig({
    includePreCompact: options.includePreCompact,
    includeMediaAware: options.includeMediaAware,
    includePostWrite: options.includePostWrite,
  });

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

ContextStream installs hooks to enforce ContextStream-first behavior.
All hooks run via Node.js - no Python dependency required.

### PreToolUse Hook
- **Command:** \`npx @contextstream/mcp-server hook pre-tool-use\`
- **Purpose:** Blocks Glob/Grep/Search/EnterPlanMode and redirects to ContextStream
- **Blocked tools:** Glob, Grep, Search, Task(Explore), Task(Plan), EnterPlanMode
- **Disable:** Set \`CONTEXTSTREAM_HOOK_ENABLED=false\` environment variable

### UserPromptSubmit Hook
- **Command:** \`npx @contextstream/mcp-server hook user-prompt-submit\`
- **Purpose:** Injects a reminder about ContextStream rules on every message
- **Disable:** Set \`CONTEXTSTREAM_REMINDER_ENABLED=false\` environment variable

### Media-Aware Hook
- **Command:** \`npx @contextstream/mcp-server hook media-aware\`
- **Purpose:** Detects media-related prompts and injects media tool guidance
- **Triggers:** Patterns like video, clips, Remotion, image, audio, creative assets
- **Disable:** Set \`CONTEXTSTREAM_MEDIA_HOOK_ENABLED=false\` environment variable

When Media-Aware hook detects media patterns, it injects context about:
- How to search indexed media assets
- How to get clips for Remotion (with frame-based props)
- How to index new media files

### PreCompact Hook (Optional)
- **Command:** \`npx @contextstream/mcp-server hook pre-compact\`
- **Purpose:** Saves conversation state before context compaction
- **Triggers:** Both manual (/compact) and automatic compaction
- **Disable:** Set \`CONTEXTSTREAM_PRECOMPACT_ENABLED=false\` environment variable
- **Note:** Enable with \`generate_rules(include_pre_compact=true)\` to activate

When PreCompact runs, it:
1. Parses the transcript for active files and tool calls
2. Saves a session_snapshot to ContextStream API
3. Injects context about using \`session_init(is_post_compact=true)\` after compaction

### PostToolUse Hook (Real-time Indexing)
- **Command:** \`npx @contextstream/mcp-server hook post-write\`
- **Purpose:** Indexes files immediately after Edit/Write/NotebookEdit operations
- **Matcher:** Edit|Write|NotebookEdit
- **Disable:** Set \`CONTEXTSTREAM_POSTWRITE_ENABLED=false\` environment variable

### Why Hooks?
Claude Code has strong built-in behaviors to use its default tools (Grep, Glob, Read)
and its built-in plan mode. CLAUDE.md instructions decay over long conversations.
Hooks provide:
1. **Physical enforcement** - Blocked tools can't be used
2. **Continuous reminders** - Rules stay in recent context
3. **Better UX** - Faster searches via indexed ContextStream
4. **Persistent plans** - ContextStream plans survive across sessions
5. **Compaction awareness** - Save state before context is compacted
6. **Real-time indexing** - Files indexed immediately after writes

### Manual Configuration
If you prefer to configure manually, add to \`~/.claude/settings.json\`:
\`\`\`json
{
  "hooks": {
    "PreToolUse": [{
      "matcher": "Glob|Grep|Search|Task|EnterPlanMode",
      "hooks": [{"type": "command", "command": "npx @contextstream/mcp-server hook pre-tool-use"}]
    }],
    "UserPromptSubmit": [
      {
        "matcher": "*",
        "hooks": [{"type": "command", "command": "npx @contextstream/mcp-server hook user-prompt-submit"}]
      },
      {
        "matcher": "*",
        "hooks": [{"type": "command", "command": "npx @contextstream/mcp-server hook media-aware"}]
      }
    ],
    "PreCompact": [{
      "matcher": "*",
      "hooks": [{"type": "command", "command": "npx @contextstream/mcp-server hook pre-compact", "timeout": 10}]
    }],
    "PostToolUse": [{
      "matcher": "Edit|Write|NotebookEdit",
      "hooks": [{"type": "command", "command": "npx @contextstream/mcp-server hook post-write", "timeout": 10}]
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

// =============================================================================
// CLINE HOOKS SUPPORT
// =============================================================================

/**
 * Cline PreToolUse hook script.
 * Uses JSON output format with cancel/contextModification fields.
 */
export const CLINE_PRETOOLUSE_HOOK_SCRIPT = `#!/usr/bin/env python3
"""
ContextStream PreToolUse Hook for Cline
Blocks discovery tools and redirects to ContextStream search.

Cline hooks use JSON output format:
{
  "cancel": true/false,
  "errorMessage": "optional error description",
  "contextModification": "optional text to inject"
}
"""

import json
import sys
import os
from pathlib import Path
from datetime import datetime, timedelta

ENABLED = os.environ.get("CONTEXTSTREAM_HOOK_ENABLED", "true").lower() == "true"
INDEX_STATUS_FILE = Path.home() / ".contextstream" / "indexed-projects.json"
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

def is_project_indexed(workspace_roots):
    """Check if any workspace root is in an indexed project."""
    if not INDEX_STATUS_FILE.exists():
        return False, False

    try:
        with open(INDEX_STATUS_FILE, "r") as f:
            data = json.load(f)
    except:
        return False, False

    projects = data.get("projects", {})

    for workspace in workspace_roots:
        cwd_path = Path(workspace).resolve()
        for project_path, info in projects.items():
            try:
                indexed_path = Path(project_path).resolve()
                if cwd_path == indexed_path or indexed_path in cwd_path.parents:
                    indexed_at = info.get("indexed_at")
                    if indexed_at:
                        try:
                            indexed_time = datetime.fromisoformat(indexed_at.replace("Z", "+00:00"))
                            if datetime.now(indexed_time.tzinfo) - indexed_time > timedelta(days=STALE_THRESHOLD_DAYS):
                                return True, True
                        except:
                            pass
                    return True, False
            except:
                continue
    return False, False

def output_allow(context_mod=None):
    result = {"cancel": False}
    if context_mod:
        result["contextModification"] = context_mod
    print(json.dumps(result))
    sys.exit(0)

def output_block(error_msg, context_mod=None):
    result = {"cancel": True, "errorMessage": error_msg}
    if context_mod:
        result["contextModification"] = context_mod
    print(json.dumps(result))
    sys.exit(0)

def main():
    if not ENABLED:
        output_allow()

    try:
        data = json.load(sys.stdin)
    except:
        output_allow()

    hook_name = data.get("hookName", "")
    if hook_name != "PreToolUse":
        output_allow()

    tool = data.get("toolName", "")
    params = data.get("toolParameters", {})
    workspace_roots = data.get("workspaceRoots", [])

    # Check if project is indexed
    is_indexed, is_stale = is_project_indexed(workspace_roots)
    if not is_indexed:
        output_allow()

    # Check for discovery patterns
    if tool == "list_files" or tool == "search_files":
        pattern = params.get("path", "") or params.get("regex", "")
        if is_discovery_glob(pattern) or is_discovery_grep(pattern):
            output_block(
                f"Use mcp__contextstream__search(mode=\\"hybrid\\", query=\\"{pattern}\\") instead of {tool}. "
                "ContextStream search is indexed and faster. Only use local tools if ContextStream returns 0 results.",
                "[CONTEXTSTREAM] Use ContextStream search for code discovery."
            )

    elif tool == "read_file":
        # Allow read_file by default - blocking discovery at search level is enough
        pass

    output_allow()

if __name__ == "__main__":
    main()
`;

/**
 * Cline UserPromptSubmit hook script.
 */
export const CLINE_USER_PROMPT_HOOK_SCRIPT = `#!/usr/bin/env python3
"""
ContextStream UserPromptSubmit Hook for Cline
Injects reminder about ContextStream rules on every message.
"""

import json
import sys
import os

ENABLED = os.environ.get("CONTEXTSTREAM_REMINDER_ENABLED", "true").lower() == "true"

REMINDER = """[CONTEXTSTREAM RULES]
1. BEFORE list_files/search_files/read_file: mcp__contextstream__search(mode="hybrid") FIRST
2. Call context_smart at start of EVERY response
3. Local tools ONLY if ContextStream returns 0 results
[END RULES]"""

def main():
    if not ENABLED:
        print(json.dumps({"cancel": False}))
        sys.exit(0)

    try:
        json.load(sys.stdin)
    except:
        print(json.dumps({"cancel": False}))
        sys.exit(0)

    print(json.dumps({
        "cancel": False,
        "contextModification": REMINDER
    }))
    sys.exit(0)

if __name__ == "__main__":
    main()
`;

/**
 * Cline/Roo/Kilo PostToolUse hook script for real-time file indexing.
 * This script calls the MCP server hook to index files after Edit/Write/NotebookEdit.
 */
export const CLINE_POSTTOOLUSE_HOOK_SCRIPT = `#!/bin/bash
# ContextStream PostToolUse Hook for Cline/Roo/Kilo Code
# Indexes files after Edit/Write/NotebookEdit operations for real-time search updates.
#
# The hook receives JSON on stdin with tool_name and toolParameters.
# Only runs for write operations (write_to_file, edit_file).

TOOL_NAME=$(cat | python3 -c "import sys, json; d=json.load(sys.stdin); print(d.get('toolName', d.get('tool_name', '')))" 2>/dev/null)

case "$TOOL_NAME" in
  write_to_file|edit_file|Write|Edit|NotebookEdit)
    npx @contextstream/mcp-server hook post-write
    ;;
esac

exit 0
`;

/**
 * Get the path to Cline's global hooks directory.
 */
export function getClineHooksDir(scope: "global" | "project", projectPath?: string): string {
  if (scope === "global") {
    return path.join(homedir(), "Documents", "Cline", "Rules", "Hooks");
  }
  if (!projectPath) {
    throw new Error("projectPath required for project scope");
  }
  return path.join(projectPath, ".clinerules", "hooks");
}

/**
 * Cline hook wrapper script that calls the Node.js hook via npx.
 * Cline expects executable scripts with specific names.
 */
const CLINE_HOOK_WRAPPER = (hookName: string) => `#!/bin/bash
# ContextStream ${hookName} Hook Wrapper for Cline/Roo/Kilo Code
# Calls the Node.js hook via npx
exec npx @contextstream/mcp-server hook ${hookName}
`;

/**
 * Install Cline hook scripts.
 * Cline hooks are named after the hook type (no extension).
 * Scripts are thin wrappers that call the Node.js hooks via npx.
 */
export async function installClineHookScripts(options: {
  scope: "global" | "project";
  projectPath?: string;
  includePostWrite?: boolean;
}): Promise<{ preToolUse: string; userPromptSubmit: string; postToolUse?: string }> {
  const hooksDir = getClineHooksDir(options.scope, options.projectPath);
  await fs.mkdir(hooksDir, { recursive: true });

  // Cline hooks are named after the hook type (no file extension)
  const preToolUsePath = path.join(hooksDir, "PreToolUse");
  const userPromptPath = path.join(hooksDir, "UserPromptSubmit");
  const postToolUsePath = path.join(hooksDir, "PostToolUse");

  // Write thin wrapper scripts that call Node.js hooks via npx
  await fs.writeFile(preToolUsePath, CLINE_HOOK_WRAPPER("pre-tool-use"), { mode: 0o755 });
  await fs.writeFile(userPromptPath, CLINE_HOOK_WRAPPER("user-prompt-submit"), { mode: 0o755 });

  const result: { preToolUse: string; userPromptSubmit: string; postToolUse?: string } = {
    preToolUse: preToolUsePath,
    userPromptSubmit: userPromptPath,
  };

  // Install PostToolUse hook for real-time indexing (default ON)
  if (options.includePostWrite !== false) {
    await fs.writeFile(postToolUsePath, CLINE_HOOK_WRAPPER("post-write"), { mode: 0o755 });
    result.postToolUse = postToolUsePath;
  }

  return result;
}

// =============================================================================
// ROO CODE HOOKS SUPPORT (Fork of Cline)
// =============================================================================

/**
 * Get the path to Roo Code's hooks directory.
 * Roo Code is a fork of Cline with similar hooks system.
 */
export function getRooCodeHooksDir(scope: "global" | "project", projectPath?: string): string {
  if (scope === "global") {
    // Roo Code uses ~/.roo/hooks/ for global hooks
    return path.join(homedir(), ".roo", "hooks");
  }
  if (!projectPath) {
    throw new Error("projectPath required for project scope");
  }
  return path.join(projectPath, ".roo", "hooks");
}

/**
 * Install Roo Code hook scripts.
 * Uses thin wrapper scripts that call Node.js hooks via npx.
 */
export async function installRooCodeHookScripts(options: {
  scope: "global" | "project";
  projectPath?: string;
  includePostWrite?: boolean;
}): Promise<{ preToolUse: string; userPromptSubmit: string; postToolUse?: string }> {
  const hooksDir = getRooCodeHooksDir(options.scope, options.projectPath);
  await fs.mkdir(hooksDir, { recursive: true });

  const preToolUsePath = path.join(hooksDir, "PreToolUse");
  const userPromptPath = path.join(hooksDir, "UserPromptSubmit");
  const postToolUsePath = path.join(hooksDir, "PostToolUse");

  // Write thin wrapper scripts that call Node.js hooks via npx
  await fs.writeFile(preToolUsePath, CLINE_HOOK_WRAPPER("pre-tool-use"), { mode: 0o755 });
  await fs.writeFile(userPromptPath, CLINE_HOOK_WRAPPER("user-prompt-submit"), { mode: 0o755 });

  const result: { preToolUse: string; userPromptSubmit: string; postToolUse?: string } = {
    preToolUse: preToolUsePath,
    userPromptSubmit: userPromptPath,
  };

  // Install PostToolUse hook for real-time indexing (default ON)
  if (options.includePostWrite !== false) {
    await fs.writeFile(postToolUsePath, CLINE_HOOK_WRAPPER("post-write"), { mode: 0o755 });
    result.postToolUse = postToolUsePath;
  }

  return result;
}

// =============================================================================
// KILO CODE HOOKS SUPPORT (Fork of Cline)
// =============================================================================

/**
 * Get the path to Kilo Code's hooks directory.
 * Kilo Code is a fork of Cline with similar hooks system.
 */
export function getKiloCodeHooksDir(scope: "global" | "project", projectPath?: string): string {
  if (scope === "global") {
    return path.join(homedir(), ".kilocode", "hooks");
  }
  if (!projectPath) {
    throw new Error("projectPath required for project scope");
  }
  return path.join(projectPath, ".kilocode", "hooks");
}

/**
 * Install Kilo Code hook scripts.
 * Uses thin wrapper scripts that call Node.js hooks via npx.
 */
export async function installKiloCodeHookScripts(options: {
  scope: "global" | "project";
  projectPath?: string;
  includePostWrite?: boolean;
}): Promise<{ preToolUse: string; userPromptSubmit: string; postToolUse?: string }> {
  const hooksDir = getKiloCodeHooksDir(options.scope, options.projectPath);
  await fs.mkdir(hooksDir, { recursive: true });

  const preToolUsePath = path.join(hooksDir, "PreToolUse");
  const userPromptPath = path.join(hooksDir, "UserPromptSubmit");
  const postToolUsePath = path.join(hooksDir, "PostToolUse");

  // Write thin wrapper scripts that call Node.js hooks via npx
  await fs.writeFile(preToolUsePath, CLINE_HOOK_WRAPPER("pre-tool-use"), { mode: 0o755 });
  await fs.writeFile(userPromptPath, CLINE_HOOK_WRAPPER("user-prompt-submit"), { mode: 0o755 });

  const result: { preToolUse: string; userPromptSubmit: string; postToolUse?: string } = {
    preToolUse: preToolUsePath,
    userPromptSubmit: userPromptPath,
  };

  // Install PostToolUse hook for real-time indexing (default ON)
  if (options.includePostWrite !== false) {
    await fs.writeFile(postToolUsePath, CLINE_HOOK_WRAPPER("post-write"), { mode: 0o755 });
    result.postToolUse = postToolUsePath;
  }

  return result;
}

// =============================================================================
// CURSOR HOOKS SUPPORT
// =============================================================================

/**
 * Cursor PreToolUse hook script.
 * Uses Cursor's output format: { decision: "allow" | "deny", reason?: string }
 */
export const CURSOR_PRETOOLUSE_HOOK_SCRIPT = `#!/usr/bin/env python3
"""
ContextStream PreToolUse Hook for Cursor
Blocks discovery tools and redirects to ContextStream search.

Cursor hooks use JSON output format:
{
  "decision": "allow" | "deny",
  "reason": "optional error description"
}
"""

import json
import sys
import os
from pathlib import Path
from datetime import datetime, timedelta

ENABLED = os.environ.get("CONTEXTSTREAM_HOOK_ENABLED", "true").lower() == "true"
INDEX_STATUS_FILE = Path.home() / ".contextstream" / "indexed-projects.json"
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

def is_project_indexed(workspace_roots):
    """Check if any workspace root is in an indexed project."""
    if not INDEX_STATUS_FILE.exists():
        return False, False

    try:
        with open(INDEX_STATUS_FILE, "r") as f:
            data = json.load(f)
    except:
        return False, False

    projects = data.get("projects", {})

    for workspace in workspace_roots:
        cwd_path = Path(workspace).resolve()
        for project_path, info in projects.items():
            try:
                indexed_path = Path(project_path).resolve()
                if cwd_path == indexed_path or indexed_path in cwd_path.parents:
                    indexed_at = info.get("indexed_at")
                    if indexed_at:
                        try:
                            indexed_time = datetime.fromisoformat(indexed_at.replace("Z", "+00:00"))
                            if datetime.now(indexed_time.tzinfo) - indexed_time > timedelta(days=STALE_THRESHOLD_DAYS):
                                return True, True
                        except:
                            pass
                    return True, False
            except:
                continue
    return False, False

def output_allow():
    print(json.dumps({"decision": "allow"}))
    sys.exit(0)

def output_deny(reason):
    print(json.dumps({"decision": "deny", "reason": reason}))
    sys.exit(0)

def main():
    if not ENABLED:
        output_allow()

    try:
        data = json.load(sys.stdin)
    except:
        output_allow()

    hook_name = data.get("hook_event_name", "")
    if hook_name != "preToolUse":
        output_allow()

    tool = data.get("tool_name", "")
    params = data.get("tool_input", {}) or data.get("parameters", {})
    workspace_roots = data.get("workspace_roots", [])

    # Check if project is indexed
    is_indexed, _ = is_project_indexed(workspace_roots)
    if not is_indexed:
        output_allow()

    # Check for Cursor tools
    if tool in ["Glob", "glob", "list_files"]:
        pattern = params.get("pattern", "") or params.get("path", "")
        if is_discovery_glob(pattern):
            output_deny(
                f"Use mcp__contextstream__search(mode=\\"hybrid\\", query=\\"{pattern}\\") instead of {tool}. "
                "ContextStream search is indexed and faster."
            )

    elif tool in ["Grep", "grep", "search_files", "ripgrep"]:
        pattern = params.get("pattern", "") or params.get("regex", "")
        file_path = params.get("path", "")
        if is_discovery_grep(file_path):
            output_deny(
                f"Use mcp__contextstream__search(mode=\\"keyword\\", query=\\"{pattern}\\") instead of {tool}. "
                "ContextStream search is indexed and faster."
            )

    output_allow()

if __name__ == "__main__":
    main()
`;

/**
 * Cursor BeforeSubmitPrompt hook script.
 */
export const CURSOR_BEFORE_SUBMIT_HOOK_SCRIPT = `#!/usr/bin/env python3
"""
ContextStream BeforeSubmitPrompt Hook for Cursor
Injects reminder about ContextStream rules.
"""

import json
import sys
import os

ENABLED = os.environ.get("CONTEXTSTREAM_REMINDER_ENABLED", "true").lower() == "true"

def main():
    if not ENABLED:
        print(json.dumps({"continue": True}))
        sys.exit(0)

    try:
        json.load(sys.stdin)
    except:
        print(json.dumps({"continue": True}))
        sys.exit(0)

    print(json.dumps({
        "continue": True,
        "user_message": "[CONTEXTSTREAM] Search with mcp__contextstream__search before using Glob/Grep/Read"
    }))
    sys.exit(0)

if __name__ == "__main__":
    main()
`;

/**
 * Cursor hooks.json configuration structure.
 */
export interface CursorHooksConfig {
  version: number;
  hooks: {
    preToolUse?: Array<{
      command: string;
      type?: "command";
      timeout?: number;
      matcher?: { tool_name?: string };
    }>;
    beforeSubmitPrompt?: Array<{
      command: string;
      type?: "command";
      timeout?: number;
    }>;
    [key: string]: unknown;
  };
}

/**
 * Get the path to Cursor's hooks configuration file.
 */
export function getCursorHooksConfigPath(scope: "global" | "project", projectPath?: string): string {
  if (scope === "global") {
    return path.join(homedir(), ".cursor", "hooks.json");
  }
  if (!projectPath) {
    throw new Error("projectPath required for project scope");
  }
  return path.join(projectPath, ".cursor", "hooks.json");
}

/**
 * Get the path to Cursor's hooks scripts directory.
 */
export function getCursorHooksDir(scope: "global" | "project", projectPath?: string): string {
  if (scope === "global") {
    return path.join(homedir(), ".cursor", "hooks");
  }
  if (!projectPath) {
    throw new Error("projectPath required for project scope");
  }
  return path.join(projectPath, ".cursor", "hooks");
}

/**
 * Read existing Cursor hooks config.
 */
export async function readCursorHooksConfig(
  scope: "global" | "project",
  projectPath?: string
): Promise<CursorHooksConfig> {
  const configPath = getCursorHooksConfigPath(scope, projectPath);
  try {
    const content = await fs.readFile(configPath, "utf-8");
    return JSON.parse(content);
  } catch {
    return { version: 1, hooks: {} };
  }
}

/**
 * Write Cursor hooks config.
 */
export async function writeCursorHooksConfig(
  config: CursorHooksConfig,
  scope: "global" | "project",
  projectPath?: string
): Promise<void> {
  const configPath = getCursorHooksConfigPath(scope, projectPath);
  const dir = path.dirname(configPath);
  await fs.mkdir(dir, { recursive: true });
  await fs.writeFile(configPath, JSON.stringify(config, null, 2));
}

/**
 * Install Cursor hook scripts and update hooks.json config.
 * Cursor hooks now use Node.js via npx - no Python files needed.
 */
export async function installCursorHookScripts(options: {
  scope: "global" | "project";
  projectPath?: string;
}): Promise<{ preToolUse: string; beforeSubmitPrompt: string; config: string }> {
  // Ensure hooks directory exists
  const hooksDir = getCursorHooksDir(options.scope, options.projectPath);
  await fs.mkdir(hooksDir, { recursive: true });

  // Update hooks.json config to use npx commands directly
  const existingConfig = await readCursorHooksConfig(options.scope, options.projectPath);

  // Remove any existing ContextStream hooks
  const filterContextStreamHooks = (hooks: unknown[] | undefined): unknown[] => {
    if (!hooks) return [];
    return hooks.filter((h) => {
      const hook = h as { command?: string };
      return !hook.command?.includes("contextstream");
    });
  };

  const filteredPreToolUse = filterContextStreamHooks(existingConfig.hooks.preToolUse) as Array<{
    command: string;
    type?: "command";
    timeout?: number;
    matcher?: { tool_name?: string };
  }>;
  const filteredBeforeSubmit = filterContextStreamHooks(existingConfig.hooks.beforeSubmitPrompt) as Array<{
    command: string;
    type?: "command";
    timeout?: number;
  }>;

  const config: CursorHooksConfig = {
    version: 1,
    hooks: {
      ...existingConfig.hooks,
      preToolUse: [
        ...filteredPreToolUse,
        {
          command: "npx @contextstream/mcp-server hook pre-tool-use",
          type: "command" as const,
          timeout: 5,
          matcher: { tool_name: "Glob|Grep|search_files|list_files|ripgrep" },
        },
      ],
      beforeSubmitPrompt: [
        ...filteredBeforeSubmit,
        {
          command: "npx @contextstream/mcp-server hook user-prompt-submit",
          type: "command" as const,
          timeout: 5,
        },
      ],
    },
  };

  await writeCursorHooksConfig(config, options.scope, options.projectPath);
  const configPath = getCursorHooksConfigPath(options.scope, options.projectPath);

  return {
    preToolUse: "npx @contextstream/mcp-server hook pre-tool-use",
    beforeSubmitPrompt: "npx @contextstream/mcp-server hook user-prompt-submit",
    config: configPath,
  };
}

// =============================================================================
// UNIFIED EDITOR HOOKS INSTALLATION
// =============================================================================

export type SupportedEditor = "claude" | "cline" | "roo" | "kilo" | "cursor";

export interface EditorHooksResult {
  editor: SupportedEditor;
  installed: string[];
  hooksDir: string;
}

/**
 * Install hooks for a specific editor.
 */
export async function installEditorHooks(options: {
  editor: SupportedEditor;
  scope: "global" | "project";
  projectPath?: string;
  includePreCompact?: boolean;
  includePostWrite?: boolean;
}): Promise<EditorHooksResult> {
  const { editor, scope, projectPath, includePreCompact, includePostWrite } = options;

  switch (editor) {
    case "claude": {
      if (scope === "project" && !projectPath) {
        throw new Error("projectPath required for project scope");
      }
      const scripts = await installHookScripts({ includePreCompact });
      const hooksConfig = buildHooksConfig({ includePreCompact, includePostWrite });

      // Update Claude Code settings
      const settingsScope = scope === "global" ? "user" : "project";
      const existing = await readClaudeSettings(settingsScope, projectPath);
      const merged = mergeHooksIntoSettings(existing, hooksConfig);
      await writeClaudeSettings(merged, settingsScope, projectPath);

      const installed = [scripts.preToolUse, scripts.userPrompt];
      if (scripts.preCompact) installed.push(scripts.preCompact);

      return {
        editor: "claude",
        installed,
        hooksDir: getHooksDir(),
      };
    }

    case "cline": {
      const scripts = await installClineHookScripts({ scope, projectPath, includePostWrite });
      const installed = [scripts.preToolUse, scripts.userPromptSubmit];
      if (scripts.postToolUse) installed.push(scripts.postToolUse);
      return {
        editor: "cline",
        installed,
        hooksDir: getClineHooksDir(scope, projectPath),
      };
    }

    case "roo": {
      const scripts = await installRooCodeHookScripts({ scope, projectPath, includePostWrite });
      const installed = [scripts.preToolUse, scripts.userPromptSubmit];
      if (scripts.postToolUse) installed.push(scripts.postToolUse);
      return {
        editor: "roo",
        installed,
        hooksDir: getRooCodeHooksDir(scope, projectPath),
      };
    }

    case "kilo": {
      const scripts = await installKiloCodeHookScripts({ scope, projectPath, includePostWrite });
      const installed = [scripts.preToolUse, scripts.userPromptSubmit];
      if (scripts.postToolUse) installed.push(scripts.postToolUse);
      return {
        editor: "kilo",
        installed,
        hooksDir: getKiloCodeHooksDir(scope, projectPath),
      };
    }

    case "cursor": {
      const scripts = await installCursorHookScripts({ scope, projectPath });
      return {
        editor: "cursor",
        installed: [scripts.preToolUse, scripts.beforeSubmitPrompt],
        hooksDir: getCursorHooksDir(scope, projectPath),
      };
    }

    default:
      throw new Error(`Unsupported editor: ${editor}`);
  }
}

/**
 * Install hooks for all supported editors.
 */
export async function installAllEditorHooks(options: {
  scope: "global" | "project";
  projectPath?: string;
  includePreCompact?: boolean;
  includePostWrite?: boolean;
  editors?: SupportedEditor[];
}): Promise<EditorHooksResult[]> {
  const editors = options.editors || ["claude", "cline", "roo", "kilo", "cursor"];
  const results: EditorHooksResult[] = [];

  for (const editor of editors) {
    try {
      const result = await installEditorHooks({
        editor,
        scope: options.scope,
        projectPath: options.projectPath,
        includePreCompact: options.includePreCompact,
        includePostWrite: options.includePostWrite,
      });
      results.push(result);
    } catch (error) {
      // Log but continue with other editors
      console.error(`Failed to install hooks for ${editor}:`, error);
    }
  }

  return results;
}

/**
 * Generate documentation for all editor hooks.
 */
export function generateAllHooksDocumentation(): string {
  return `
## Editor Hooks Support (ContextStream)

ContextStream can install hooks for multiple AI code editors to enforce ContextStream-first behavior.

### Supported Editors

| Editor | Hooks Location | Hook Types |
|--------|---------------|------------|
| **Claude Code** | \`~/.claude/hooks/\` | PreToolUse, UserPromptSubmit, PreCompact |
| **Cursor** | \`~/.cursor/hooks/\` | preToolUse, beforeSubmit |
| **Cline** | \`~/Documents/Cline/Rules/Hooks/\` | PreToolUse, UserPromptSubmit |
| **Roo Code** | \`~/.roo/hooks/\` | PreToolUse, UserPromptSubmit |
| **Kilo Code** | \`~/.kilocode/hooks/\` | PreToolUse, UserPromptSubmit |

### Claude Code Hooks

${generateHooksDocumentation()}

### Cursor Hooks

Cursor uses a \`hooks.json\` configuration file:
- **preToolUse**: Blocks discovery tools before execution
- **beforeSubmitPrompt**: Injects ContextStream rules reminder

#### Output Format
\`\`\`json
{ "decision": "allow" }
\`\`\`
or
\`\`\`json
{ "decision": "deny", "reason": "Use ContextStream search instead" }
\`\`\`

### Cline/Roo/Kilo Code Hooks

These editors use the same hook format (JSON output):
- **PreToolUse**: Blocks discovery tools, redirects to ContextStream search
- **UserPromptSubmit**: Injects ContextStream rules reminder

Hooks are executable scripts named after the hook type (no extension).

#### Output Format
\`\`\`json
{
  "cancel": true,
  "errorMessage": "Use ContextStream search instead",
  "contextModification": "[CONTEXTSTREAM] Use search tool first"
}
\`\`\`

### Installation

Use \`generate_rules(install_hooks=true, editors=["claude", "cursor", "cline", "roo", "kilo"])\` to install hooks for specific editors, or omit \`editors\` to install for all.

### Disabling Hooks

Set environment variables:
- \`CONTEXTSTREAM_HOOK_ENABLED=false\` - Disable PreToolUse blocking
- \`CONTEXTSTREAM_REMINDER_ENABLED=false\` - Disable UserPromptSubmit reminders
`.trim();
}
