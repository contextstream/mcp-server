"""Tests for coach.py against a local stub of the ContextStream Coach API.

Run: python3 -I -m unittest discover -s kits/agent-coach-kit/codex-cloud/tests -v
"""

import base64
import contextlib
import importlib.util
import io
import json
import os
import pathlib
import socket
import tempfile
import threading
import unittest
import urllib.error
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

    do_GET = do_POST = do_PATCH = do_PUT = do_DELETE = _handle

    def do_CONNECT(self):
        """Act as a network proxy that refuses the host (a domain not allowed)."""
        Stub.log.append({"method": "CONNECT", "path": self.path, "headers": {}, "body": b""})
        self.send_response(403, "Forbidden")
        self.send_header("Content-Length", "0")
        self.end_headers()

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
        cls.server.server_close()

    def setUp(self):
        Stub.routes = {}
        Stub.log = []
        self.state = tempfile.TemporaryDirectory()
        self.env = {"CONTEXTSTREAM_API_KEY": KEY, "CONTEXTSTREAM_API_URL": self.base,
                    "COACH_STATE_DIR": self.state.name, "no_proxy": "", "NO_PROXY": "",
                    "https_proxy": "", "HTTPS_PROXY": "", "http_proxy": "", "HTTP_PROXY": ""}
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

    def paths(self, method):
        return [e["path"] for e in Stub.log if e["method"] == method]

    def closed_port(self):
        with socket.socket() as s:
            s.bind(("127.0.0.1", 0))
            return s.getsockname()[1]

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

    # -- replay and retries ------------------------------------------------

    def test_a_lowercase_replay_header_reports_already_saved(self):
        self.route("POST", "/api/v1/coach/workflows",
                   ok({"ref": "weekly-note"}, 200, {"idempotent-replayed": "true"}))
        code, out, _ = self.run_cli("create", "--name", "Weekly note", "--goal", "g")
        self.assertEqual(code, 0)
        self.assertIn("[COACH_ALREADY_SAVED] workflow weekly-note", out)

    def test_an_operation_in_progress_is_retried_with_the_same_id(self):
        busy = (409, {"Retry-After": "2"},
                {"success": False, "data": None,
                 "error": {"code": "operation_in_progress", "message": "operation_in_progress: wait"}})
        self.route("POST", "/api/v1/coach/workflows", busy, ok({"ref": "weekly-note"}))
        code, out, _ = self.run_cli("create", "--name", "Weekly note", "--goal", "g")
        self.assertEqual(code, 0, out)
        ids = {b["operation_id"] for b in self.bodies("POST", "/api/v1/coach/workflows")}
        self.assertEqual(len(ids), 1)
        self.assertEqual(self.sleeps, [2.0])

    def test_an_operation_id_used_for_another_request_is_a_conflict(self):
        self.route("POST", "/api/v1/coach/workflows",
                   (409, {}, {"success": False, "data": None,
                              "error": {"code": "operation_id_conflict",
                                        "message": "operation_id_conflict: different request"}}))
        code, _, err = self.run_cli("create", "--name", "Weekly note", "--goal", "g")
        self.assertEqual(code, coach.EXIT_CONFLICT)
        self.assertIn("[COACH_CONFLICT] Operation id cli-", err)
        self.assertIn("Nothing changed", err)

    def test_a_rerun_after_the_guidance_changed_sends_the_first_version_again(self):
        self.route("GET", "/api/v1/coach/guidance/n%2Fg1", ok({"version": 1}, 200), ok({"version": 2}, 200))
        self.route("PATCH", "/api/v1/coach/guidance/n%2Fg1",
                   ok({"ref": "n/g1", "version": 2, "status": "active"}, 200),
                   ok({"ref": "n/g1", "version": 2, "status": "active"}, 200,
                      {"idempotent-replayed": "true"}))
        self.run_cli("edit-guidance", "n/g1", "--text", "Shorter.", "--reason", "Asked")
        code, out, _ = self.run_cli("edit-guidance", "n/g1", "--text", "Shorter.", "--reason", "Asked")
        self.assertEqual(code, 0, out)
        sent = self.bodies("PATCH", "/api/v1/coach/guidance/")
        self.assertEqual([b["expected_version"] for b in sent], [1, 1])
        self.assertEqual(sent[0]["operation_id"], sent[1]["operation_id"])
        self.assertEqual(len(self.paths("GET")), 1, "a rerun does not read the version again")
        self.assertIn("[COACH_ALREADY_SAVED]", out)

    def test_a_rerun_of_a_save_sends_the_context_etag_it_first_sent(self):
        self.route("GET", "/api/v1/coach/jobs/n%231/context",
                   ok({"text": "...", "context_etag": "a" * 32}, 200),
                   ok({"text": "...", "context_etag": "b" * 32}, 200))
        self.route("POST", "/api/v1/coach/jobs/n%231/outputs", ok({"ref": "n#1/v1"}))
        self.run_cli("context", "n#1")
        self.run_cli("save-output", "n#1", stdin="Draft.\n")
        self.run_cli("context", "n#1")
        self.run_cli("save-output", "n#1", stdin="Draft.\n")
        sent = self.bodies("POST", "/api/v1/coach/jobs/n%231/outputs")
        self.assertEqual([b["context_etag"] for b in sent], ["a" * 32, "a" * 32])
        self.assertEqual(sent[0]["operation_id"], sent[1]["operation_id"])
        self.assertNotIn("title", sent[0], "the title is optional")

    # -- network -----------------------------------------------------------

    def test_no_network_prints_the_network_hint_with_its_own_exit_code(self):
        os.environ["CONTEXTSTREAM_API_URL"] = f"http://127.0.0.1:{self.closed_port()}"
        code, out, err = self.run_cli("create", "--name", "Weekly note", "--goal", "g")
        self.assertEqual(code, coach.EXIT_NO_NETWORK)
        self.assertEqual(coach.EXIT_NO_NETWORK, 6)
        self.assertTrue(err.startswith(
            "[COACH_NOT_SAVED] this command needs network access; run it again with network enabled"))
        self.assertEqual(self.sleeps, [1.0, 3.0])
        self.assertNotIn(KEY, out + err)

    def test_an_unreachable_proxy_means_no_network(self):
        os.environ["CONTEXTSTREAM_API_URL"] = "https://api.contextstream.invalid"
        os.environ["https_proxy"] = f"http://127.0.0.1:{self.closed_port()}"
        code, _, err = self.run_cli("list")
        self.assertEqual(code, coach.EXIT_NO_NETWORK)
        self.assertIn("needs network access; run it again with network enabled", err)

    def test_a_proxy_that_refuses_the_host_names_the_domain_to_allow(self):
        os.environ["CONTEXTSTREAM_API_URL"] = "https://api.contextstream.invalid"
        os.environ["https_proxy"] = self.base
        code, _, err = self.run_cli("list")
        self.assertEqual(code, coach.EXIT_NETWORK)
        self.assertIn("allow api.contextstream.invalid", err)
        self.assertEqual(self.paths("CONNECT")[0], "api.contextstream.invalid:443")

    def test_proxy_and_curl_style_failures_are_classified_as_no_network(self):
        url = "https://api.contextstream.io/api/v1/coach/workflows"
        for reason in ("unable to connect to proxy",
                       "Failed to connect to proxy port 8080: Connection refused",
                       ConnectionRefusedError(111, "Connection refused")):
            err = coach.classify_network_error(url, urllib.error.URLError(reason))
            self.assertEqual(err.exit_code, coach.EXIT_NO_NETWORK, reason)
            self.assertIn("needs network access", str(err))
        err = coach.classify_network_error(url, urllib.error.URLError(TimeoutError("timed out")))
        self.assertEqual(err.exit_code, coach.EXIT_NETWORK)

    # -- new commands ------------------------------------------------------

    def test_save_output_records_a_link_with_a_text_snapshot(self):
        with tempfile.NamedTemporaryFile("w", suffix=".md", delete=False) as f:
            f.write("Snapshot of the page.\n")
            snapshot = f.name
        self.addCleanup(os.unlink, snapshot)
        self.route("POST", "/api/v1/coach/jobs/n%231/outputs", ok({"ref": "n#1/v2"}))
        code, out, _ = self.run_cli("save-output", "n#1", "--url", "https://example.com/deck",
                                    "--file", snapshot, "--revises", "n#1/v1")
        self.assertEqual(code, 0, out)
        body = self.bodies("POST", "/api/v1/coach/jobs/n%231/outputs")[0]
        self.assertEqual((body["kind"], body["url"], body["body"]),
                         ("link", "https://example.com/deck", "Snapshot of the page.\n"))
        self.assertIn("[COACH_SAVED] version n#1/v2", out)

    def test_a_link_without_a_snapshot_does_not_read_stdin_and_must_be_https(self):
        self.route("POST", "/api/v1/coach/jobs/n%231/outputs", ok({"ref": "n#1/v1"}))
        code, out, _ = self.run_cli("save-output", "n#1", "--url", "https://example.com/a", stdin="")
        self.assertEqual(code, 0, out)
        self.assertNotIn("body", self.bodies("POST", "/api/v1/coach/jobs/")[0])
        code, _, err = self.run_cli("save-output", "n#1", "--url", "http://example.com/a")
        self.assertEqual(code, coach.EXIT_USAGE)
        self.assertIn("https://", err)

    def test_add_feedback_sends_the_reading_and_the_repeat_apart_from_the_words(self):
        self.route("POST", "/api/v1/coach/outputs/n%231%2Fv1/feedback",
                   ok({"feedback": {"ref": "n#1/f1"}, "guidance": {"ref": "n/g2"}, "possible_repeats": []}))
        code, out, _ = self.run_cli("add-feedback", "n#1/v1", "--kind", "correction", "--scope", "reusable",
                                    "--text", "No jargon!", "--reading", "Plain words for a general reader.",
                                    "--repeat-of", "n/g1")
        self.assertEqual(code, 0, out)
        body = self.bodies("POST", "/api/v1/coach/outputs/")[0]
        self.assertEqual(body["body"], "No jargon!")
        self.assertEqual(body["interpretation"], "Plain words for a general reader.")
        self.assertEqual(body["repeat_of_guidance"], "n/g1")
        self.assertIn("[COACH_SAVED] feedback n#1/f1 (reusable), guidance n/g2", out)

    def test_rescope_sends_the_new_scope(self):
        self.route("POST", "/api/v1/coach/feedback/n%231%2Ff1/rescope",
                   ok({"action": "feedback.rescope", "scope": "workflow", "original": {"ref": "n#1/f1"},
                       "restated": {"ref": "n#1/f3"}, "guidance": {"ref": "n/g4"}, "retired_guidance": None}))
        code, out, _ = self.run_cli("rescope", "n#1/f1", "--scope", "reusable")
        self.assertEqual(code, 0, out)
        body = self.bodies("POST", "/api/v1/coach/feedback/")[0]
        self.assertEqual(body["scope"], "workflow")
        self.assertRegex(body["operation_id"], r"^cli-[0-9a-f]{40}$")
        self.assertIn("[COACH_SAVED] feedback n#1/f1 is now reusable: restated as n#1/f3, guidance n/g4", out)

    def test_import_guidance_names_the_sources(self):
        self.route("POST", "/api/v1/coach/workflows/recap/guidance/import",
                   ok({"action": "guidance.import", "workflow": {"ref": "recap"},
                       "imported": [{"from": "notes/g2", "guidance": {"ref": "recap/g1"}}]}))
        code, out, _ = self.run_cli("import-guidance", "notes/g2", "--into", "recap")
        self.assertEqual(code, 0, out)
        self.assertEqual(self.bodies("POST", "/api/v1/coach/workflows/recap/")[0]["sources"], ["notes/g2"])
        self.assertIn("imported into recap: recap/g1 (from notes/g2)", out)

    def test_set_effort_for_a_workflow_a_job_and_clearing_it(self):
        self.route("PUT", "/api/v1/coach/workflows/n/effort", ok({"coaching_effort": "quick"}, 200))
        self.route("PUT", "/api/v1/coach/jobs/n%232/effort", ok({"coaching_effort": None}, 200))
        code, out, _ = self.run_cli("set-effort", "n", "--effort", "quick", "--said", "Just fix it")
        self.assertEqual(code, 0, out)
        self.assertEqual(self.bodies("PUT", "/api/v1/coach/workflows/")[0]["effort"], "quick")
        self.assertEqual(self.bodies("PUT", "/api/v1/coach/workflows/")[0]["said"], "Just fix it")
        code, out, _ = self.run_cli("set-effort", "n#2", "--effort", "clear")
        self.assertEqual(code, 0, out)
        body = self.bodies("PUT", "/api/v1/coach/jobs/")[0]
        self.assertIn("effort", body)
        self.assertIsNone(body["effort"])
        self.assertNotIn("said", body)
        code, _, err = self.run_cli("set-effort", "n", "--effort", "explore")
        self.assertEqual(code, coach.EXIT_USAGE)
        self.assertIn("--said", err)

    def test_edit_guidance_limits_removes_and_restores(self):
        self.route("GET", "/api/v1/coach/guidance/n%2Fg1", ok({"version": 3}, 200))
        self.route("PATCH", "/api/v1/coach/guidance/n%2Fg1", ok({"ref": "n/g1", "version": 4, "status": "active"}, 200))
        self.run_cli("edit-guidance", "n/g1", "--applies-when", "long posts")
        self.run_cli("edit-guidance", "n/g1", "--applies-when", "")
        self.run_cli("edit-guidance", "n/g1", "--remove")
        self.run_cli("retire-guidance", "n/g1", "--restore", "--reason", "Wanted back")
        sent = self.bodies("PATCH", "/api/v1/coach/guidance/")
        self.assertEqual(sent[0]["applies_when"], "long posts")
        self.assertIn("applies_when", sent[1])
        self.assertIsNone(sent[1]["applies_when"])
        self.assertEqual(sent[2]["status"], "removed")
        self.assertEqual((sent[3]["status"], sent[3]["reason"]), ("active", "Wanted back"))
        self.assertTrue(all(b["expected_version"] == 3 for b in sent))
        code, _, err = self.run_cli("edit-guidance", "n/g1", "--text", "x", "--remove")
        self.assertEqual(code, coach.EXIT_USAGE)

    def test_close_job_sends_the_version_used_and_the_verdict(self):
        self.route("POST", "/api/v1/coach/jobs/n%231/close",
                   ok({"job": {"ref": "n#1", "status": "closed"}, "used": {"ref": "n#1/v2"}}, 200))
        code, out, _ = self.run_cli("close-job", "n#1", "--used", "n#1/v2", "--verdict", "better",
                                    "--note", "Tighter")
        self.assertEqual(code, 0, out)
        body = self.bodies("POST", "/api/v1/coach/jobs/n%231/close")[0]
        self.assertEqual(body["used"], "n#1/v2")
        self.assertEqual(body["verdict"], {"verdict": "better", "note": "Tighter"})
        self.assertIn("[COACH_SAVED] job n#1 closed, used n#1/v2, verdict better", out)
        code, _, err = self.run_cli("close-job", "n#1", "--note", "x")
        self.assertEqual(code, coach.EXIT_USAGE)

    def test_a_verdict_on_a_closed_job_is_recorded_as_a_new_revision(self):
        self.route("POST", "/api/v1/coach/jobs/n%231/close",
                   (409, {}, {"success": False, "data": None,
                              "error": {"code": "job_not_open", "message": "job_not_open: closed"}}))
        self.route("POST", "/api/v1/coach/jobs/n%231/verdicts", ok({"verdict": "same"}))
        code, out, _ = self.run_cli("close-job", "n#1", "--used", "n#1/v2", "--verdict", "same")
        self.assertEqual(code, 0, out)
        body = self.bodies("POST", "/api/v1/coach/jobs/n%231/verdicts")[0]
        self.assertEqual((body["verdict"], body["candidate"]), ("same", "n#1/v2"))
        self.assertIn("already closed", out)

    def test_export_prints_markdown_or_json(self):
        self.route("GET", "/api/v1/coach/workflows/n/export",
                   ok({"format": "markdown", "filename": "n-coach-export.md", "markdown": "# Weekly note\n"}, 200))
        code, out, _ = self.run_cli("export", "n", "--format", "markdown")
        self.assertEqual(code, 0)
        self.assertEqual(out, "# Weekly note\n\n")
        self.assertIn("format=markdown", self.paths("GET")[0])
        Stub.routes[("GET", "/api/v1/coach/workflows/n/export")] = [ok({"workflow": {"ref": "n"}}, 200)]
        code, out, _ = self.run_cli("export", "n", "--format", "json")
        self.assertEqual(json.loads(out), {"workflow": {"ref": "n"}})

    def test_delete_refuses_locally_unless_the_confirm_is_the_slug(self):
        code, _, err = self.run_cli("delete-workflow", "weekly-note", "--confirm", "yes")
        self.assertEqual(code, coach.EXIT_USAGE)
        self.assertIn("[COACH_NOT_SAVED] Not deleted", err)
        self.assertEqual(Stub.log, [], "nothing was sent")
        self.route("DELETE", "/api/v1/coach/workflows/weekly-note",
                   ok({"action": "workflow.delete", "workflow": {"ref": "weekly-note"},
                       "erase_after": "2026-11-11T00:00:00Z"}, 200))
        code, out, _ = self.run_cli("delete-workflow", "weekly-note", "--confirm", "weekly-note")
        self.assertEqual(code, 0, out)
        body = self.bodies("DELETE", "/api/v1/coach/workflows/")[0]
        self.assertEqual(body["confirm"], "weekly-note")
        self.assertRegex(body["operation_id"], r"^cli-")
        self.assertIn("[COACH_SAVED] workflow weekly-note deleted", out)

    def test_delete_by_id_checks_the_slug_first(self):
        wid = "11111111-1111-4111-8111-111111111111"
        self.route("GET", f"/api/v1/coach/workflows/{wid}", ok({"ref": "notes", "slug": "notes"}, 200))
        code, _, err = self.run_cli("delete-workflow", wid, "--confirm", wid)
        self.assertEqual(code, coach.EXIT_USAGE)
        self.assertIn("(notes)", err)
        self.assertEqual(self.paths("DELETE"), [])

    def test_lists_are_read_page_by_page(self):
        self.route("GET", "/api/v1/coach/workflows",
                   ok({"items": [{"ref": "a", "name": "A"}], "total": 2, "page": 1, "per_page": 1,
                       "total_pages": 2}, 200),
                   ok({"items": [{"ref": "b", "name": "B"}], "total": 2, "page": 2, "per_page": 1,
                       "total_pages": 2}, 200))
        code, out, _ = self.run_cli("list")
        self.assertEqual(code, 0)
        self.assertIn("a: A", out)
        self.assertIn("b: B", out)
        self.assertEqual(len(self.paths("GET")), 2)

    def test_the_presigned_upload_host_is_named(self):
        old_limit = coach.INLINE_MEDIA_LIMIT
        coach.INLINE_MEDIA_LIMIT = 4
        self.addCleanup(setattr, coach, "INLINE_MEDIA_LIMIT", old_limit)
        with tempfile.NamedTemporaryFile(suffix=".png", delete=False) as f:
            f.write(b"0123456789")
            image = f.name
        self.addCleanup(os.unlink, image)
        self.route("POST", "/api/v1/coach/jobs/a%231/uploads",
                   ok({"content_id": "c-3", "upload_url": f"{self.base}/presigned/x",
                       "upload_headers": {"Content-Type": "image/png"}}))
        self.route("PUT", "/presigned/x", (200, {}, b""))
        self.route("POST", "/api/v1/coach/jobs/a%231/outputs", ok({"ref": "a#1/v1"}))
        code, out, err = self.run_cli("save-output", "a#1", "--media", image)
        self.assertEqual(code, 0, out + err)
        self.assertIn("directly to 127.0.0.1", err)


if __name__ == "__main__":
    unittest.main()
