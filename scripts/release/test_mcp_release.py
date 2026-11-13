#!/usr/bin/env python3
"""Tests for scripts/release/mcp_release.py: what a run decides to do, the darwin binary check, the digest of the six
binaries and the time on every line."""
from __future__ import annotations

import contextlib
import hashlib
import io
import json
import struct
import sys
import tempfile
import unittest
from pathlib import Path
from unittest import mock

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

    def test_the_release_host_releases_and_another_runner_does_not(self):
        self.assertEqual(release.RELEASE_RUNNERS, ("release-host",))
        self.assertTrue(release.runner_releases({"ACT": "true"}))
        self.assertFalse(release.runner_releases({}))
        self.assertFalse(release.runner_releases({"ACT": "false"}))
        self.assertTrue(release.runner_releases({}, runners=("other", "release-host")), "adding another runner turns it on")

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


def printed(call, *args, **kwargs):
    """What `call` returned and the lines it printed, parsed."""
    out = io.StringIO()
    with contextlib.redirect_stdout(out):
        result = call(*args, **kwargs)
    return result, [json.loads(line) for line in out.getvalue().splitlines() if line.startswith("{")]


class Lines(unittest.TestCase):
    def test_every_line_says_when_it_was_written(self):
        _, lines = printed(release.say, stage="npm", result="published", version="1.0.30")
        self.assertRegex(lines[0].pop("at"), r"^([01][0-9]|2[0-3]):[0-5][0-9]:[0-5][0-9]Z$")
        self.assertEqual(lines, [{"stage": "npm", "result": "published", "version": "1.0.30"}])

    def test_a_check_line_of_verify_says_when_too(self):
        self.assertRegex(release.check_line(True, "npm latest is 1.0.30"),
                         r"^ok   ([01][0-9]|2[0-3]):[0-5][0-9]:[0-5][0-9]Z npm latest is 1\.0\.30$")
        self.assertRegex(release.check_line(False, "GitHub Latest is v1.0.30"),
                         r"^FAIL ([01][0-9]|2[0-3]):[0-5][0-9]:[0-5][0-9]Z GitHub Latest is v1\.0\.30$")
        import inspect
        source = inspect.getsource(release.cmd_verify)
        self.assertIn("print(check_line(ok, what), flush=True)", source)
        self.assertNotIn('"ok   "', source)

    def test_a_refusal_is_a_line_too(self):
        with tempfile.TemporaryDirectory() as empty:
            code, lines = printed(release.main, ["assemble", "--dist", empty, "--version", "1.0.30", "--commit", HEAD])
        self.assertEqual(code, 1)
        self.assertEqual((lines[-1]["stage"], lines[-1]["error"][:17], "at" in lines[-1]), ("assemble", "the build left no", True))


