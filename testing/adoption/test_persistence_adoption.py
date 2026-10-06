"""Synthetic transcripts only; no real session content is read or stored."""
import json
import os
from pathlib import Path
import tempfile
import time
import unittest

from persistence_adoption import local_kind, measure, persist_kind

TS = "2026-10-05T12:00:00Z"
WEEK = "2026-W41"


def write_jsonl(path, records):
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text("".join(json.dumps(record) + "\n" for record in records))


def claude_call(name, **arguments):
    return {
        "type": "assistant",
        "timestamp": TS,
        "message": {"content": [{"type": "tool_use", "id": "t", "name": name, "input": arguments}]},
    }


def codex_call(name, arguments=None, body=None):
    payload = {"type": "custom_tool_call" if body is not None else "function_call", "name": name}
    if body is not None:
        payload["input"] = body
    else:
        payload["arguments"] = json.dumps(arguments or {})
    return {"timestamp": TS, "type": "response_item", "payload": payload}


class ClassifierTests(unittest.TestCase):
    """Mirrors `durable_paths.rs` path classification."""

    def test_local_durable_paths(self):
        for path, kind in [
            ("/Users/me/.claude/projects/-Users-me-repo/memory/prefs.md", "claude_auto_memory"),
            ("/Users/me/.claude/plans/jolly-plan.md", "claude_plan_file"),
            ("/repo/HANDOFF.md", "handoff_file"),
            ("/repo/docs/runbooks/ops-vm.md", "runbook_file"),
            ("/repo/ops/deploy-runbook.md", "runbook_file"),
            ("/repo/docs/adr/0001-queues.md", "adr_file"),
            ("/repo/docs/rfcs/12.mdx", "rfc_file"),
            ("/repo/docs/postmortems/2026-10.md", "postmortem_file"),
            ("/repo/notes/today.md", "notes_file"),
            ("/repo/TODO.md", "notes_file"),
            ("/repo/release-plan.md", "plan_file"),
        ]:
            self.assertEqual(local_kind(path), kind, path)

    def test_ordinary_files_are_not_durable_knowledge(self):
        for path in [
            "/repo/README.md",
            "/repo/docs/explain.md",
            "/repo/src/plan.rs",
            "/repo/node_modules/x/runbooks/a.md",
            "/tmp/claude-501/x/scratchpad/notes/a.md",
            "/Users/me/.claude/projects/-repo/abc.jsonl",
            "",
            None,
        ]:
            self.assertIsNone(local_kind(path), path)

    def test_persist_kinds(self):
        self.assertEqual(persist_kind("session", {"action": "remember"}), "preference")
        self.assertEqual(persist_kind("session", {"action": "capture", "event_type": "decision"}), "capture:decision")
        self.assertEqual(persist_kind("entity", {"action": "create", "kind": "handoff"}), "entity:handoff")
        self.assertEqual(persist_kind("memory", {"action": "create_node", "node_type": "preference"}), "node:preference")
        self.assertEqual(persist_kind("memory_create_doc", {}), "doc")
        self.assertIsNone(persist_kind("memory", {"action": "search"}))
        self.assertIsNone(persist_kind("search", {"query": "x"}))


class MeasureTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.root = Path(self.temp.name)

    def tearDown(self):
        self.temp.cleanup()

    def test_claude_local_share_by_week(self):
        write_jsonl(
            self.root / "claude" / "p" / "s.jsonl",
            [
                claude_call("Write", file_path="/Users/me/.claude/projects/-r/memory/a.md", content="x"),
                claude_call("Write", file_path="/repo/src/main.rs", content="x"),
                claude_call("mcp__contextstream__session", action="capture_lesson", title="t"),
                claude_call("mcp__claude_ai_ContextStream__session", action="remember", content="c"),
                claude_call("mcp__contextstream__search", query="q"),
            ],
        )
        report = measure("claude-code", self.root / "claude", time.time() - 86400)
        week = report[WEEK]
        self.assertEqual(week["local"], 1)
        self.assertEqual(week["contextstream"], 2)
        self.assertEqual(week["local_share"], round(1 / 3, 4))
        self.assertEqual(week["local_kinds"], {"claude_auto_memory": 1})
        self.assertEqual(week["contextstream_kinds"], {"lesson": 1, "preference": 1})

    def test_codex_function_calls_and_code_mode_exec(self):
        exec_body = (
            "const r = await tools.mcp__contextstream__memory({action: \"create_doc\", title: \"Ops\"});\n"
            "await tools.mcp__contextstream__session({ action: 'capture', event_type: 'decision' });\n"
            "await tools.mcp__contextstream__search({query: 'x'});\n"
            "await tools.apply_patch(`*** Begin Patch\n*** Add File: HANDOFF.md\n+hi\n*** End Patch`);\n"
        )
        write_jsonl(
            self.root / "codex" / "2026" / "rollout.jsonl",
            [
                codex_call("mcp__contextstream__entity", {"action": "create", "kind": "ticket"}),
                codex_call("exec", body=exec_body),
                codex_call("shell", {"command": ["bash", "-lc", "ls"]}),
            ],
        )
        report = measure("codex", self.root / "codex", time.time() - 86400)
        week = report[WEEK]
        self.assertEqual(
            week["contextstream_kinds"],
            {"capture:decision": 1, "doc": 1, "entity:ticket": 1},
        )
        self.assertEqual(week["local_kinds"], {"handoff_file": 1})

    def test_old_transcripts_are_outside_the_window(self):
        path = self.root / "claude" / "old.jsonl"
        write_jsonl(path, [claude_call("Write", file_path="/repo/HANDOFF.md", content="x")])
        old = time.time() - 40 * 86400
        os.utime(path, (old, old))
        self.assertEqual(measure("claude-code", self.root / "claude", time.time() - 86400), {})


if __name__ == "__main__":
    unittest.main()
