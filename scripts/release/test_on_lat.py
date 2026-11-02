"""scripts/release/on-lat.sh: a gate part or a release binary on Latitude's BuildKit, or on the box when it cannot be reached."""
import os
import shutil
import subprocess
import tempfile
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
SCRIPTS = ("on-lat.sh", "gate-on-box.sh", "build-on-box.sh")

# A docker that records each call. FAKE_LAT=down fails the builder's bootstrap; FAKE_BUILD=fail fails buildx build; a
# --output type=local build writes the binary its ARTIFACT names; the box scripts' calls succeed.
FAKE_DOCKER = r'''#!/usr/bin/env bash
printf '%s\n' "$*" >> "$FAKE_LOG"
[ -n "${BUILDX_CONFIG:-}" ] && echo "BUILDX_CONFIG $BUILDX_CONFIG" >> "$FAKE_LOG"
if [ "$1 $2" = "buildx inspect" ] && [ "${FAKE_LAT:-}" = down ]; then exit 1; fi
if [ "$1 $2" = "buildx build" ] && [[ " $* " == *" --builder lat "* ]]; then
  [ "${FAKE_BUILD:-}" = fail ] && exit 7
  dest=""; artifact=""
  for a in "$@"; do
    case "$a" in type=local,dest=*) dest=${a#type=local,dest=} ;; ARTIFACT=*) artifact=${a#ARTIFACT=} ;; esac
  done
  [ -z "$dest" ] || [ "${FAKE_EMPTY:-}" = 1 ] || echo binary > "$dest/$artifact"
fi
if [ "$1 $2" = "image inspect" ]; then exit 0; fi
exit 0
'''


