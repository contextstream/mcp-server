"""Tests for scripts/release/on-namespace.sh: a release step's command on Namespace when csrun runs the release.

    python3 -m unittest discover -s scripts/release -p 'test_on_namespace.py'
"""
import json
import os
import shutil
import stat
import subprocess
import tempfile
import unittest
from pathlib import Path

HERE = Path(__file__).resolve().parent
SCRIPT = HERE / "on-namespace.sh"
TOKEN = json.dumps({"bearer_token": "tok-123"})

# A stand-in ns-run: records what it was given and what it could read, and leaves an artifact the way ns-run brings one back.
FAKE_NS_RUN = r'''#!/usr/bin/env bash
set -euo pipefail
out=$(git rev-parse --show-toplevel)/calls.json
python3 - "$out" "$@" <<'EOF'
import json, os, stat, sys
token = os.environ.get("NSC_TOKEN_FILE", "")
info = os.stat(token) if token else None
json.dump({"argv": sys.argv[2:], "token": open(token).read() if token else None,
           "mode": stat.S_IMODE(info.st_mode) if info else None, "token_file": token,
           "env_token": os.environ.get("REPOS_NAMESPACE_TOKEN"), "nsc_token": os.environ.get("NSC_TOKEN")}, open(sys.argv[1], "w"))
EOF
args=("$@")
for ((i = 0; i < ${#args[@]}; i++)); do
  if [[ "${args[i]}" == --artifact && "${FAKE_LEAVE:-1}" == 1 ]]; then
    mkdir -p ".depot-run-artifacts/inst1/$(dirname "${args[i+1]}")"; echo built > ".depot-run-artifacts/inst1/${args[i+1]}"
  fi
done
exit "${FAKE_EXIT:-0}"
'''


class OnNamespaceTests(unittest.TestCase):
    def setUp(self):
        self.repo = Path(tempfile.mkdtemp())
        self.addCleanup(shutil.rmtree, self.repo, True)
        (self.repo / "scripts" / "release").mkdir(parents=True)
        (self.repo / "scripts" / "dev").mkdir(parents=True)
        shutil.copy(SCRIPT, self.repo / "scripts" / "release" / "on-namespace.sh")
        fake = self.repo / "scripts" / "dev" / "ns-run"
        fake.write_text(FAKE_NS_RUN)
        fake.chmod(0o755)
        env = dict(os.environ, GIT_CONFIG_NOSYSTEM="1", GIT_AUTHOR_NAME="t", GIT_AUTHOR_EMAIL="t@example.com",
                   GIT_COMMITTER_NAME="t", GIT_COMMITTER_EMAIL="t@example.com", HOME=str(self.repo))
        for argv in (["git", "init", "-q", "-b", "main"], ["git", "add", "-A"], ["git", "commit", "-q", "-m", "release"]):
            subprocess.run(argv, cwd=self.repo, env=env, check=True)
        self.env = env

    def run_script(self, *args, token=TOKEN, extra=None):
        env = dict(self.env, **(extra or {}))
        env.pop("REPOS_NAMESPACE_TOKEN", None)
        if token is not None:
            env["REPOS_NAMESPACE_TOKEN"] = token
        env["NSC_TOKEN"] = "must-not-reach-ns-run"
        return subprocess.run(["bash", "scripts/release/on-namespace.sh", *args], cwd=self.repo, env=env,
                              capture_output=True, text=True)

    def calls(self):
        return json.loads((self.repo / "calls.json").read_text())

    def test_the_token_reaches_ns_run_only_as_a_private_file_that_is_gone_afterwards(self):
        done = self.run_script("--cpus", "16", "--", "bash", "scripts/release/gate.sh", "rust")
        self.assertEqual(done.returncode, 0, done.stderr)
        call = self.calls()
        self.assertEqual(call["argv"], ["--cpus", "16", "--", "bash", "scripts/release/gate.sh", "rust"])
        self.assertEqual((call["token"], call["mode"]), (TOKEN, 0o600))
        self.assertIsNone(call["env_token"], "the secret itself is not passed on")
        self.assertIsNone(call["nsc_token"])
        self.assertFalse(Path(call["token_file"]).exists(), "the token file is removed at exit")
        self.assertNotIn("tok-123", done.stdout + done.stderr)

    def test_a_release_checkout_without_origin_main_gets_its_own_commit_as_the_base(self):
        head = subprocess.run(["git", "rev-parse", "HEAD"], cwd=self.repo, capture_output=True, text=True).stdout.strip()
        self.assertEqual(self.run_script("--", "true").returncode, 0)
        ref = subprocess.run(["git", "rev-parse", "refs/remotes/origin/main"], cwd=self.repo, capture_output=True, text=True)
        self.assertEqual(ref.stdout.strip(), head)

    def test_an_artifact_comes_back_to_its_path_and_a_missing_one_fails(self):
        done = self.run_script("--arch", "arm64", "--artifact", "out/contextstream-mcp-linux-arm64", "--", "bash", "build.sh")
        self.assertEqual(done.returncode, 0, done.stderr)
        self.assertEqual((self.repo / "out" / "contextstream-mcp-linux-arm64").read_text(), "built\n")
        self.assertIn("--artifact", self.calls()["argv"])
        (self.repo / "out" / "contextstream-mcp-linux-arm64").unlink()
        done = self.run_script("--artifact", "out/contextstream-mcp-linux-arm64", "--", "true", extra={"FAKE_LEAVE": "0"})
        self.assertEqual(done.returncode, 4)
        self.assertIn("did not come back", done.stderr)

    def test_the_commands_exit_status_is_the_verdict(self):
        done = self.run_script("--artifact", "out/x", "--", "false", extra={"FAKE_EXIT": "1"})
        self.assertEqual(done.returncode, 1)

    def test_a_missing_or_malformed_token_stops_before_ns_run(self):
        for token in (None, "", "not json", json.dumps({"bearer_token": ""}), json.dumps({"token": "x"})):
            with self.subTest(token=token):
                done = self.run_script("--", "true", token=token)
                self.assertEqual(done.returncode, 5, done.stderr)
                self.assertFalse((self.repo / "calls.json").exists())



