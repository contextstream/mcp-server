#!/usr/bin/env python3
"""
ContextStream PreToolUse Hook for Claude Code

This hook intercepts Grep, Glob, Read (for discovery), and Search tool calls
and blocks them with a message instructing Claude to use ContextStream search instead.

The hook only blocks tools when they appear to be used for code discovery/exploration.
It allows Read when there's a specific file path that was likely found via search first.

IMPORTANT: Files/directories matching .contextstream/ignore patterns are ALLOWED through
because they won't be indexed in ContextStream anyway.

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
import fnmatch
from pathlib import Path
from typing import Optional, List, Dict, Any

# Configuration: Set to False to disable blocking (useful for debugging)
BLOCKING_ENABLED = os.environ.get("CONTEXTSTREAM_HOOK_ENABLED", "true").lower() == "true"

# Path to the indexed projects status file (matches hooks-config.ts)
INDEX_STATUS_PATH = Path.home() / ".contextstream" / "indexed-projects.json"

# Default ignore patterns (same as mcp-server/src/ignore.ts)
DEFAULT_IGNORE_PATTERNS = [
    # Version control
    ".git/", ".svn/", ".hg/",
    # Package managers / dependencies
    "node_modules/", "vendor/", ".pnpm/",
    # Build outputs
    "target/", "dist/", "build/", "out/", ".next/", ".nuxt/",
    # Python
    "__pycache__/", ".pytest_cache/", ".mypy_cache/", "venv/", ".venv/", "env/", ".env/",
    # IDE
    ".idea/", ".vscode/", ".vs/",
    # Coverage
    "coverage/", ".coverage/",
    # Lock files
    "package-lock.json", "yarn.lock", "pnpm-lock.yaml", "Cargo.lock",
    "poetry.lock", "Gemfile.lock", "composer.lock",
    # OS files
    ".DS_Store", "Thumbs.db",
]


def read_indexed_projects() -> Dict[str, Any]:
    """Read the indexed projects status file."""
    try:
        if INDEX_STATUS_PATH.exists():
            content = INDEX_STATUS_PATH.read_text()
            return json.loads(content).get("projects", {})
    except (json.JSONDecodeError, IOError, OSError):
        pass
    return {}


def is_project_indexed(project_root: str) -> bool:
    """Check if a project is registered as indexed in ContextStream.

    Returns True only if the project has been indexed via session_init.
    This prevents blocking tools for projects that haven't been indexed yet.
    """
    if not project_root:
        return False

    indexed_projects = read_indexed_projects()
    resolved_path = str(Path(project_root).resolve())

    # Check if this exact path is indexed
    if resolved_path in indexed_projects:
        return True

    # Also check with trailing slash normalization
    normalized = resolved_path.rstrip("/\\")
    for indexed_path in indexed_projects:
        if indexed_path.rstrip("/\\") == normalized:
            return True

    return False


def find_project_root(start_path: str) -> Optional[str]:
    """Find project root by looking for .contextstream directory or .git"""
    current = Path(start_path).resolve()

    # If start_path is a file, start from its parent
    if current.is_file():
        current = current.parent

    while current != current.parent:
        # Check for .contextstream directory (our marker)
        if (current / ".contextstream").is_dir():
            return str(current)
        # Fall back to .git
        if (current / ".git").exists():
            return str(current)
        current = current.parent

    return None


def load_ignore_patterns(project_root: str) -> List[str]:
    """Load ignore patterns from .contextstream/ignore file"""
    patterns = list(DEFAULT_IGNORE_PATTERNS)

    ignore_file = Path(project_root) / ".contextstream" / "ignore"
    if ignore_file.exists():
        try:
            content = ignore_file.read_text()
            for line in content.splitlines():
                line = line.strip()
                # Skip empty lines and comments
                if line and not line.startswith("#"):
                    patterns.append(line)
        except Exception:
            pass  # If we can't read the file, just use defaults

    return patterns


def is_path_ignored(path: str, patterns: List[str], project_root: str) -> bool:
    """Check if a path matches any ignore pattern using gitignore-like matching"""
    # Make path relative to project root
    try:
        abs_path = Path(path).resolve()
        rel_path = abs_path.relative_to(project_root)
        path_str = str(rel_path)
    except (ValueError, TypeError):
        # Path is not under project root, or invalid
        path_str = path

    # Normalize path separators
    path_str = path_str.replace("\\", "/")

    for pattern in patterns:
        pattern = pattern.replace("\\", "/")

        # Handle directory patterns (ending with /)
        if pattern.endswith("/"):
            dir_pattern = pattern.rstrip("/")
            # Check if any component of the path matches
            parts = path_str.split("/")
            if dir_pattern in parts:
                return True
            # Also check if path starts with pattern
            if path_str.startswith(dir_pattern + "/"):
                return True
        else:
            # File pattern - check exact match or fnmatch
            if fnmatch.fnmatch(path_str, pattern):
                return True
            if fnmatch.fnmatch(os.path.basename(path_str), pattern):
                return True
            # Check with ** for recursive matching
            if "**" in pattern:
                # Convert gitignore ** to fnmatch pattern
                fn_pattern = pattern.replace("**", "*")
                if fnmatch.fnmatch(path_str, fn_pattern):
                    return True

    return False

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


def get_target_path(tool_name: str, tool_input: dict) -> Optional[str]:
    """Extract the target path from a tool invocation"""
    if tool_name == "Glob":
        # Glob uses pattern, but we need the path being searched
        return tool_input.get("path", os.getcwd())
    elif tool_name == "Grep":
        return tool_input.get("path", os.getcwd())
    elif tool_name == "Read":
        return tool_input.get("file_path", "")
    return None


def main():
    if not BLOCKING_ENABLED:
        sys.exit(0)  # Hook disabled, allow all

    try:
        input_data = json.load(sys.stdin)
    except json.JSONDecodeError:
        sys.exit(0)  # Can't parse input, allow

    tool_name = input_data.get("tool_name", "")
    tool_input = input_data.get("tool_input", {})

    # Check if the target path is in an indexed project
    # Only block tools if the project is actually indexed in ContextStream
    target_path = get_target_path(tool_name, tool_input)
    project_root = find_project_root(target_path) if target_path else None

    # If no project root found, allow local tools (not in any project)
    if not project_root:
        sys.exit(0)

    # If project is NOT indexed in ContextStream, allow local tools
    # This prevents blocking for projects that haven't been set up yet
    if not is_project_indexed(project_root):
        sys.exit(0)

    # Check if the target path is in an ignored location
    # If so, allow the local tool (don't redirect to ContextStream)
    patterns = load_ignore_patterns(project_root)
    if target_path and is_path_ignored(target_path, patterns, project_root):
        # Path is ignored - allow local tool since it won't be in ContextStream
        sys.exit(0)

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