class Binaries(unittest.TestCase):
    """One digest names the six binaries a run built; a run told which six to expect refuses any other."""

    def setUp(self):
        self._tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self._tmp.cleanup)
        self.dist = Path(self._tmp.name)
        for name in release.BINARIES:
            (self.dist / name).write_bytes(b"binary " + name.encode())

    def test_six_binaries(self):
        self.assertEqual(len(release.BINARIES), 6)
        self.assertEqual(len(set(release.BINARIES)), 6)

    def test_the_digest_is_the_sha256_of_the_sorted_checksum_lines(self):
        lines = ["%s  %s\n" % (hashlib.sha256(b"binary " + name.encode()).hexdigest(), name) for name in release.BINARIES]
        expected = hashlib.sha256("".join(sorted(lines)).encode()).hexdigest()
        self.assertEqual(release.binaries_digest(self.dist), expected)
        self.assertEqual(release.binaries_digest(self.dist), expected, "the same six give the same digest")

    def test_the_digest_does_not_depend_on_the_order_of_the_binaries(self):
        first = release.binaries_digest(self.dist)
        listed = release.BINARIES
        self.addCleanup(setattr, release, "BINARIES", listed)
        release.BINARIES = tuple(reversed(listed))
        self.assertEqual(release.binaries_digest(self.dist), first)
        release.BINARIES = listed[3:] + listed[:3]
        self.assertEqual(release.binaries_digest(self.dist), first)

    def test_one_changed_binary_changes_the_digest(self):
        first = release.binaries_digest(self.dist)
        for name in release.BINARIES:
            with self.subTest(binary=name):
                kept = (self.dist / name).read_bytes()
                (self.dist / name).write_bytes(kept + b"\0")
                self.assertNotEqual(release.binaries_digest(self.dist), first)
                (self.dist / name).write_bytes(kept)
        self.assertEqual(release.binaries_digest(self.dist), first)

    def test_every_run_prints_the_digest(self):
        found, lines = printed(release.check_binaries, "assemble", self.dist)
        self.assertEqual(found, release.binaries_digest(self.dist))
        self.assertEqual({key: value for key, value in lines[0].items() if key != "at"}, {"stage": "assemble", "binaries": found})

    def test_the_expected_binaries_pass(self):
        digest = release.binaries_digest(self.dist)
        for stage in ("assemble", "fetch-bundle"):
            found, lines = printed(release.check_binaries, stage, self.dist, digest)
            self.assertEqual((found, lines[0]["binaries"], lines[0]["expected"], lines[0]["stage"]), (digest, digest, digest, stage))

    def test_a_run_told_nothing_checks_nothing_and_says_so_by_printing_no_expectation(self):
        for given in ((), (None,)):
            found, lines = printed(release.check_binaries, "assemble", self.dist, *given)
            self.assertEqual(found, release.binaries_digest(self.dist))
            self.assertEqual([sorted(line) for line in lines], [["at", "binaries", "stage"]])

    def test_the_whole_digest_is_compared(self):
        # An expectation that agrees with the binaries in its first 8 digits, in all but its last digit, or in all but one
        # digit in the middle is another digest.
        found = release.binaries_digest(self.dist)
        other = lambda digit: "0" if digit != "0" else "1"                                     # noqa: E731
        near = {
            "the first 8 digits agree": found[:8] + "".join(other(digit) for digit in found[8:]),
            "all but the last digit agree": found[:-1] + other(found[-1]),
            "all but digit 9 agree": found[:8] + other(found[8]) + found[9:],
            "all but the first digit agree": other(found[0]) + found[1:],
        }
        for label, expected in near.items():
            with self.subTest(label):
                self.assertEqual(len(expected), 64)
                with contextlib.redirect_stdout(io.StringIO()), self.assertRaises(release.Refused) as refused:
                    release.check_binaries("assemble", self.dist, expected)
                self.assertIn(found, str(refused.exception))
                self.assertIn(expected, str(refused.exception))

    def test_other_binaries_are_refused_and_the_reason_names_both_digests(self):
        expected = release.binaries_digest(self.dist)
        (self.dist / release.BINARIES[0]).write_bytes(b"built again, other bytes")
        found = release.binaries_digest(self.dist)
        self.assertNotEqual(found, expected)
        with contextlib.redirect_stdout(io.StringIO()), self.assertRaises(release.Refused) as refused:
            release.check_binaries("assemble", self.dist, expected)
        self.assertIn(found, str(refused.exception))
        self.assertIn(expected, str(refused.exception))

    def test_a_refusal_says_how_to_go_on(self):
        with contextlib.redirect_stdout(io.StringIO()), self.assertRaises(release.Refused) as refused:
            release.check_binaries("assemble", self.dist, "0" * 64)
        reason = str(refused.exception)
        self.assertIn("this run publishes nothing", reason)
        self.assertIn("a run started again with the same expectation is refused again", reason)
        self.assertIn("start the release without an expectation", reason)
        self.assertIn("start a new dry run and give the release the digest that dry run prints", reason)
        with contextlib.redirect_stdout(io.StringIO()), self.assertRaises(release.Refused) as refused:
            release.check_binaries("fetch-bundle", self.dist, "0" * 64)
        reason = str(refused.exception)
        self.assertIn("a run started again with the same expectation is refused again", reason)
        self.assertIn("To publish the bundle R2 holds, start the release without an expectation", reason)
        self.assertNotIn("new dry run", reason, "a dry run cannot change the bundle R2 holds")

    def bad_expectations(self):
        """Values an operator can pass that are not the digest a run printed. None of them may switch the check off."""
        digest = release.binaries_digest(self.dist)
        return {
            "one space": " ", "a tab": "\t", "a newline": "\n", "63 digits": digest[:63], "65 digits": digest + "0",
            "upper case": digest.upper(),
            "empty": "", "spaces and a newline": "  \n", "the digest after a space": " " + digest,
            "the digest and a newline": digest + "\n", "a word": "yes", "64 letters that are no hex": "g" * 64,
            "a 0x prefix": "0x" + digest[2:], "a sha256: prefix": "sha256:" + digest, "quoted": '"%s"' % digest,
            "full-width digits": "\uff10" * 64, "one upper-case digit": digest[:10] + "A" + digest[11:],
        }

    def test_an_expectation_that_is_no_digest_is_refused(self):
        # The digest of these binaries is right there in six of the values: the refusal is about the form, and no form but
        # the exact one is accepted, so a blank value never reads as "no check".
        for label, given in self.bad_expectations().items():
            with self.subTest(label, given=given):
                out = io.StringIO()
                with contextlib.redirect_stdout(out), self.assertRaises(release.Refused) as refused:
                    release.check_binaries("assemble", self.dist, given)
                self.assertIn("exactly the 64 lowercase hex digits", str(refused.exception))
                self.assertIn("this run publishes nothing", str(refused.exception))
                self.assertNotIn("expected", json.loads(out.getvalue().splitlines()[0]))

    def test_assemble_and_fetch_bundle_refuse_a_bad_expectation_given_on_the_command_line(self):
        # The path a run takes: the option's value arrives through argparse. fetch-bundle's R2 and the release contract
        # are stand-ins; neither command may get past the check.
        for command in ("assemble", "fetch-bundle"):
            for label, given in self.bad_expectations().items():
                with self.subTest(command=command, label=label), \
                        mock.patch.object(release, "r2_public", lambda key: (self.dist / key.rsplit("/", 1)[1]).read_bytes()
                                          if key.rsplit("/", 1)[1] in release.BINARIES else b"{}"), \
                        mock.patch.object(release.contract, "verify_release_files") as verified, \
                        mock.patch.object(release, "check_darwin") as darwin, \
                        tempfile.TemporaryDirectory() as fetched:
                    dist = self.dist if command == "assemble" else Path(fetched)
                    code, lines = printed(release.main, [command, "--dist", str(dist), "--version", "1.0.30", "--commit", HEAD,
                                                         "--expect-binaries=" + given])
                    self.assertEqual(code, 1)
                    self.assertEqual([sorted(line) for line in lines], [["at", "binaries", "stage"], ["at", "error", "stage"]])
                    self.assertIn("exactly the 64 lowercase hex digits", lines[1]["error"])
                    verified.assert_not_called()
                    darwin.assert_not_called()
        self.assertEqual(sorted(path.name for path in self.dist.iterdir()), sorted(release.BINARIES))

    def test_the_option_left_out_is_none_never_a_blank_string(self):
        seen = {}
        for command in ("assemble", "fetch-bundle"):
            with mock.patch.object(release, "cmd_" + command.replace("-", "_"), lambda args, command=command: seen.update({command: args.expect_binaries})):
                self.assertEqual(release.main([command, "--dist", str(self.dist), "--version", "1.0.30", "--commit", HEAD]), 0)
        self.assertEqual(seen, {"assemble": None, "fetch-bundle": None})
        import inspect
        self.assertIn("expected=None", str(inspect.signature(release.check_binaries)))

    def test_assemble_refuses_other_binaries_before_it_assembles_anything(self):
        # The refusal comes before the darwin check, the SBOM tool's download and every file assemble writes; publish, which
        # pushes the tag, is a later step of the run.
        found = release.binaries_digest(self.dist)
        code, lines = printed(release.main, ["assemble", "--dist", str(self.dist), "--version", "1.0.30", "--commit", HEAD,
                                             "--expect-binaries", "0" * 64])
        self.assertEqual(code, 1)
        self.assertEqual([line["stage"] for line in lines], ["assemble", "assemble"])
        self.assertEqual((lines[0]["binaries"], lines[0]["expected"]), (found, "0" * 64))
        self.assertIn(found, lines[1]["error"])
        self.assertIn("0" * 64, lines[1]["error"])
        self.assertEqual(sorted(path.name for path in self.dist.iterdir()), sorted(release.BINARIES))

    def test_publish_takes_no_expectation(self):
        # The expectation is checked where the bundle is made or fetched; publish ships the bundle it is given.
        with contextlib.redirect_stderr(io.StringIO()), self.assertRaises(SystemExit):
            release.main(["publish", "--dist", str(self.dist), "--version", "1.0.30", "--commit", HEAD, "--mode", "dry-run",
                          "--expect-binaries", "0" * 64])
        for command in ("assemble", "fetch-bundle"):
            with self.subTest(command=command), contextlib.redirect_stderr(io.StringIO()), self.assertRaises(SystemExit):
                release.main([command, "--dist", str(self.dist), "--version", "1.0.30", "--commit", HEAD, "--expect-binaries"])


