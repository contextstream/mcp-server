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


class Decide(unittest.TestCase):
    def test_a_new_version_releases_the_pushed_commit(self):
        self.assertEqual(release.decide("1.0.30", "release", None, HEAD, state())[:3], (True, True, HEAD))

    def test_a_tag_fixes_the_commit_whatever_main_holds(self):
        self.assertEqual(release.decide("1.0.30", "release", TAG, HEAD, state())[:3], (True, True, TAG))

    def test_a_released_version_ships_nothing(self):
        ship, build, _, reason = release.decide("1.0.30", "release", TAG, HEAD, DONE)
        self.assertEqual((ship, build), (False, False))
        self.assertIn("fully released", reason)

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
