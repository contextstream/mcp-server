#!/usr/bin/env python3
"""ContextStream search adoption from local agent transcripts.

Answers one question per agent: when an agent searches code, does it use
ContextStream search or shell `rg`/`grep`/`find`? Run it before and after a
change that is meant to move that number and compare the two JSON files.

Reads Claude Code (`~/.claude/projects/**/*.jsonl`) and Codex
(`~/.codex/sessions/**/*.jsonl`) transcripts; nothing leaves the machine and no
transcript text is printed or stored, only counts.

The shell classifier is a port of `detect_bash_code_search` in
`crates/mcp-server/src/hook_handlers/pre_tool_use.rs`, so "shell code search"
here means exactly what the PreToolUse hook would nudge. Keep the two in step.

The Claude Code reader is validated against real transcripts. The Codex reader
accepts the `function_call` / `local_shell_call` shapes seen in recent rollouts
but has not been checked against every Codex release; treat its counts as a
floor and rerun if a Codex update changes the transcript format.
"""
import argparse
import json
import re
import sys
import time
from collections import Counter
from pathlib import Path

CONTEXTSTREAM_PREFIX = "mcp__contextstream__"
NATIVE_SEARCH_TOOLS = {"Grep", "Glob"}
FIND_NAME_FLAGS = (" -name ", " -iname ", " -path ", " -ipath ", " -regex ", " -iregex ")
CONCRETE_FILE_BLOCKERS = set("*?[]()|\\$^")


def _recursive_flag(head):
    for token in head.split():
        if token == "--recursive":
            return True
        if (
            token.startswith("-")
            and not token.startswith("--")
            and len(token) > 1
            and token[1:].isalpha()
            and any(c in "rR" for c in token[1:])
        ):
            return True
    return False


def _log_or_text_target(token):
    lower = token.strip("'\"").lower()
    return lower.endswith((".log", ".txt")) or lower.startswith("/var/log")


def _concrete_file_target(token):
    t = token.strip("'\"")
    if not t or t.endswith("/") or any(c in CONCRETE_FILE_BLOCKERS for c in t):
        return False
    last = t.rsplit("/", 1)[-1]
    dot = last.rfind(".")
    return 0 < dot < len(last) - 1


def shell_code_search(command):
    """Return the search tool name when `command` is shell code discovery."""
    head = (command or "").strip()
    if not head:
        return None
    while True:
        stripped = head.lstrip()
        if stripped.startswith("cd "):
            rest = stripped[3:]
            for separator in ("&&", ";"):
                before, found, after = rest.partition(separator)
                if found:
                    head = after.lstrip()
                    break
            else:
                break
            continue
        break
    # Anything piped is filtering, not discovery.
    if "|" in head:
        return None
    tokens = head.split()
    if not tokens:
        return None
    tool, needs_find_flag = {
        "grep": ("grep", False),
        "egrep": ("grep", False),
        "fgrep": ("grep", False),
        "rg": ("rg", False),
        "ripgrep": ("rg", False),
        "ag": ("ag", False),
        "find": ("find", True),
        "fd": ("fd", False),
        "fdfind": ("fd", False),
    }.get(tokens[0], (None, False))
    if tool is None:
        return None
    if needs_find_flag and not any(flag in head for flag in FIND_NAME_FLAGS):
        return None
    recursive = _recursive_flag(head)
    for token in tokens[1:]:
        if token.startswith("-"):
            continue
        if _log_or_text_target(token):
            return None
        if not recursive and tool in ("grep", "rg", "ag") and _concrete_file_target(token):
            return None
    return tool


def _command_text(arguments):
    """The shell command in a tool-call argument object, if any."""
    if isinstance(arguments, str):
        try:
            arguments = json.loads(arguments)
        except ValueError:
            return None
    if not isinstance(arguments, dict):
        return None
    command = arguments.get("command", arguments.get("cmd"))
    if isinstance(command, list):
        # ["bash", "-lc", "rg foo"]: the script is the last element.
        command = command[-1] if command else None
    return command if isinstance(command, str) else None


def _contextstream_tool(name):
    """`search` for `mcp__contextstream__search`, `contextstream__search`, ..."""
    lowered = (name or "").lower()
    if "contextstream" not in lowered:
        return None
    return lowered.rsplit("__", 1)[-1] or None


def records(path):
    """JSON objects of a JSONL transcript, skipping lines that are not JSON."""
    with path.open(errors="replace") as handle:
        for line in handle:
            try:
                record = json.loads(line)
            except ValueError:
                continue
            if isinstance(record, dict):
                yield record


