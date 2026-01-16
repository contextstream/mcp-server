#!/usr/bin/env python3
"""
ContextStream UserPromptSubmit Hook for Claude Code

This hook injects a reminder about using ContextStream search before every user prompt.
This ensures the instruction stays in recent context and doesn't decay over long conversations.

Install this hook via Claude Code settings:
{
  "hooks": {
    "UserPromptSubmit": [
      {
        "matcher": "*",
        "hooks": [{
          "type": "command",
          "command": "python3 ~/.claude/hooks/contextstream-reminder.py"
        }]
      }
    ]
  }
}
"""

import json
import sys
import os

# Configuration
REMINDER_ENABLED = os.environ.get("CONTEXTSTREAM_REMINDER_ENABLED", "true").lower() == "true"

# The reminder message that gets injected into context
REMINDER = """
[CONTEXTSTREAM RULES - ALWAYS FOLLOW]
1. BEFORE Glob/Grep/Read/Search: Call mcp__contextstream__search(mode="hybrid", query="...") FIRST
2. Call context_smart at start of EVERY response
3. Only use local tools if ContextStream returns 0 results
[END RULES]
""".strip()


def main():
    if not REMINDER_ENABLED:
        sys.exit(0)

    try:
        input_data = json.load(sys.stdin)
    except json.JSONDecodeError:
        sys.exit(0)

    # Output the additional context to inject
    output = {
        "hookSpecificOutput": {
            "hookEventName": "UserPromptSubmit",
            "additionalContext": REMINDER
        }
    }

    print(json.dumps(output))
    sys.exit(0)


if __name__ == "__main__":
    main()
