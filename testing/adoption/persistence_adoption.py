#!/usr/bin/env python3
"""Where agents keep durable knowledge: ContextStream or local markdown files.

Answers one question per agent and ISO week: of the times an agent saved a
memory, preference, lesson, decision, plan, runbook, handoff, ticket or todo,
how many went to ContextStream and how many into a local markdown file
(Claude auto memory, ~/.claude/plans, HANDOFF.md, notes, runbooks)? Run it
before and after a change that is meant to move that share.

Reads Claude Code (`~/.claude/projects/**/*.jsonl`) and Codex
(`~/.codex/sessions/**/*.jsonl`) transcripts, including Codex code mode, where
tools are called from inside an `exec` cell. Nothing leaves the machine and no
transcript text is printed or stored, only counts.

The path classifier mirrors `crates/mcp-server/src/hook_handlers/durable_paths.rs`
(repository docs) plus the Claude plan and auto memory locations; keep the two
in step.
"""

import argparse
import json
import re
import sys
import time
from collections import Counter, defaultdict
from datetime import datetime, timezone
from pathlib import Path

WRITE_TOOLS = {"Write", "Edit", "MultiEdit", "NotebookEdit"}
NON_SOURCE_SEGMENTS = {"node_modules", ".git", "target", "vendor", "scratchpad", "dist", "build"}
HANDOFF_NAMES = {"handoff.md", "handoff.txt", "agent-handoff.md", "agent_handoff.md", ".handoff.md"}
PATCH_PATH = re.compile(r"^\*\*\* (?:Add|Update) File: (.+?)\s*$", re.M)
EXEC_CS_CALL = re.compile(r"tools\.mcp__([A-Za-z0-9_]*?)__([A-Za-z0-9_]+)\s*\(\s*(\{[^{}]*)?", re.S)
EXEC_FIELD = re.compile(r"""\b(action|event_type|kind)\s*:\s*["']([A-Za-z_]+)["']""")

# (tool, action) -> kind for ContextStream persistence calls. Alias tools
# (memory_create_doc, session_capture_lesson, ...) carry their action in the name.
PERSIST = {
    ("session", "capture_lesson"): "lesson",
    ("session_capture_lesson", ""): "lesson",
    ("session", "remember"): "preference",
    ("session_remember", ""): "preference",
    ("session", "capture_plan"): "plan",
    ("capture_plan", ""): "plan",
    ("session", "capture"): "capture",
    ("session_capture", ""): "capture",
    ("memory", "create_decision"): "decision",
    ("memory", "create_doc"): "doc",
    ("memory", "update_doc"): "doc",
    ("memory_create_doc", ""): "doc",
    ("memory_update_doc", ""): "doc",
    ("memory", "create_node"): "node",
    ("memory", "create_todo"): "todo",
    ("memory_create_todo", ""): "todo",
    ("memory", "create_task"): "task",
    ("memory_create_task", ""): "task",
    ("memory", "create_event"): "event",
    ("memory_create_event", ""): "event",
    ("entity", "create"): "entity",
}


def local_kind(path):
    """Kind of durable knowledge a local markdown write stores, or None."""
    lower = (path or "").replace("\\", "/").lower()
    if "/.claude/projects/" in lower and "/memory/" in lower:
        return "claude_auto_memory"
    if re.search(r"/\.claude/plans/[^/]+\.md$", lower):
        return "claude_plan_file"
    segments = lower.split("/")
    name = segments[-1]
    if name in HANDOFF_NAMES:
        return "handoff_file"
    if not (name.endswith(".md") or name.endswith(".mdx")):
        return None
    if any(segment in NON_SOURCE_SEGMENTS for segment in segments):
        return None
    dirs = set(segments[:-1])
    if dirs & {"runbooks", "runbook"} or "runbook" in name:
        return "runbook_file"
    if dirs & {"adr", "adrs", "decisions"}:
        return "adr_file"
    if dirs & {"rfc", "rfcs"}:
        return "rfc_file"
    if dirs & {"postmortems", "postmortem", "incidents"}:
        return "postmortem_file"
    if "notes" in dirs or name in {"notes.md", "todo.md", "decisions.md", "lessons.md", "memory.md"}:
        return "notes_file"
    if re.search(r"(^|[-_.])plans?([-_.]|$)", name):
        return "plan_file"
    return None