class FetchBundle(unittest.TestCase):
    """A run that takes its bundle from R2 (a resume) checks the six binaries R2 serves against the expectation, before
    the bundle is verified and long before publish."""

    def setUp(self):
        self._tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self._tmp.cleanup)
        self.dist = Path(self._tmp.name) / "dist"
        self.r2 = {"v1.0.30/" + name: b"R2's " + name.encode() for name in release.PAYLOAD}
        self.asked, self.verified = [], []
        for name, stand_in in (("r2_public", lambda key: (self.asked.append(key), self.r2.get(key))[1]),):
            patch = mock.patch.object(release, name, stand_in)
            patch.start()
            self.addCleanup(patch.stop)
        patch = mock.patch.object(release.contract, "verify_release_files", lambda *args: self.verified.append(args))
        patch.start()
        self.addCleanup(patch.stop)

    def digest(self):
        lines = sorted("%s  %s\n" % (hashlib.sha256(self.r2["v1.0.30/" + name]).hexdigest(), name) for name in release.BINARIES)
        return hashlib.sha256("".join(lines).encode()).hexdigest()

    def run_it(self, *more):
        return printed(release.main, ["fetch-bundle", "--dist", str(self.dist), "--version", "1.0.30", "--commit", HEAD, *more])

    def test_it_downloads_the_nine_files_prints_the_digest_and_verifies_the_bundle(self):
        code, lines = self.run_it()
        self.assertEqual(code, 0)
        self.assertEqual(sorted(self.asked), sorted(self.r2))
        self.assertEqual({path.name: path.read_bytes() for path in self.dist.iterdir()},
                         {name: self.r2["v1.0.30/" + name] for name in release.PAYLOAD})
        self.assertEqual([{key: value for key, value in line.items() if key != "at"} for line in lines],
                         [{"stage": "fetch-bundle", "binaries": self.digest()},
                          {"stage": "fetch-bundle", "version": "1.0.30", "commit": HEAD, "source": "R2 mcp/v1.0.30"}])
        self.assertEqual(self.verified, [(self.dist, "1.0.30", HEAD)])

    def test_the_expected_bundle_goes_on(self):
        code, lines = self.run_it("--expect-binaries", self.digest())
        self.assertEqual(code, 0)
        self.assertEqual((lines[0]["binaries"], lines[0]["expected"]), (self.digest(), self.digest()))
        self.assertEqual(len(self.verified), 1)

    def test_another_bundle_is_refused_before_it_is_verified(self):
        code, lines = self.run_it("--expect-binaries", "0" * 64)
        self.assertEqual(code, 1)
        self.assertEqual((lines[0]["binaries"], lines[0]["expected"]), (self.digest(), "0" * 64))
        self.assertEqual([line["stage"] for line in lines], ["fetch-bundle", "fetch-bundle"])
        self.assertIn(self.digest(), lines[1]["error"])
        self.assertIn("To publish the bundle R2 holds, start the release without an expectation", lines[1]["error"])
        self.assertEqual(self.verified, [], "the refusal comes before the bundle is verified")

    def test_one_changed_binary_on_r2_is_another_bundle(self):
        expected = self.digest()
        self.r2["v1.0.30/" + release.BINARIES[-1]] += b"\0"
        code, lines = self.run_it("--expect-binaries", expected)
        self.assertEqual(code, 1)
        self.assertEqual(self.verified, [])

    def test_a_file_r2_lacks_is_refused(self):
        del self.r2["v1.0.30/version.json"]
        code, lines = self.run_it("--expect-binaries", self.digest())
        self.assertEqual(code, 1)
        self.assertIn("R2 has no v1.0.30/version.json", lines[-1]["error"])


