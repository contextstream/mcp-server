#!/usr/bin/env python3
"""
ContextStream PreToolUse Hook for Claude Code

This hook intercepts Grep, Glob, Read (for discovery), and Search tool calls
and blocks them with a message instructing Claude to use ContextStream search instead.

The hook only blocks tools when they appear to be used for code discovery/exploration.
It allows Read when there's a specific file path that was likely found via search first.

Install this hook via Claude Code settings:
{
  "hooks": {
    "PreToolUse": [
      {
        "matcher": "Grep|Glob|Read|Search",
        "hooks": [{
          "type": "command",
          "command": "python3 ~/.claude/hooks/contextstream-redirect.py"
        }]
      }
    ]
  }
}
"""

import json
import sys
import os

# Configuration: Set to False to disable blocking (useful for debugging)
BLOCKING_ENABLED = os.environ.get("CONTEXTSTREAM_HOOK_ENABLED", "true").lower() == "true"

# File extensions that indicate code/source files (for discovery detection)
CODE_EXTENSIONS = {
    ".ts", ".tsx", ".js", ".jsx", ".mjs", ".cjs",
    ".py", ".pyi",
    ".rs",
    ".go",
    ".java", ".kt", ".scala",
    ".c", ".cpp", ".cc", ".h", ".hpp",
    ".rb",
    ".php",
    ".swift",
    ".cs",
    ".vue", ".svelte",
    ".sql",
    ".sh", ".bash", ".zsh",
    ".yaml", ".yml", ".json", ".toml",
    ".md", ".mdx", ".txt",
    ".html", ".css", ".scss", ".less",
}

# Glob patterns that indicate exploration/discovery (should use ContextStream)
DISCOVERY_GLOB_PATTERNS = [
    "**/*",
    "**/",
    "src/**",
    "lib/**",
    "app/**",
    "components/**",
    "pages/**",
    "api/**",
    "services/**",
    "utils/**",
    "helpers/**",
    "hooks/**",
    "types/**",
    "models/**",
    "controllers/**",
    "routes/**",
    "tests/**",
    "__tests__/**",
    "spec/**",
]


def is_discovery_glob(pattern: str) -> bool:
    """Check if a glob pattern indicates code discovery/exploration."""
    pattern_lower = pattern.lower()

    # Check for broad discovery patterns
    for discovery_pattern in DISCOVERY_GLOB_PATTERNS:
        if discovery_pattern in pattern_lower:
            return True

    # Check for patterns that search all files of a type
    if pattern_lower.startswith("**/*.") or pattern_lower.startswith("**/"):
        return True

    # Check for patterns with wildcards in directory part
    if "**" in pattern or "*/" in pattern:
        return True

    return False


def is_discovery_grep(pattern: str, file_path: str | None) -> bool:
    """Check if a grep search indicates code discovery/exploration."""
    # If searching a broad path or no specific file, it's discovery
    if not file_path:
        return True

    if file_path in [".", "./", "*", "**"]:
        return True

    # If the path contains wildcards, it's discovery
    if "*" in file_path or "**" in file_path:
        return True

    return False


def is_discovery_read(file_path: str) -> bool:
    """
    Check if a Read call is for discovery vs. targeted editing.

    We allow Read when:
    - The path is very specific (likely from a previous search result)
    - It's a config file that needs to be read entirely
    - It's a single known file

    We block Read when:
    - Multiple files are being read in sequence without prior search
    - The path looks like it's guessing/exploring
    """
    if not file_path:
        return True

    # Allow reading specific config/known files
    known_files = [
        "package.json", "tsconfig.json", "pyproject.toml", "Cargo.toml",
        "go.mod", "pom.xml", "build.gradle", "requirements.txt",
        ".env", ".env.local", ".env.example",
        "README.md", "CLAUDE.md", "AGENTS.md",
        "docker-compose.yml", "Dockerfile",
        ".gitignore", ".eslintrc", ".prettierrc",
    ]

    file_name = os.path.basename(file_path)
    if file_name.lower() in [f.lower() for f in known_files]:
        return False  # Allow reading known config files

    # Allow if it's a very specific path with line numbers or exact file
    # This indicates it came from a search result
    if ":" in file_path:  # Line number specified
        return False

    # If the path is specific and absolute, allow it
    if file_path.startswith("/") and not any(c in file_path for c in ["*", "?"]):
        return False

    return False  # Default to allowing Read for now (be less aggressive)


def is_discovery_search(tool_input: dict) -> bool:
    """Check if a Search (Task with Explore) is being used."""
    # If using Task tool with Explore subagent, it's discovery
    subagent_type = tool_input.get("subagent_type", "")
    if subagent_type.lower() == "explore":
        return True
    return False


def main():
    if not BLOCKING_ENABLED:
        sys.exit(0)  # Hook disabled, allow all

    try:
        input_data = json.load(sys.stdin)
    except json.JSONDecodeError:
        sys.exit(0)  # Can't parse input, allow

    tool_name = input_data.get("tool_name", "")
    tool_input = input_data.get("tool_input", {})

    # Handle different tools
    if tool_name == "Glob":
        pattern = tool_input.get("pattern", "")
        if is_discovery_glob(pattern):
            print(
                f"STOP: Use mcp__contextstream__search(mode=\"hybrid\", query=\"{pattern}\") "
                f"instead of Glob. ContextStream search is faster and provides semantic matching. "
                f"Only use Glob if ContextStream returns 0 results.",
                file=sys.stderr
            )
            sys.exit(2)  # Block

    elif tool_name == "Grep":
        pattern = tool_input.get("pattern", "")
        file_path = tool_input.get("path", "")
        if is_discovery_grep(pattern, file_path):
            print(
                f"STOP: Use mcp__contextstream__search(mode=\"keyword\", query=\"{pattern}\") "
                f"instead of Grep. ContextStream search is indexed and faster. "
                f"Only use Grep if ContextStream returns 0 results.",
                file=sys.stderr
            )
            sys.exit(2)  # Block

    elif tool_name == "Search":
        # The Search tool in Claude Code is for web search, which is fine
        # But we want to catch if it's being used for code search somehow
        pass  # Allow web search

    elif tool_name == "Task":
        subagent_type = tool_input.get("subagent_type", "").lower()
        if subagent_type == "explore":
            print(
                "STOP: Use mcp__contextstream__search(mode=\"hybrid\", query=\"...\") "
                "instead of Task with Explore subagent. ContextStream search provides "
                "indexed semantic code search. Only use Explore if ContextStream returns 0 results.",
                file=sys.stderr
            )
            sys.exit(2)  # Block
        elif subagent_type == "plan":
            print(
                "STOP: Use mcp__contextstream__session(action=\"capture_plan\", title=\"...\", steps=[...]) "
                "instead of Task with Plan subagent. ContextStream plans persist across sessions.",
                file=sys.stderr
            )
            sys.exit(2)  # Block

    elif tool_name == "Read":
        file_path = tool_input.get("file_path", "")
        # Be less aggressive with Read - only block obvious discovery patterns
        # The real protection is blocking Glob/Grep that find the files
        pass  # Allow Read by default

    elif tool_name == "EnterPlanMode":
        print(
            "STOP: Use mcp__contextstream__session(action=\"capture_plan\", title=\"...\", steps=[...]) "
            "instead of EnterPlanMode. ContextStream plans persist across sessions and are searchable.",
            file=sys.stderr
        )
        sys.exit(2)  # Block

    # Default: allow the tool
    sys.exit(0)


if __name__ == "__main__":
    main()
