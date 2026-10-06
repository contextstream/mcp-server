"""Synthetic transcripts only; no real session content is read or stored."""
import json
import os
from pathlib import Path
import tempfile
import time
import unittest

import search_adoption as adoption
from search_adoption import measure, shell_code_search


def write_jsonl(path, records):
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text("".join(json.dumps(record) + "\n" for record in records))


def claude_call(name, **arguments):
    return {
        "type": "assistant",
        "message": {"content": [{"type": "tool_use", "id": "t", "name": name, "input": arguments}]},
    }


class ShellClassifierParityTests(unittest.TestCase):
    """Mirrors the `bash_search_*` tests of the Rust PreToolUse hook."""

    def test_detects_code_discovery(self):
        for command, tool in [
            ("grep -rn handle_oauth crates/", "grep"),
            ("grep -nE 'pub async fn handle' src/", "grep"),
            ("rg --type rust 'PgPool' .", "rg"),
            ("fd '\\.rs$' crates/", "fd"),
            ("find . -name '*.rs' -type f", "find"),
            ("cd /home/foo && grep -rn foo .", "grep"),
            ("cd crates; rg -n foo", "rg"),
        ]:
            self.assertEqual(shell_code_search(command), tool, command)

    def test_ignores_filtering_and_metadata_work(self):
        for command in [
            "ps aux | grep contextstream",
            "cat /tmp/log | grep ERROR",
            "find . -mtime -7 -type f",
            "find /var/log -size +10M",
            "grep -n ERROR /tmp/app.log",
            "grep -rn ERROR /var/log/syslog",
            "grep -i warning deploy.txt",
            "grep -n foo src/main.rs",
            "grep -n handler ./crates/api/lib.rs",
            "git status",
            "cargo build",
            "npm test",
            "",
            "   ",
        ]:
            self.assertIsNone(shell_code_search(command), command)


class AdoptionTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.root = Path(self.tmp.name)

    def test_claude_sessions_count_each_search_path(self):
        claude = self.root / "claude"
        write_jsonl(
            claude / "proj" / "a.jsonl",
            [
                claude_call("mcp__contextstream__search", mode="hybrid", query="x"),
                claude_call("mcp__contextstream__search", mode="keyword", query="y"),
                claude_call("mcp__contextstream__context", user_message="hi"),
                claude_call("Bash", command="rg -n foo crates"),
                claude_call("Bash", command="ps aux | grep node"),
                claude_call("Grep", pattern="foo"),
                {"type": "user", "message": {"content": "ignored"}},
            ],
        )
        write_jsonl(
            claude / "proj" / "b.jsonl",
            [claude_call("Bash", command="grep -rn foo ."), claude_call("Glob", pattern="**/*.rs")],
        )

        stats = measure("claude-code", claude, since=0)

        self.assertEqual(stats["sessions"], 2)
        self.assertEqual(stats["sessions_with_contextstream_search"], 1)
        self.assertEqual(stats["contextstream_search"], 2)
        self.assertEqual(stats["contextstream_other"], 1)
        self.assertEqual(stats["shell_search"], 2)
        self.assertEqual(stats["native_search"], 2)
        self.assertEqual(stats["shell_search_tools"], {"grep": 1, "rg": 1})
        self.assertEqual(stats["contextstream_search_share"], round(2 / 6, 4))

    def test_codex_shell_and_mcp_calls(self):
        codex = self.root / "codex"
        write_jsonl(
            codex / "2026" / "rollout-1.jsonl",
            [
                {"type": "response_item", "payload": {
                    "type": "function_call", "name": "shell",
                    "arguments": json.dumps({"command": ["bash", "-lc", "rg -n foo ."]})}},
                {"type": "response_item", "payload": {
                    "type": "function_call", "name": "exec_command",
                    "arguments": json.dumps({"cmd": "grep -rn bar src/"})}},
                {"type": "response_item", "payload": {
                    "type": "function_call", "name": "mcp__contextstream__search",
                    "arguments": json.dumps({"query": "x"})}},
                {"type": "response_item", "payload": {
                    "type": "function_call", "name": "shell",
                    "arguments": json.dumps({"command": ["bash", "-lc", "git status"]})}},
                {"type": "event_msg", "payload": {"type": "agent_message", "message": "rg foo"}},
            ],
        )

        stats = measure("codex", codex, since=0)

        self.assertEqual(stats["sessions"], 1)
        self.assertEqual(stats["sessions_with_contextstream_search"], 1)
        self.assertEqual(stats["contextstream_search"], 1)
        self.assertEqual(stats["shell_search"], 2)

    def test_codex_code_mode_exec_calls_count(self):
        codex = self.root / "codex"
        write_jsonl(
            codex / "2026" / "rollout-2.jsonl",
            [
                {"type": "response_item", "payload": {
                    "type": "custom_tool_call", "name": "exec",
                    "input": "const hits = await tools.mcp__contextstream__search({query: 'x'});\n"
                             "await tools.mcp__contextstream__context({user_message: 'y'});\n"
                             "await tools.mcp__github__search({q: 'z'});"}},
            ],
        )

        stats = measure("codex", codex, since=0)

        self.assertEqual(stats["contextstream_search"], 1)
        self.assertEqual(stats["contextstream_other"], 1)
        self.assertEqual(stats["sessions_with_contextstream_search"], 1)

    def test_window_excludes_old_transcripts_and_missing_dirs_are_empty(self):
        claude = self.root / "claude"
        old = claude / "old.jsonl"
        write_jsonl(old, [claude_call("Grep", pattern="foo")])
        stale = time.time() - 40 * 86400
        os.utime(old, (stale, stale))

        self.assertEqual(measure("claude-code", claude, since=time.time() - 14 * 86400)["sessions"], 0)
        empty = measure("codex", self.root / "does-not-exist", since=0)
        self.assertEqual(empty["sessions"], 0)
        self.assertIsNone(empty["contextstream_search_share"])

    def test_malformed_lines_are_skipped(self):
        claude = self.root / "claude"
        claude.mkdir()
        (claude / "bad.jsonl").write_text(
            "not json\n" + json.dumps(claude_call("Grep", pattern="foo")) + "\n{\n"
        )
        self.assertEqual(measure("claude-code", claude, since=0)["native_search"], 1)

    def test_cli_writes_comparable_json_and_prints_table(self):
        claude = self.root / "claude"
        write_jsonl(claude / "a.jsonl", [claude_call("mcp__contextstream__search", query="x")])
        out = self.root / "report.json"
        import contextlib
        import io

        buffer = io.StringIO()
        with contextlib.redirect_stdout(buffer):
            code = adoption.main([
                "--claude-dir", str(claude),
                "--codex-dir", str(self.root / "none"),
                "--json", str(out),
            ])

        self.assertEqual(code, 0)
        report = json.loads(out.read_text())
        self.assertEqual(report["agents"]["claude-code"]["contextstream_search"], 1)
        self.assertIn("claude-code", buffer.getvalue())
        self.assertIn("cs share", buffer.getvalue())


if __name__ == "__main__":
    unittest.main()