def claude_calls(path):
    """Yield (kind, detail) for each tool call in a Claude Code transcript."""
    for record in records(path):
        if record.get("type") != "assistant":
            continue
        content = (record.get("message") or {}).get("content")
        if not isinstance(content, list):
            continue
        for block in content:
            if not isinstance(block, dict) or block.get("type") != "tool_use":
                continue
            name = block.get("name") or ""
            arguments = block.get("input")
            if name.startswith(CONTEXTSTREAM_PREFIX):
                yield "contextstream", name[len(CONTEXTSTREAM_PREFIX):]
            elif name in NATIVE_SEARCH_TOOLS:
                yield "native_search", name
            elif name == "Bash":
                tool = shell_code_search(_command_text(arguments))
                if tool:
                    yield "shell_search", tool


# Codex code mode calls tools from inside an `exec` cell: tools.mcp__<server>__<tool>(...).
EXEC_TOOL_CALL = re.compile(r"tools\.mcp__([A-Za-z0-9_]*?)__([A-Za-z0-9_]+)\s*\(")


def codex_calls(path):
    """Yield (kind, detail) for each tool call in a Codex rollout."""
    for record in records(path):
        payload = record.get("payload") if isinstance(record.get("payload"), dict) else record
        if payload.get("type") not in ("function_call", "local_shell_call", "custom_tool_call"):
            continue
        name = payload.get("name") or ""
        arguments = payload.get("arguments", payload.get("action", payload.get("input")))
        tool = _contextstream_tool(name)
        if tool:
            yield "contextstream", tool
            continue
        if name == "exec" and isinstance(arguments, str):
            for server, exec_tool in EXEC_TOOL_CALL.findall(arguments):
                if "contextstream" in server.lower():
                    yield "contextstream", exec_tool.lower()
            continue
        command = _command_text(arguments)
        search = shell_code_search(command)
        if search:
            yield "shell_search", search


READERS = {"claude-code": claude_calls, "codex": codex_calls}


def transcripts(root, since):
    root = Path(root).expanduser()
    if not root.is_dir():
        return
    for path in root.rglob("*.jsonl"):
        try:
            if path.stat().st_mtime >= since:
                yield path
        except OSError:
            continue


def measure(agent, root, since):
    reader = READERS[agent]
    sessions = with_cs_search = 0
    totals = Counter()
    cs_tools = Counter()
    shell_tools = Counter()
    for path in transcripts(root, since):
        sessions += 1
        used_search = False
        for kind, detail in reader(path):
            totals[kind] += 1
            if kind == "contextstream":
                cs_tools[detail] += 1
                used_search |= detail == "search"
            elif kind == "shell_search":
                shell_tools[detail] += 1
        with_cs_search += used_search
    cs_search = cs_tools["search"]
    searches = cs_search + totals["shell_search"] + totals["native_search"]
    return {
        "sessions": sessions,
        "sessions_with_contextstream_search": with_cs_search,
        "contextstream_search": cs_search,
        "contextstream_other": totals["contextstream"] - cs_search,
        "shell_search": totals["shell_search"],
        "native_search": totals["native_search"],
        "contextstream_search_share": round(cs_search / searches, 4) if searches else None,
        "contextstream_tools": dict(sorted(cs_tools.items())),
        "shell_search_tools": dict(sorted(shell_tools.items())),
    }


def render(report):
    columns = (
        ("agent", None),
        ("sessions", "sessions"),
        ("w/ cs search", "sessions_with_contextstream_search"),
        ("cs search", "contextstream_search"),
        ("shell search", "shell_search"),
        ("native search", "native_search"),
        ("cs share", "contextstream_search_share"),
    )
    rows = [[title for title, _ in columns]]
    for agent, stats in report["agents"].items():
        row = [agent]
        for _, key in columns[1:]:
            value = stats[key]
            row.append("n/a" if value is None else f"{value:.1%}" if key.endswith("share") else str(value))
        rows.append(row)
    widths = [max(len(row[i]) for row in rows) for i in range(len(columns))]
    lines = [f"window: last {report['days']} days"]
    for row in rows:
        lines.append("  ".join(cell.ljust(widths[i]) if i == 0 else cell.rjust(widths[i]) for i, cell in enumerate(row)))
    return "\n".join(lines)


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    parser.add_argument("--days", type=float, default=14, help="look back this many days (default 14)")
    parser.add_argument("--claude-dir", default="~/.claude/projects")
    parser.add_argument("--codex-dir", default="~/.codex/sessions")
    parser.add_argument("--json", metavar="FILE", help="also write the report as JSON for before/after comparison")
    args = parser.parse_args(argv)

    since = time.time() - args.days * 86400
    report = {
        "days": args.days,
        "generated_at": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime()),
        "agents": {
            "claude-code": measure("claude-code", args.claude_dir, since),
            "codex": measure("codex", args.codex_dir, since),
        },
    }
    print(render(report))
    if args.json:
        Path(args.json).write_text(json.dumps(report, indent=2, sort_keys=True) + "\n")
    return 0


if __name__ == "__main__":
    sys.exit(main())
