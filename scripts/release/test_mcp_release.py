#!/usr/bin/env python3
"""Tests for scripts/release/mcp_release.py: what a run decides to do, and the darwin binary check."""
from __future__ import annotations

import struct
import sys
import tempfile
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import mcp_release as release  # noqa: E402

HEAD = "a" * 40
TAG = "b" * 40


def state(**overrides):
    base = {"npm": False, "npm_latest": "1.0.29", "github": "absent", "github_latest": "v1.0.29", "registry": False,
            "r2_commit": None, "r2_latest": "1.0.29"}
    base.update(overrides)
    return base


DONE = state(npm=True, npm_latest="1.0.30", github="published", github_latest="v1.0.30", registry=True,
             r2_commit=TAG, r2_latest="1.0.30")


class _Done:
    def __init__(self, returncode=0, stdout="", stderr=""):
        self.returncode, self.stdout, self.stderr = returncode, stdout, stderr


class FreshInstallCheck(unittest.TestCase):
    """The outside check waits for npm to catch up and reports what is missing instead of failing with a traceback."""

    def test_the_install_is_tried_again_until_the_registry_has_the_version(self):
        answers = [_Done(1, "", "npm error code ETARGET\nnpm error notarget No matching version"), _Done(1), _Done(0)]
        pauses = []
        installed, problem = release.npm_install_fresh(Path("."), {}, "pkg@1.0.30", attempts=5, pause=7,
                                                       run=lambda *a, **k: answers.pop(0), sleep=pauses.append)
        self.assertEqual((installed, problem), (True, ""))
        self.assertEqual(pauses, [7, 7])

    def test_an_install_that_never_works_says_why_and_stops_after_the_attempts(self):
        calls, pauses = [], []

        def run(*args, **kwargs):
            calls.append(args)
            return _Done(1, "", "npm error notarget No matching version found for pkg@1.0.30\n")

        installed, problem = release.npm_install_fresh(Path("."), {}, "pkg@1.0.30", attempts=3, pause=1, run=run,
                                                       sleep=pauses.append)
        self.assertFalse(installed)
        self.assertIn("No matching version", problem)
        self.assertEqual((len(calls), len(pauses)), (3, 2))

    def test_an_install_that_hangs_counts_as_a_failed_attempt(self):
        import subprocess

        def run(*args, **kwargs):
            raise subprocess.TimeoutExpired("npm", 600)

        installed, problem = release.npm_install_fresh(Path("."), {}, "pkg@1", attempts=2, pause=0, run=run,
                                                       sleep=lambda _: None)
        self.assertFalse(installed)
        self.assertIn("did not finish", problem)

    def test_the_signature_audit_waits_for_the_signatures(self):
        answers = [_Done(1, "", ""), _Done(0, "audited 1 package\n1 package has a verified registry signature\n")]
        self.assertTrue(release.npm_signatures_verified(Path("."), {}, attempts=4, pause=0,
                                                        run=lambda *a, **k: answers.pop(0), sleep=lambda _: None))
        self.assertFalse(release.npm_signatures_verified(Path("."), {}, attempts=2, pause=0,
                                                         run=lambda *a, **k: _Done(1), sleep=lambda _: None))

    def test_a_missing_binary_is_a_reason_not_a_traceback(self):
        with tempfile.TemporaryDirectory() as tmp:
            shown, why = release.fresh_binary_version(Path(tmp), {})
            self.assertIsNone(shown)
            self.assertIn("node_modules/.bin/contextstream-mcp", why)

    def test_the_binary_prints_its_version(self):
        with tempfile.TemporaryDirectory() as tmp:
            binary = Path(tmp) / "node_modules/.bin/contextstream-mcp"
            binary.parent.mkdir(parents=True)
            binary.write_text("")
            shown, why = release.fresh_binary_version(
                Path(tmp), {}, run=lambda *a, **k: _Done(0, "downloading\ncontextstream-mcp 1.0.30\n"))
            self.assertEqual((shown, why), ("contextstream-mcp 1.0.30", ""))
            shown, why = release.fresh_binary_version(Path(tmp), {}, run=lambda *a, **k: _Done(0, ""))
            self.assertEqual((shown, why), (None, "it printed nothing"))


class ReleaseRunners(unittest.TestCase):
    """Only the release host releases; any other runner plans nothing."""

    def test_csrun_releases_and_depot_does_not(self):
        self.assertEqual(release.RELEASE_RUNNERS, ("csrun",))
        self.assertTrue(release.runner_releases({"ACT": "true"}))
        self.assertFalse(release.runner_releases({}))
        self.assertFalse(release.runner_releases({"ACT": "false"}))
        self.assertTrue(release.runner_releases({}, runners=("depot", "csrun")), "adding depot back turns it on")

    def test_plan_drops_ship_and_build_on_a_runner_that_does_not_release(self):
        import inspect
        source = inspect.getsource(release.cmd_plan)
        self.assertIn("if (ship or build) and not runner_releases():", source)
        self.assertLess(source.index("not runner_releases()"), source.index('outputs = {"ship": ship'))


