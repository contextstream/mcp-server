"""Tests for coach.py against a local stub of the ContextStream Coach API.

Run: python3 -m unittest discover -s kits/agent-coach-kit/codex-cloud/tests -v
"""

import base64
import contextlib
import importlib.util
import io
import json
import os
import pathlib
import tempfile
import threading
import unittest
import urllib.parse
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

HERE = pathlib.Path(__file__).resolve().parent
COACH_PATH = HERE.parent / ".agents" / "skills" / "contextstream-coach" / "scripts" / "coach.py"
_spec = importlib.util.spec_from_file_location("coach", COACH_PATH)
coach = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(coach)

KEY = "test-key-never-printed"


def ok(data, status=201, headers=None):
    return (status, headers or {}, {"success": True, "data": data})


class Stub(BaseHTTPRequestHandler):
    routes: dict = {}
    log: list = []

    def _handle(self):
        length = int(self.headers.get("Content-Length") or 0)
        raw = self.rfile.read(length) if length else b""
        is_json = (self.headers.get("Content-Type") or "").startswith("application/json")
        Stub.log.append({
            "method": self.command,
            "path": self.path,
            "headers": {k.lower(): v for k, v in self.headers.items()},
            "body": json.loads(raw) if raw and is_json else raw,
        })
        key = (self.command, urllib.parse.urlsplit(self.path).path)
        queue = Stub.routes.get(key)
        if not queue:
            status, headers, body = 404, {}, {"success": False, "error": f"not_found: {key}"}
        elif len(queue) > 1:
            status, headers, body = queue.pop(0)
        else:
            status, headers, body = queue[0]
        payload = body if isinstance(body, bytes) else json.dumps(body).encode()
        self.send_response(status)
        for name, value in headers.items():
            self.send_header(name, value)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(payload)))
        self.end_headers()
        self.wfile.write(payload)

    do_GET = do_POST = do_PATCH = do_PUT = _handle

    def log_message(self, *args):
        pass


