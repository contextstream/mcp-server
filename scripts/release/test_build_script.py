#!/usr/bin/env python3
"""scripts/release/build.sh: every Linux binary starts on glibc 2.28 (RHEL 8, Debian 10) and on anything newer.

A binary needs the glibc it was linked against, or a newer one. Until 1.0.82 the two x64 binaries were built natively on
the release machine (Ubuntu 24.04, glibc 2.39), so they did not start on Debian 12 (2.36), Ubuntu 22.04 (2.35), RHEL 9 or
Amazon Linux 2023 (2.34). All three Linux binaries are built with cargo-zigbuild, which links against the glibc named in
the target's suffix whatever the machine has, and each is refused when it needs a newer one.

The script is pinned as text, as gate.Dockerfile is: a native `cargo build` back in the Linux branch, or a raised floor,
passes every check on the release machine and fails only on a user's."""
import re
import subprocess
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
BUILD = ROOT / "scripts" / "release" / "build.sh"
DOCS = ROOT / "docs" / "release.md"
FLOOR = "2.28"
LINUX = {
    "contextstream-mcp-linux-x64": "x86_64-unknown-linux-gnu",
    "contextstream-mcp-linux-x64-remote-acceleration": "x86_64-unknown-linux-gnu",
    "contextstream-mcp-linux-arm64": "aarch64-unknown-linux-gnu",
}
ZIGBUILD = 'cargo zigbuild --locked --release --package mcp-server --target "$target.$GLIBC_FLOOR" "${features[@]}"'


def linux_branches(text):
    """The Linux branch of the build `case "$kind"` and of the check one: each from `  linux)` to its own `    ;;`."""
    found = re.findall(r"^  linux\)\n(.*?)^    ;;$", text, re.S | re.M)
    if len(found) != 2:
        raise AssertionError("build.sh has %d Linux branches, not a build and a check" % len(found))
    return found


def commands(block):
    """`block` without its comment lines."""
    return "\n".join(line for line in block.splitlines() if not line.lstrip().startswith("#"))


def within_floor(text, newest, floor=FLOOR):
    """What build.sh's own glibc_within_floor says about a binary that needs glibc `newest`."""
    match = re.search(r"^glibc_within_floor\(\) \{.*\}$", text, re.M)
    if not match:
        raise AssertionError("build.sh has no one-line glibc_within_floor()")
    script = 'GLIBC_FLOOR=%s\n%s\nglibc_within_floor "$1"' % (floor, match.group(0))
    return subprocess.run(["bash", "-c", script, "bash", newest]).returncode == 0


class LinuxBuild(unittest.TestCase):
    def test_the_floor_is_glibc_2_28(self):
        self.assertEqual(re.findall(r"^GLIBC_FLOOR=(\S+)$", BUILD.read_text(), re.M), [FLOOR])

    def test_the_three_linux_binaries_are_the_linux_kind(self):
        text = BUILD.read_text()
        for artifact, target in LINUX.items():
            with self.subTest(artifact=artifact):
                self.assertRegex(text, r"(?m)^  %s\)\s+target=%s; kind=linux\b" % (re.escape(artifact), re.escape(target)))
        self.assertEqual(text.count("kind=linux"), len(LINUX))

    def test_every_linux_binary_is_built_with_cargo_zigbuild_against_the_floor(self):
        build, _ = linux_branches(BUILD.read_text())
        build = commands(build)
        # One build command for the three, and never the machine's own compiler and glibc.
        self.assertEqual(build.count(ZIGBUILD), 1)
        self.assertEqual(len(re.findall(r"\bcargo (?:\S+ )?build\b|\bcargo zigbuild\b", build)), 1)
        # Zig and cargo-zigbuild come from pinned downloads.
        for pin in ('"$ZIG_SHA256"', '"$ZIGBUILD_SHA256"'):
            self.assertIn(pin, build)
        # A linker the environment sets for a native build is not Zig: it is dropped for whichever target is built.
        self.assertIn("-u RUSTFLAGS -u CARGO_ENCODED_RUSTFLAGS", build)
        self.assertNotIn("CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_LINKER", build)

    def test_every_linux_binary_is_checked_against_the_floor(self):
        _, check = linux_branches(BUILD.read_text())
        # At the branch's own level: not inside the if that tells a cross-built binary from one this machine runs.
        own_level = [line[4:] for line in commands(check).splitlines() if re.match(r"    \S", line)]
        newest = [line for line in own_level if line.startswith("newest=")]
        self.assertEqual(len(newest), 1)
        self.assertIn("-T \"out/$artifact\" | grep -o 'GLIBC_[0-9.]*'", newest[0])
        refusal = [line for line in own_level if line.startswith('glibc_within_floor "$newest" || {')]
        self.assertEqual(len(refusal), 1, own_level)
        self.assertIn("exit 1", commands(check)[commands(check).index(refusal[0]):])

    def test_a_binary_that_needs_a_newer_glibc_is_refused(self):
        text = BUILD.read_text()
        for newest in ("2.2.5", "2.3", "2.17", "2.27", "2.28"):
            with self.subTest(newest=newest):
                self.assertTrue(within_floor(text, newest))
        # 2.29 to 2.39: what a native build on Ubuntu 20.04 to 24.04 needs. 2.100 sorts after 2.28 as a version.
        for newest in ("2.29", "2.34", "2.36", "2.39", "2.100", "3.0"):
            with self.subTest(newest=newest):
                self.assertFalse(within_floor(text, newest))
        # No version read from the binary is not a pass.
        self.assertFalse(within_floor(text, ""))

    def test_the_release_doc_names_the_same_glibc(self):
        rows = [line for line in DOCS.read_text().splitlines() if line.lstrip().startswith("| `linux-")]
        self.assertEqual(len(rows), 2, rows)
        for row in rows:
            with self.subTest(row=row):
                self.assertIn("cargo-zigbuild", row)
                self.assertIn("glibc %s" % FLOOR, row)
                self.assertNotIn("native", row)
        self.assertNotIn("2.39", "\n".join(rows))


if __name__ == "__main__":
    unittest.main()