class AssembleGoesOn(unittest.TestCase):
    """assemble with the expected six: the digest line comes first, with the expectation, and the bundle is made. The
    darwin check, the SBOM tool and the release contract are stand-ins (they need real binaries and a download)."""

    def test_the_expected_six_are_assembled_and_others_are_not(self):
        with tempfile.TemporaryDirectory() as tmp:
            dist = Path(tmp)
            for name in release.BINARIES:
                (dist / name).write_bytes(b"binary " + name.encode())
            digest = release.binaries_digest(dist)
            calls = []

            def generate(folder, version, commit):
                calls.append("generate")
                for name in release.PAYLOAD:
                    if name not in release.BINARIES and name != "sbom.spdx.json":
                        (Path(folder) / name).write_text("{}")

            def syft(argv, **kwargs):
                calls.append("sbom")
                (dist / "sbom.spdx.json").write_text("{}")

            with mock.patch.object(release, "check_darwin", lambda path, version: calls.append("darwin")), \
                    mock.patch.object(release, "fetch_tool", lambda *args: Path("syft")), \
                    mock.patch.object(release.subprocess, "run", syft), \
                    mock.patch.object(release.contract, "generate_release_files", generate), \
                    mock.patch.object(release.contract, "verify_release_files", lambda *args: calls.append("verify")):
                argv = ["assemble", "--dist", str(dist), "--version", "1.0.30", "--commit", HEAD]
                code, lines = printed(release.main, argv + ["--expect-binaries", digest])
                self.assertEqual(code, 0)
                self.assertEqual(calls, ["darwin", "darwin", "sbom", "generate", "verify"])
                self.assertEqual({key: value for key, value in lines[0].items() if key != "at"},
                                 {"stage": "assemble", "binaries": digest, "expected": digest})
                self.assertEqual((lines[1]["stage"], sorted(lines[1]["files"])), ("assemble", sorted(release.PAYLOAD)))
                for name in release.PAYLOAD:
                    if name not in release.BINARIES:
                        (dist / name).unlink()
                del calls[:]
                code, lines = printed(release.main, argv + ["--expect-binaries", digest[:-1] + ("0" if digest[-1] != "0" else "1")])
                self.assertEqual(code, 1)
                self.assertEqual(calls, [], "nothing runs after the refusal")
                self.assertEqual(sorted(path.name for path in dist.iterdir()), sorted(release.BINARIES))


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
