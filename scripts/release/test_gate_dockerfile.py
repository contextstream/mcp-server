#!/usr/bin/env python3
"""scripts/release/gate.Dockerfile: the six release binaries build side by side on one builder.

A cache mount with sharing=locked admits one build at a time. The build stage runs once per binary, so a locked mount whose
id is the same for every binary makes the six wait for each other; each locked mount's id must name the artifact."""
import re
import unittest
from pathlib import Path

DOCKERFILE = Path(__file__).resolve().parent / "gate.Dockerfile"
MOUNT = re.compile(r"--mount=(\S+)")


def stage(text, name):
    """The instructions of stage `name`, up to the next FROM."""
    match = re.search(r"^FROM \S+ AS %s\n(.*?)(?=^FROM |\Z)" % re.escape(name), text, re.S | re.M)
    if not match:
        raise AssertionError("gate.Dockerfile has no stage %s" % name)
    return match.group(1)


def locked_mounts(block):
    """{id: options} of the cache mounts in `block` that take a lock."""
    found = {}
    for spec in MOUNT.findall(block):
        options = dict(item.split("=", 1) for item in spec.split(",") if "=" in item)
        if options.get("type") == "cache" and options.get("sharing") == "locked":
            found[options["id"]] = options
    return found


class GateDockerfileTest(unittest.TestCase):
    def test_every_locked_mount_of_a_binary_build_is_that_artifacts_own(self):
        mounts = locked_mounts(stage(DOCKERFILE.read_text(), "build"))
        self.assertIn("mcp-build-target-${ARTIFACT}", mounts)
        for mount_id in mounts:
            with self.subTest(mount=mount_id):
                self.assertIn("${ARTIFACT}", mount_id)

    def test_the_check_sees_a_shared_lock(self):
        block = ("RUN --mount=type=cache,id=shared,target=/x,sharing=locked "
                 "--mount=type=cache,id=own-${ARTIFACT},target=/y,sharing=locked true\n")
        self.assertEqual(sorted(locked_mounts(block)), ["own-${ARTIFACT}", "shared"])


if __name__ == "__main__":
    unittest.main()
