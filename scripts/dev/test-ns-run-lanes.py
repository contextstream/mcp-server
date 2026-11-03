#!/usr/bin/env python3
"""scripts/dev/ns-run's LANES (owner 2026-11-03: nothing uses Namespace except coflow and contextstudio): a run goes to a lane host, waits
for a slot when every one is busy, and refuses anything else (Namespace, Bitrise, arm64). A fake lane host's serve sits
behind a fake ssh; NS_RUN_PY points at a stand-in for ns-run.py that must never be called (the tripwire).

  python3 scripts/dev/test-ns-run-lanes.py
"""
import json, os, subprocess, sys, tarfile, tempfile, textwrap, unittest
from pathlib import Path

HERE = Path(__file__).resolve().parent
NS_RUN = HERE / "ns-run"

FAKE_SSH = textwrap.dedent('''\
    #!/usr/bin/env python3
    import io, json, os, sys, tarfile
    from pathlib import Path
    state = Path(os.environ["FAKE_STATE"])
    host = next(a for a in sys.argv[1:] if a.startswith("lanes@"))
    n = len(list(state.glob("lanes-req-*")))
    data = sys.stdin.buffer.read()
    (state / f"lanes-req-{n}").write_bytes(data)
    with tarfile.open(fileobj=io.BytesIO(data)) as t:
        req = json.loads(t.extractfile("request.json").read())
    if os.environ.get("FAKE_LANES") == "busy":
        print("csvalidate: every mcp-server slot is busy", flush=True)
        sys.exit(75)
    print("LANE OUTPUT", flush=True)
    code = 75 if os.environ.get("FAKE_LANES") == "exit75" else 0
    print("LANES-REPORT " + json.dumps({"v": 1, "id": req["id"], "exit": code, "seconds": 1, "slot": "mcp-server-rust-1",
                                        "warm": True}), flush=True)
    sys.exit(code)
''')
FAKE_NS_RUN_PY = textwrap.dedent('''\
    import json, os, sys
    open(os.environ["FAKE_STATE"] + "/ns-run-py.json", "w").write(json.dumps(sys.argv[1:]))
    print("NAMESPACE RAN")
''')


class LanesTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        root = Path(self.tmp.name)
        self.repo, self.state, bindir = root / "repo", root / "state", root / "bin"
        for d in (self.repo, self.state, bindir):
            d.mkdir()
        (bindir / "ssh").write_text(FAKE_SSH)
        (bindir / "ssh").chmod(0o755)
        (root / "ns-run.py").write_text(FAKE_NS_RUN_PY)
        g = lambda *a: subprocess.run(["git", "-C", str(self.repo), *a], check=True, capture_output=True, text=True).stdout
        g("init", "-q", "-b", "main"); g("config", "user.email", "t@example.com"); g("config", "user.name", "t")
        (self.repo / "Cargo.toml").write_text("[workspace]\n"); g("add", "Cargo.toml"); g("commit", "-qm", "base")
        g("update-ref", "refs/remotes/origin/main", "HEAD")
        self.base = g("rev-parse", "HEAD").strip()
        (self.repo / "NEW").write_text("new\n")
        self.key = root / "key"
        self.key.write_text("not a key\n")
        self.env = {**os.environ, "PATH": f"{bindir}{os.pathsep}{os.environ['PATH']}", "FAKE_STATE": str(self.state),
                    "NS_RUN_PY": str(root / "ns-run.py"), "NS_RUN_LANES": "lanes@lane-a", "DEPOT_RUN_LANES_KEY": str(self.key),
                    "TMPDIR": str(root)}

    def tearDown(self):
        self.tmp.cleanup()

    def ns_run(self, *argv, **env):
        return subprocess.run(["bash", str(NS_RUN), *argv], cwd=self.repo, env={**self.env, **env}, capture_output=True,
                              text=True, timeout=60)

    def namespace_argv(self):
        f = self.state / "ns-run-py.json"
        return json.loads(f.read_text()) if f.exists() else None

    def request(self):
        with tarfile.open(sorted(self.state.glob("lanes-req-*"))[-1]) as t:
            return json.loads(t.extractfile("request.json").read()), t.extractfile("patch.diff").read()

    def test_a_run_goes_to_the_lane_as_mcp_server(self):
        done = self.ns_run("--", "bash", "scripts/release/gate.sh", "rust")
        self.assertEqual(done.returncode, 0, done.stderr)
        self.assertIn("LANE OUTPUT", done.stdout)
        self.assertIsNone(self.namespace_argv())
        req, patch = self.request()
        self.assertEqual((req["repo"], req["family"], req["base"], req["command"]),
                         ("mcp-server", "rust", self.base, "bash scripts/release/gate.sh rust"))
        self.assertIn(b"new", patch)   # an untracked file travels too
        self.assertEqual(self.ns_run("--", "true", LANES_FAMILY="mcp-rust").returncode, 0)
        self.assertEqual(self.request()[0]["family"], "mcp-rust")

    def test_a_command_that_exits_75_on_the_lane_is_its_verdict_not_a_refusal(self):
        # 75 is also "no lane ran it"; the result's "ran" tells them apart (read as JSON, not by its spacing).
        done = self.ns_run("--", "bash", "-c", "exit 75", FAKE_LANES="exit75")
        self.assertEqual(done.returncode, 75, done.stderr)
        self.assertIn("LANE OUTPUT", done.stdout)
        self.assertIsNone(self.namespace_argv())

    def test_every_lane_busy_waits_for_a_slot_and_never_goes_anywhere_else(self):
        done = self.ns_run("--wait-s", "2", "--", "true", FAKE_LANES="busy")
        self.assertEqual(done.returncode, 7, done.stderr)
        self.assertIn("every lane host is busy", done.stderr)
        self.assertIn("waiting for a slot", done.stderr)
        self.assertIn("only coflow and contextstudio may use Namespace", done.stderr)
        self.assertGreaterEqual(len(list(self.state.glob("lanes-req-*"))), 1)
        self.assertIsNone(self.namespace_argv())

    def test_no_lane_host_or_no_key_is_a_refusal_naming_the_owner_rule(self):
        for env in ({"NS_RUN_LANES": "off"}, {"DEPOT_RUN_LANES_KEY": str(self.key) + ".missing"}):
            with self.subTest(env=env):
                done = self.ns_run("--", "true", **env)
                self.assertEqual(done.returncode, 7, done.stderr)
                self.assertIn("a lane host is the only place a run happens", done.stderr)
                self.assertIn("only coflow and contextstudio may use Namespace", done.stderr)
                self.assertIsNone(self.namespace_argv())
                self.assertEqual(list(self.state.glob("lanes-req-*")), [])

    def test_an_arm64_run_is_refused_and_never_goes_to_namespace(self):
        done = self.ns_run("--arch", "arm64", "--", "true")
        self.assertEqual(done.returncode, 2, done.stderr)
        self.assertIn("only coflow and contextstudio may use Namespace", done.stderr)
        self.assertEqual(list(self.state.glob("lanes-req-*")), [])
        self.assertIsNone(self.namespace_argv())

    def test_where_namespace_bitrise_and_depot_are_refused(self):
        for where in ("namespace", "bitrise", "depot"):
            with self.subTest(where=where):
                done = self.ns_run("--where", where, "--", "true")
                self.assertEqual(done.returncode, 2, done.stderr)
                self.assertIn("only coflow and contextstudio may use Namespace", done.stderr)
                self.assertEqual(list(self.state.glob("lanes-req-*")), [])
                self.assertIsNone(self.namespace_argv())
        self.assertEqual(self.ns_run("--where", "lanes", "--", "true").returncode, 0)

    def test_print_shows_the_lane_and_runs_nothing(self):
        done = self.ns_run("--print", "--", "true")
        self.assertEqual(done.returncode, 0, done.stderr)
        self.assertIn("would run on a lane host from: lanes@lane-a", done.stdout)
        self.assertEqual(list(self.state.glob("lanes-req-*")), [])
        self.assertIsNone(self.namespace_argv())

    def test_every_lane_host_is_pinned(self):
        lines = [l for l in NS_RUN.read_text().splitlines() if l.startswith("LANE_HOSTS=")]
        self.assertEqual(len(lines), 1, lines)
        pinned = {l.split()[0] for l in (HERE / "lanes-known-hosts").read_text().splitlines() if l.strip() and not l.startswith("#")}
        for host in lines[0].split("=", 1)[1].strip('"').split():
            self.assertIn(host.split("@", 1)[1], pinned)


if __name__ == "__main__":
    unittest.main()