class Decide(unittest.TestCase):
    def test_a_new_version_releases_the_pushed_commit(self):
        self.assertEqual(release.decide("1.0.30", "release", None, HEAD, state())[:3], (True, True, HEAD))

    def test_a_tag_fixes_the_commit_whatever_main_holds(self):
        self.assertEqual(release.decide("1.0.30", "release", TAG, HEAD, state())[:3], (True, True, TAG))

    def test_a_released_version_ships_nothing(self):
        ship, build, _, reason = release.decide("1.0.30", "release", TAG, HEAD, DONE)
        self.assertEqual((ship, build), (False, False))
        self.assertIn("fully released", reason)

    def test_a_dispatched_run_checks_a_released_version_again(self):
        ship, build, commit, reason = release.decide("1.0.30", "release", TAG, HEAD, DONE, explicit=True)
        self.assertEqual((ship, build, commit), (True, False, TAG))
        self.assertIn("checking every channel again", reason)

    def test_a_push_never_checks_a_released_version_again(self):
        self.assertEqual(release.decide("1.0.30", "release", TAG, HEAD, DONE, explicit=False)[:2], (False, False))

    def test_a_dispatch_for_a_version_that_is_no_longer_latest_does_nothing(self):
        newer = {**DONE, "npm_latest": "1.0.31", "r2_latest": "1.0.31"}
        ship, build, _, reason = release.decide("1.0.30", "release", TAG, HEAD, newer, explicit=True)
        self.assertEqual((ship, build), (False, False))
        self.assertIn("1.0.31", reason)

    def test_a_dry_run_is_unchanged_by_the_dispatch(self):
        self.assertEqual(release.decide("1.0.30", "dry-run", TAG, HEAD, DONE, explicit=True)[:3], (True, True, HEAD))

    def test_only_a_dispatch_is_explicit(self):
        self.assertTrue(release.dispatched({"GITHUB_EVENT_NAME": "workflow_dispatch"}))
        self.assertFalse(release.dispatched({"GITHUB_EVENT_NAME": "push"}))
        self.assertFalse(release.dispatched({}))
        import inspect
        self.assertIn("explicit=dispatched()", inspect.getsource(release.cmd_plan))

    def test_a_bundle_on_r2_is_reused_not_rebuilt(self):
        partial = state(r2_commit=TAG, r2_latest="1.0.30", github="draft")
        self.assertEqual(release.decide("1.0.30", "release", TAG, HEAD, partial)[:3], (True, False, TAG))

    def test_each_missing_channel_resumes(self):
        for missing in ({"npm": False}, {"registry": False}, {"github": "draft"}, {"r2_latest": "1.0.29"}):
            with self.subTest(missing=missing):
                ship, build, commit, _ = release.decide("1.0.30", "release", TAG, HEAD, {**DONE, **missing})
                self.assertEqual((ship, build, commit), (True, False, TAG))

    def test_an_r2_bundle_from_another_commit_is_refused(self):
        with self.assertRaises(release.Refused):
            release.decide("1.0.30", "release", TAG, HEAD, state(r2_commit=HEAD))

    def test_a_version_below_npm_latest_is_refused(self):
        with self.assertRaises(release.Refused):
            release.decide("1.0.28", "release", None, HEAD, state())

    def test_public_artifacts_without_a_tag_are_refused(self):
        with self.assertRaises(release.Refused):
            release.decide("1.0.30", "release", None, HEAD, state(npm=True, npm_latest="1.0.30"))

    def test_a_dry_run_always_builds_the_checkout(self):
        self.assertEqual(release.decide("1.0.30", "dry-run", TAG, HEAD, DONE)[:3], (True, True, HEAD))


class Notes(unittest.TestCase):
    def test_the_section_of_the_version_without_its_heading(self):
        text = "# Changelog\n\n## 1.0.30\n\n- new\n\n## 1.0.29\n\n- old\n"
        self.assertEqual(release.changelog_section(text, "1.0.30"), "- new\n")

    def test_a_missing_section_is_refused(self):
        with self.assertRaises(release.Refused):
            release.changelog_section("## 1.0.29\n\n- old\n", "1.0.30")


def macho(cputype, dylibs, minos, signed):
    commands = []
    for name in dylibs:
        raw = name.encode() + b"\0"
        raw += b"\0" * (-len(raw) % 8)
        commands.append(struct.pack("<IIIIII", 0xC, 24 + len(raw), 24, 2, 0, 0) + raw)
    commands.append(struct.pack("<IIIIII", 0x32, 24, 1, (minos[0] << 16) | (minos[1] << 8), 0, 0))
    if signed:
        commands.append(struct.pack("<IIII", 0x1D, 16, 0, 0))
    body = b"".join(commands)
    header = struct.pack("<IiiIIIII", 0xFEEDFACF, cputype, 0, 2, len(commands), len(body), 0, 0)
    return header + body + b"contextstream-mcp 1.0.30"


class Darwin(unittest.TestCase):
    required = [lib for lib, needed in release.DARWIN_DYLIBS.items() if needed]

    def check(self, name, data):
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / name
            path.write_bytes(data)
            return release.check_darwin(path, "1.0.30")

    def test_a_binary_like_the_mac_builds_passes(self):
        facts = self.check("contextstream-mcp-darwin-arm64", macho(0x0100000C, self.required, (11, 0), True))
        self.assertTrue(facts["signed"])
        self.check("contextstream-mcp-darwin-x64", macho(0x01000007, self.required, (10, 12), False))

    def test_refusals(self):
        cases = {
            "wrong architecture": ("contextstream-mcp-darwin-arm64", macho(0x01000007, self.required, (11, 0), True)),
            "unsigned arm64": ("contextstream-mcp-darwin-arm64", macho(0x0100000C, self.required, (11, 0), False)),
            "foreign library": ("contextstream-mcp-darwin-x64",
                                macho(0x01000007, self.required + ["/usr/local/lib/libssl.3.dylib"], (10, 12), False)),
            "framework missing": ("contextstream-mcp-darwin-x64", macho(0x01000007, self.required[1:], (10, 12), False)),
            "newer macOS": ("contextstream-mcp-darwin-x64", macho(0x01000007, self.required, (11, 0), False)),
        }
        for label, (name, data) in cases.items():
            with self.subTest(label), self.assertRaises(release.Refused):
                self.check(name, data)


if __name__ == "__main__":
    unittest.main()