def persist_kind(tool, arguments):
    """Kind of a ContextStream persistence call, or None for reads."""
    tool = (tool or "").lower()
    arguments = arguments if isinstance(arguments, dict) else {}
    action = str(arguments.get("action") or "").lower()
    kind = PERSIST.get((tool, action)) or PERSIST.get((tool, ""))
    if kind == "capture":
        return "capture:" + str(arguments.get("event_type") or "event").lower()
    if kind == "entity":
        return "entity:" + str(arguments.get("kind") or "unknown").lower()
    if kind == "node":
        return "node:" + str(arguments.get("node_type") or "unknown").lower()
    return kind


def records(path):
    with path.open(errors="replace") as handle:
        for line in handle:
            try:
                record = json.loads(line)
            except ValueError:
                continue
            if isinstance(record, dict):
                yield record


def _week(timestamp):
    try:
        moment = datetime.fromisoformat(str(timestamp).replace("Z", "+00:00"))
    except ValueError:
        return None
    year, week, _ = moment.astimezone(timezone.utc).isocalendar()
    return f"{year}-W{week:02d}"


def _arguments(raw):
    if isinstance(raw, str):
        try:
            raw = json.loads(raw)
        except ValueError:
            return raw
    return raw


def claude_events(path):
    """Yield (week, side, kind) for durable saves in a Claude Code transcript."""
    for record in records(path):
        if record.get("type") != "assistant":
            continue
        week = _week(record.get("timestamp", ""))
        content = (record.get("message") or {}).get("content")
        if not week or not isinstance(content, list):
            continue
        for block in content:
            if not isinstance(block, dict) or block.get("type") != "tool_use":
                continue
            name = block.get("name") or ""
            arguments = block.get("input") or {}
            if name in WRITE_TOOLS:
                kind = local_kind(arguments.get("file_path") or arguments.get("notebook_path"))
                if kind:
                    yield week, "local", kind
            elif name.startswith("mcp__") and "contextstream" in name.lower():
                kind = persist_kind(name.rsplit("__", 1)[-1], arguments)
                if kind:
                    yield week, "contextstream", kind


def _exec_events(body):
    for match in EXEC_CS_CALL.finditer(body):
        if "contextstream" not in match.group(1).lower():
            continue
        fields = dict(EXEC_FIELD.findall(match.group(3) or ""))
        kind = persist_kind(match.group(2), fields)
        if kind:
            yield "contextstream", kind
    for path in PATCH_PATH.findall(body):
        kind = local_kind(path)
        if kind:
            yield "local", kind


def codex_events(path):
    """Yield (week, side, kind) for durable saves in a Codex rollout."""
    for record in records(path):
        payload = record.get("payload") if isinstance(record.get("payload"), dict) else record
        if payload.get("type") not in ("function_call", "custom_tool_call", "local_shell_call"):
            continue
        week = _week(record.get("timestamp", ""))
        if not week:
            continue
        name = payload.get("name") or ""
        raw = payload.get("input", payload.get("arguments"))
        arguments = _arguments(raw)
        if "contextstream" in name.lower():
            kind = persist_kind(name.rsplit("__", 1)[-1], arguments)
            if kind:
                yield week, "contextstream", kind
            continue
        body = raw if isinstance(raw, str) else json.dumps(raw or "")
        body = body.replace("\\n", "\n")
        for side, kind in _exec_events(body):
            yield week, side, kind


READERS = {"claude-code": claude_events, "codex": codex_events}


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
    weeks = defaultdict(lambda: {"local": Counter(), "contextstream": Counter()})
    for path in transcripts(root, since):
        for week, side, kind in READERS[agent](path):
            weeks[week][side][kind] += 1
    report = {}
    for week in sorted(weeks):
        local = weeks[week]["local"]
        cs = weeks[week]["contextstream"]
        total = sum(local.values()) + sum(cs.values())
        report[week] = {
            "local": sum(local.values()),
            "contextstream": sum(cs.values()),
            "local_share": round(sum(local.values()) / total, 4) if total else None,
            "local_kinds": dict(sorted(local.items())),
            "contextstream_kinds": dict(sorted(cs.items())),
        }
    return report


def render(report):
    lines = [f"window: last {report['days']} days"]
    for agent, weeks in report["agents"].items():
        lines.append(f"{agent}:")
        if not weeks:
            lines.append("  (no transcripts)")
        for week, stats in weeks.items():
            share = "n/a" if stats["local_share"] is None else f"{stats['local_share']:.1%}"
            kinds = ", ".join(f"{k} {v}" for k, v in stats["local_kinds"].items()) or "-"
            lines.append(
                f"  {week}  contextstream {stats['contextstream']:>5}  local {stats['local']:>4}  local share {share:>6}  local: {kinds}"
            )
    return "\n".join(lines)


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    parser.add_argument("--days", type=float, default=28, help="look back this many days (default 28)")
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