class CoachCliTest(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.server = ThreadingHTTPServer(("127.0.0.1", 0), Stub)
        cls.base = f"http://127.0.0.1:{cls.server.server_address[1]}"
        threading.Thread(target=cls.server.serve_forever, daemon=True).start()

    @classmethod
    def tearDownClass(cls):
        cls.server.shutdown()

    def setUp(self):
        Stub.routes = {}
        Stub.log = []
        self.state = tempfile.TemporaryDirectory()
        self.env = {"CONTEXTSTREAM_API_KEY": KEY, "CONTEXTSTREAM_API_URL": self.base,
                    "COACH_STATE_DIR": self.state.name}
        self.saved_env = {k: os.environ.get(k) for k in self.env}
        os.environ.update(self.env)
        self.sleeps = []

    def tearDown(self):
        for k, v in self.saved_env.items():
            if v is None:
                os.environ.pop(k, None)
            else:
                os.environ[k] = v
        self.state.cleanup()

    def run_cli(self, *argv, stdin=""):
        out, err = io.StringIO(), io.StringIO()
        old_stdin = coach.sys.stdin
        coach.sys.stdin = io.StringIO(stdin)
        try:
            with contextlib.redirect_stdout(out), contextlib.redirect_stderr(err):
                code = coach.main(list(argv), sleep=self.sleeps.append)
        finally:
            coach.sys.stdin = old_stdin
        return code, out.getvalue(), err.getvalue()

    def route(self, method, path, *responses):
        Stub.routes[(method, path)] = list(responses)

    def bodies(self, method, path_prefix):
        return [e["body"] for e in Stub.log if e["method"] == method and e["path"].startswith(path_prefix)]

    # -- writes ------------------------------------------------------------

    def test_create_sends_an_operation_id_and_reports_the_save(self):
        self.route("POST", "/api/v1/coach/workflows", ok({"ref": "weekly-note", "name": "Weekly note"}))
        code, out, _ = self.run_cli("create", "--name", "Weekly note", "--goal", "A candid weekly note.")
        self.assertEqual(code, 0)
        self.assertIn("[COACH_SAVED] workflow weekly-note", out)
        body = self.bodies("POST", "/api/v1/coach/workflows")[0]
        self.assertEqual(body["name"], "Weekly note")
        self.assertEqual(body["goal"], "A candid weekly note.")
        self.assertRegex(body["operation_id"], r"^cli-[0-9a-f]{40}$")
        self.assertEqual(Stub.log[0]["headers"]["x-api-key"], KEY)
        self.assertTrue(Stub.log[0]["headers"]["user-agent"].startswith("contextstream-coach-kit/"))

    def test_a_replayed_operation_reports_already_saved(self):
        self.route("POST", "/api/v1/coach/workflows",
                   ok({"ref": "weekly-note"}, 200, {"Idempotent-Replayed": "true"}))
        code, out, _ = self.run_cli("create", "--name", "Weekly note", "--goal", "g")
        self.assertEqual(code, 0)
        self.assertIn("[COACH_ALREADY_SAVED]", out)

    def test_the_same_command_twice_uses_the_same_operation_id(self):
        self.route("POST", "/api/v1/coach/workflows", ok({"ref": "weekly-note"}))
        self.run_cli("create", "--name", "Weekly note", "--goal", "g")
        self.run_cli("create", "--name", "Weekly note", "--goal", "g")
        self.run_cli("create", "--name", "Weekly note", "--goal", "another goal")
        ids = [b["operation_id"] for b in self.bodies("POST", "/api/v1/coach/workflows")]
        self.assertEqual(ids[0], ids[1])
        self.assertNotEqual(ids[0], ids[2])

    def test_retries_reuse_the_operation_id(self):
        self.route("POST", "/api/v1/coach/workflows",
                   (503, {}, {"success": False, "error": "busy"}),
                   (503, {}, {"success": False, "error": "busy"}),
                   ok({"ref": "weekly-note"}))
        code, out, _ = self.run_cli("create", "--name", "Weekly note", "--goal", "g")
        self.assertEqual(code, 0)
        ids = {b["operation_id"] for b in self.bodies("POST", "/api/v1/coach/workflows")}
        self.assertEqual(len(self.bodies("POST", "/api/v1/coach/workflows")), 3)
        self.assertEqual(len(ids), 1)
        self.assertEqual(self.sleeps, [1.0, 3.0])

    def test_feedback_is_recorded_word_for_word_with_mapped_kind_and_scope(self):
        text = "  Keep the “founder voice” — but cut the jokes.\n"
        self.route("POST", "/api/v1/coach/outputs/weekly-note%234%2Fv2/feedback",
                   ok({"feedback": {"ref": "weekly-note#4/f1"}, "guidance": None, "possible_repeats": []}))
        code, out, _ = self.run_cli("add-feedback", "weekly-note#4/v2", "--kind", "approval",
                                    "--scope", "this-job", "--text", text, "--excerpt", "founder voice")
        self.assertEqual(code, 0, out)
        body = self.bodies("POST", "/api/v1/coach/outputs/")[0]
        self.assertEqual(body["body"], text)
        self.assertEqual(body["kind"], "partial_approval")
        self.assertEqual(body["scope"], "job")
        self.assertEqual(body["origin"], "typed")
        self.assertEqual(body["excerpt"], "founder voice")
        self.assertIn("[COACH_SAVED] feedback weekly-note#4/f1", out)

    def test_reusable_feedback_reports_its_guidance_and_time_ranges_are_parsed(self):
        self.route("POST", "/api/v1/coach/outputs/recap%232%2Fv1/feedback",
                   ok({"feedback": {"ref": "recap#2/f1"}, "guidance": {"ref": "recap/g1"},
                       "possible_repeats": [{"ref": "recap/g0"}]}))
        code, out, _ = self.run_cli("add-feedback", "recap#2/v1", "--kind", "correction",
                                    "--scope", "reusable", "--text", "Slower here.",
                                    "--from", "0:34", "--to", "1:02.5")
        self.assertEqual(code, 0, out)
        body = self.bodies("POST", "/api/v1/coach/outputs/")[0]
        self.assertEqual(body["scope"], "workflow")
        self.assertEqual((body["time_start_ms"], body["time_end_ms"]), (34_000, 62_500))
        self.assertIn("guidance recap/g1", out)
        self.assertIn("may repeat earlier guidance: recap/g0", out)

    def test_picked_suggestions_must_be_directions(self):
        code, _, err = self.run_cli("add-feedback", "recap#2/v1", "--kind", "correction",
                                    "--scope", "reusable", "--text", "x", "--picked")
        self.assertEqual(code, coach.EXIT_USAGE)
        self.assertIn("--kind direction", err)
        self.assertEqual(Stub.log, [])

    def test_save_output_sends_the_text_the_revision_and_the_context_etag(self):
        self.route("GET", "/api/v1/coach/jobs/weekly-note%234/context",
                   ok({"text": "GOAL ...", "context_etag": "a" * 32}, 200))
        self.route("POST", "/api/v1/coach/jobs/weekly-note%234/outputs", ok({"ref": "weekly-note#4/v2"}))
        self.run_cli("context", "weekly-note#4")
        draft = "This week we shipped onboarding.\n"
        code, out, _ = self.run_cli("save-output", "weekly-note#4", "--title", "Draft 2",
                                    "--revises", "weekly-note#4/v1", "--carry-note", "Kept the opening.",
                                    stdin=draft)
        self.assertEqual(code, 0, out)
        body = self.bodies("POST", "/api/v1/coach/jobs/weekly-note%234/outputs")[0]
        self.assertEqual(body["body"], draft)
        self.assertEqual(body["kind"], "text")
        self.assertEqual(body["source_output"], "weekly-note#4/v1")
        self.assertEqual(body["context_etag"], "a" * 32)
        self.assertEqual(body["carry_note"], "Kept the opening.")
        self.assertIn("[COACH_SAVED] version weekly-note#4/v2", out)

    def test_small_media_is_sent_inline_then_linked(self):
        with tempfile.NamedTemporaryFile(suffix=".png", delete=False) as f:
            f.write(b"\x89PNG fake image bytes")
            image = f.name
        self.addCleanup(os.unlink, image)
        self.route("POST", "/api/v1/coach/jobs/art%231/uploads", ok({"content_id": "c-1", "status": "ready"}))
        self.route("POST", "/api/v1/coach/jobs/art%231/outputs", ok({"ref": "art#1/v1"}))
        code, out, _ = self.run_cli("save-output", "art#1", "--title", "Cover", "--media", image)
        self.assertEqual(code, 0, out)
        upload = self.bodies("POST", "/api/v1/coach/jobs/art%231/uploads")[0]
        self.assertEqual(upload["kind"], "image")
        self.assertEqual(base64.b64decode(upload["data_base64"]), b"\x89PNG fake image bytes")
        output = self.bodies("POST", "/api/v1/coach/jobs/art%231/outputs")[0]
        self.assertEqual((output["kind"], output["content_id"]), ("image", "c-1"))

    def test_large_media_goes_through_the_presigned_upload(self):
        old_limit = coach.INLINE_MEDIA_LIMIT
        coach.INLINE_MEDIA_LIMIT = 4
        self.addCleanup(setattr, coach, "INLINE_MEDIA_LIMIT", old_limit)
        with tempfile.NamedTemporaryFile(suffix=".mp3", delete=False) as f:
            f.write(b"0123456789")
            audio = f.name
        self.addCleanup(os.unlink, audio)
        self.route("POST", "/api/v1/coach/jobs/pod%231/uploads",
                   ok({"content_id": "c-2", "status": "pending",
                       "upload_url": f"{self.base}/presigned/abc",
                       "upload_headers": {"Content-Type": "audio/mpeg"}}))
        self.route("PUT", "/presigned/abc", (200, {}, b""))
        self.route("POST", "/api/v1/coach/jobs/pod%231/outputs", ok({"ref": "pod#1/v1"}))
        code, out, _ = self.run_cli("save-output", "pod#1", "--title", "Episode", "--media", audio)
        self.assertEqual(code, 0, out)
        upload = self.bodies("POST", "/api/v1/coach/jobs/pod%231/uploads")[0]
        self.assertEqual(upload["upload"], {"size_bytes": 10})
        self.assertNotIn("data_base64", upload)
        put = [e for e in Stub.log if e["method"] == "PUT"][0]
        self.assertEqual(put["body"], b"0123456789")
        self.assertNotIn("x-api-key", put["headers"], "the key never goes to the storage host")

    def test_refine_goal_reads_the_current_version_first(self):
        self.route("GET", "/api/v1/coach/workflows/weekly-note", ok({"ref": "weekly-note", "goal_version": 3}, 200))
        self.route("POST", "/api/v1/coach/workflows/weekly-note/goals", ok({"version": 4}))
        code, out, _ = self.run_cli("refine-goal", "weekly-note", "--goal", "New goal", "--reason", "Asked")
        self.assertEqual(code, 0, out)
        body = self.bodies("POST", "/api/v1/coach/workflows/weekly-note/goals")[0]
        self.assertEqual(body["expected_goal_version"], 3)
        self.assertIn("version 4", out)

    def test_a_conflict_says_nothing_changed(self):
        self.route("GET", "/api/v1/coach/guidance/weekly-note%2Fg2", ok({"version": 1}, 200))
        self.route("PATCH", "/api/v1/coach/guidance/weekly-note%2Fg2",
                   (409, {}, {"success": False, "error": "version_conflict: guidance changed"}))
        code, _, err = self.run_cli("retire-guidance", "weekly-note/g2", "--reason", "Asked")
        self.assertEqual(code, coach.EXIT_CONFLICT)
        self.assertIn("[COACH_CONFLICT]", err)
        self.assertIn("Nothing changed", err)

    # -- auth and setup ----------------------------------------------------

    def test_a_refused_key_exits_with_a_setup_hint_and_never_prints_the_key(self):
        self.route("GET", "/api/v1/coach/workflows", (401, {}, {"success": False, "error": "unauthorized"}))
        code, out, err = self.run_cli("list")
        self.assertEqual(code, coach.EXIT_AUTH)
        self.assertIn("network secret CONTEXTSTREAM_API_KEY", err)
        self.assertNotIn(KEY, out + err)

    def test_doctor_reports_each_check_without_the_key(self):
        self.route("GET", "/api/v1/auth/me", ok({"id": "u"}, 200))
        self.route("GET", "/api/v1/coach/capabilities", ok({"contract": "coach.v1", "enabled": True}, 200))
        code, out, err = self.run_cli("doctor")
        self.assertEqual(code, 0, out + err)
        self.assertIn("key: set", out)
        self.assertIn("sign-in: HTTP 200", out)
        self.assertIn("coach: enabled (contract coach.v1)", out)
        self.assertNotIn(KEY, out + err)

    def test_bearer_auth_is_available(self):
        self.route("GET", "/api/v1/coach/workflows", ok({"items": []}, 200))
        code, out, _ = self.run_cli("--auth", "bearer", "list")
        self.assertEqual(code, 0)
        self.assertEqual(Stub.log[0]["headers"]["authorization"], f"Bearer {KEY}")
        self.assertNotIn("x-api-key", Stub.log[0]["headers"])

    def test_start_job_ids_change_between_ten_minute_windows(self):
        a = coach.operation_id("start_job", {"objective": "x"}, 10, now=0)
        b = coach.operation_id("start_job", {"objective": "x"}, 10, now=599)
        c = coach.operation_id("start_job", {"objective": "x"}, 10, now=600)
        self.assertEqual(a, b)
        self.assertNotEqual(a, c)

    def test_there_is_no_delete_command(self):
        self.assertFalse(any("delete" in name for name in coach.COMMANDS))


if __name__ == "__main__":
    unittest.main()
