#!/usr/bin/env python3
"""scripts/dev/ns-run's LANES (owner 2026-11-02: Namespace only for overflow and coflow): a run goes to a lane host first
(scripts/dev/lanes-run.py, mcp-server's slot), then Namespace labelled with why. A fake lane host's serve sits behind a
fake ssh; NS_RUN_PY points at a stand-in for ns-run.py that records how it was called.

  python3 scripts/dev/test-ns-run-lanes.py
"""
import io, json, os, subprocess, sys, tarfile, tempfile, textwrap, unittest
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
    print("LANES-REPORT " + json.dumps({"v": 1, "id": req["id"], "exit": 0, "seconds": 1, "slot": "mcp-server-rust-1",
                                        "warm": True}), flush=True)
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

    def test_namespace_only_when_no_lane_ran_it_and_says_why(self):
        for env, want in (({"FAKE_LANES": "busy"}, "overflow-busy"), ({"NS_RUN_LANES": "off"}, "off"),
                          ({"DEPOT_RUN_LANES_KEY": str(self.key) + ".missing"}, "off")):
            with self.subTest(env=env):
                done = self.ns_run("--", "true", **env)
                self.assertEqual(done.returncode, 0, done.stderr)
                self.assertIn("NAMESPACE RAN", done.stdout)
                argv = self.namespace_argv()
                self.assertEqual(argv[argv.index("--lanes-state") + 1], want)

    def test_an_arm64_run_goes_to_namespace(self):
        done = self.ns_run("--arch", "arm64", "--", "true")
        self.assertEqual(done.returncode, 0, done.stderr)
        argv = self.namespace_argv()
        self.assertEqual((argv[argv.index("--lanes-state") + 1], argv[argv.index("--arch") + 1]), ("arm64", "arm64"))
        self.assertEqual(list(self.state.glob("lanes-req-*")), [])

    def test_every_lane_host_is_pinned(self):
        lines = [l for l in NS_RUN.read_text().splitlines() if l.startswith("LANE_HOSTS=")]
        self.assertEqual(len(lines), 1, lines)
        pinned = {l.split()[0] for l in (HERE / "lanes-known-hosts").read_text().splitlines() if l.strip() and not l.startswith("#")}
        for host in lines[0].split("=", 1)[1].strip('"').split():
            self.assertIn(host.split("@", 1)[1], pinned)


if __name__ == "__main__":
    unittest.main()