class OnLatitudeTests(unittest.TestCase):
    def setUp(self):
        self.tmp = Path(tempfile.mkdtemp())
        self.addCleanup(shutil.rmtree, self.tmp)
        self.repo = self.tmp / "repo"
        (self.repo / "scripts/release").mkdir(parents=True)
        for name in SCRIPTS + ("gate.Dockerfile",):
            shutil.copy(ROOT / "scripts/release" / name, self.repo / "scripts/release" / name)
        shutil.copy(ROOT / "rust-toolchain.toml", self.repo / "rust-toolchain.toml")
        subprocess.run(["git", "init", "-q"], cwd=self.repo, check=True)
        subprocess.run(["git", "-c", "user.name=t", "-c", "user.email=t@t", "commit", "-q", "--allow-empty", "-m", "t"],
                       cwd=self.repo, check=True)
        bin_dir = self.tmp / "bin"
        bin_dir.mkdir()
        (bin_dir / "docker").write_text(FAKE_DOCKER)
        (bin_dir / "docker").chmod(0o755)
        self.log = self.tmp / "docker.log"
        self.env = dict(os.environ, PATH=f"{bin_dir}:{os.environ['PATH']}", FAKE_LOG=str(self.log), RUNNER_TEMP=str(self.tmp),
                        CSRUN_RUN_NUMBER="42")
        self.env.pop("BUILDX_CONFIG", None)
        self.env.pop("RELEASE_LAT", None)

    def run_script(self, *args, **env):
        result = subprocess.run(["bash", "scripts/release/on-lat.sh", *args], cwd=self.repo, env=dict(self.env, **env),
                                capture_output=True, text=True, timeout=60)
        calls = self.log.read_text().splitlines() if self.log.exists() else []
        return result, calls

    def test_a_gate_part_builds_the_gate_stage_on_latitude_and_its_status_is_the_parts(self):
        result, calls = self.run_script("gate", "rust")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("buildx create --name lat --driver remote ssh://buildkit@relbuild-lat1", calls)
        build = [c for c in calls if c.startswith("buildx build")]
        self.assertEqual(len(build), 1)
        for part in ("--builder lat", "--target gate", "PART=rust", "RUN_ID=42-rust-", "--output type=cacheonly",
                     "-f scripts/release/gate.Dockerfile"):
            self.assertIn(part, build[0])
        # The builder lives in a private buildx config, removed afterwards; nothing runs on the box.
        config = next(c for c in calls if c.startswith("BUILDX_CONFIG ")).split(" ", 1)[1]
        self.assertFalse(Path(config).exists())
        self.assertFalse([c for c in calls if c.startswith("run ")])
        self.assertIn("on Latitude", result.stderr)
        failed, _ = self.run_script("gate", "checks", FAKE_BUILD="fail")
        self.assertEqual(failed.returncode, 7)

    def test_a_binary_comes_back_from_latitude_into_out(self):
        result, calls = self.run_script("build", "contextstream-mcp-linux-arm64", "1.2.3", SOURCE_DATE_EPOCH="1700000000")
        self.assertEqual(result.returncode, 0, result.stderr)
        build = next(c for c in calls if c.startswith("buildx build"))
        for part in ("--target binary", "ARTIFACT=contextstream-mcp-linux-arm64", "VERSION=1.2.3",
                     "SOURCE_DATE_EPOCH=1700000000", "type=local,dest="):
            self.assertIn(part, build)
        self.assertEqual((self.repo / "out/contextstream-mcp-linux-arm64").read_text(), "binary\n")
        self.assertTrue(os.access(self.repo / "out/contextstream-mcp-linux-arm64", os.X_OK))

    def test_a_build_that_exports_no_binary_fails(self):
        result, _ = self.run_script("build", "contextstream-mcp-linux-x64", "1.2.3", FAKE_EMPTY="1")
        self.assertEqual(result.returncode, 1)
        self.assertIn("exported no contextstream-mcp-linux-x64", result.stderr)
        self.assertFalse((self.repo / "out/contextstream-mcp-linux-x64").exists())

    def test_when_latitude_cannot_be_reached_the_box_runs_it(self):
        result, calls = self.run_script("gate", "rust", FAKE_LAT="down")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("Latitude's builder is unreachable: gate.sh rust runs on the box", result.stderr)
        self.assertFalse([c for c in calls if c.startswith("buildx build") and "--builder lat" in c])
        box = [c for c in calls if c.startswith("run ") and "relgate.slice" in c]
        self.assertEqual(len(box), 1)
        self.assertIn('scripts/release/gate.sh "$0" rust', self.log.read_text())
        # The box's docker sees no private buildx config.
        after = calls[calls.index(next(c for c in calls if c.startswith("image inspect"))):]
        self.assertFalse([c for c in after if c.startswith("BUILDX_CONFIG ")])

    def test_release_lat_0_goes_to_the_box_without_asking_latitude(self):
        result, calls = self.run_script("build", "contextstream-mcp-linux-x64", "1.2.3", RELEASE_LAT="0")
        self.assertIn("RELEASE_LAT=0", result.stderr)
        self.assertFalse([c for c in calls if c.startswith("buildx create")])
        self.assertTrue([c for c in calls if c.startswith("run ") and "relgate.slice" in c])
        self.assertIn("scripts/release/build.sh", self.log.read_text())

    def test_unknown_parts_and_artifacts_are_refused(self):
        for args in (("gate", "docs"), ("build", "other", "1.2.3"), ("publish",)):
            with self.subTest(args=args):
                result, calls = self.run_script(*args)
                self.assertEqual(result.returncode, 2)
                self.assertEqual(calls, [])

    def test_the_dockerfile_has_the_stages_on_lat_builds(self):
        text = (ROOT / "scripts/release/gate.Dockerfile").read_text()
        for stage in ("AS env", "AS src", "AS gate", "AS build-env", "AS build", "AS binary"):
            self.assertIn(stage, text)
        self.assertIn("--target env", (ROOT / "scripts/release/gate-on-box.sh").read_text())


if __name__ == "__main__":
    unittest.main()