class WorkflowOnCsrunTests(unittest.TestCase):
    """.depot/workflows/release.yml on csrun: the gate and the builds run on Namespace, nothing compiles on the runner."""

    def setUp(self):
        try:
            import yaml
        except ImportError:
            self.skipTest("PyYAML is not installed")
        self.jobs = yaml.safe_load((HERE.parents[1] / ".depot" / "workflows" / "release.yml").read_text())["jobs"]

    def test_plan_names_the_runner_from_act(self):
        plan = self.jobs["plan"]
        self.assertEqual(plan["outputs"]["runner"], "${{ steps.runner.outputs.runner }}")
        step = next(step for step in plan["steps"] if step.get("id") == "runner")
        self.assertIn('"${ACT:-}" = true', step["run"])

    def test_on_csrun_the_gate_and_builds_run_only_their_namespace_step(self):
        for name in ("gate", "build"):
            steps = self.jobs[name]["steps"]
            namespace = [step for step in steps if step["name"].endswith("on Namespace")]
            self.assertEqual(len(namespace), 1, name)
            self.assertEqual(namespace[0]["if"], "needs.plan.outputs.runner == 'csrun'")
            self.assertIn("scripts/release/on-namespace.sh", namespace[0]["run"])
            self.assertEqual(namespace[0]["env"], {"REPOS_NAMESPACE_TOKEN": "${{ secrets.REPOS_NAMESPACE_TOKEN }}"})
            for step in steps:
                if step is namespace[0] or step["name"].startswith(("Check out", "Upload")):
                    continue
                with self.subTest(job=name, step=step["name"]):
                    self.assertIn("needs.plan.outputs.runner != 'csrun'", step.get("if", ""))
        # The artifact steps: Depot keeps its pins; on csrun, act's artifact server takes only the older actions'
        # requests (the newer ones send mime_type, which it refuses).
        for job, prefix, old_pin in (("build", "Upload", "ea165f8d65b6e75b540449e92b4886f43607fa02"),
                                     ("publish", "Download", "d3f86a106a0bac45b974a628896c90dbdf5c8093")):
            steps = [step for step in self.jobs[job]["steps"] if step["name"].startswith(prefix)]
            self.assertEqual(len(steps), 2, job)
            depot, csrun = steps
            self.assertIn("needs.plan.outputs.runner != 'csrun'", depot["if"])
            self.assertIn("needs.plan.outputs.runner == 'csrun'", csrun["if"])
            self.assertTrue(csrun["uses"].endswith("@" + old_pin), csrun["uses"])
            self.assertNotEqual(depot["uses"], csrun["uses"])
            self.assertEqual(depot["with"], csrun["with"])
        build = next(step for step in self.jobs["build"]["steps"] if step["name"].endswith("on Namespace"))["run"]
        for piece in ('--artifact "out/${{ matrix.artifact }}"', "SOURCE_DATE_EPOCH=${{ needs.plan.outputs.epoch }}",
                      "'contextstream-mcp-linux-arm64' && 'arm64' || 'amd64'"):
            self.assertIn(piece, build)


if __name__ == "__main__":
    unittest.main()
