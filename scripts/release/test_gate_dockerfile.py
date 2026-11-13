#!/usr/bin/env python3
"""scripts/release/gate.Dockerfile: the six release binaries build side by side on one builder, and a release reuses the
build of its dry run.

A cache mount with sharing=locked admits one build at a time. The build stage runs once per binary, so a locked mount whose
id is the same for every binary makes the six wait for each other; each locked mount's id must name the artifact.

Two checkouts of one commit differ under .git, so a build stage that copies the build context never finds its build step in
the builder's cache. It takes the sources from the stage tree, which holds the checkout without .git; the gate needs .git
and keeps the whole checkout. The build step stamps the sources before cargo runs, because a reused layer carries old file
times and cargo trusts them."""
import re
import unittest
from pathlib import Path

DOCKERFILE = Path(__file__).resolve().parent / "gate.Dockerfile"
MOUNT = re.compile(r"--mount=(\S+)")
COPY = re.compile(r"^COPY\s+((?:--\S+\s+)*)(.+)$", re.M)
STAMP = "find . -path ./target -prune -o -type f -exec touch {} +"


def stage(text, name):
    """The instructions of stage `name`, up to the next FROM."""
    match = re.search(r"^FROM \S+ AS %s\n(.*?)(?=^FROM |\Z)" % re.escape(name), text, re.S | re.M)
    if not match:
        raise AssertionError("gate.Dockerfile has no stage %s" % name)
    return match.group(1)


def base(text, name):
    """The stage that stage `name` starts from."""
    match = re.search(r"^FROM (\S+) AS %s$" % re.escape(name), text, re.M)
    if not match:
        raise AssertionError("gate.Dockerfile has no stage %s" % name)
    return match.group(1)


def copies(block):
    """[(the stage a COPY reads, or None for the build context; its sources and destination)] in `block`."""
    found = []
    for flags, paths in COPY.findall(block):
        source = re.search(r"--from=(\S+)", flags)
        found.append((source.group(1) if source else None, paths.strip()))
    return found


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

    def test_a_binary_builds_from_the_checkout_without_git_never_from_the_build_context(self):
        text = DOCKERFILE.read_text()
        self.assertEqual(copies(stage(text, "build")), [("tree", "/home/runner/tree/ .")])
        tree = stage(text, "tree")
        self.assertEqual(copies(tree), [(None, ". /home/runner/tree")])
        self.assertIn("\nRUN rm -rf /home/runner/tree/.git\n", tree)
        self.assertLess(tree.index("COPY "), tree.index("RUN rm -rf /home/runner/tree/.git"))
        # Whatever the stages before the tree and the build hold comes from the image, not from the checkout.
        self.assertEqual((base(text, "tree"), base(text, "build"), base(text, "build-env")), ("env", "build-env", "env"))
        for name in ("env", "build-env"):
            self.assertEqual(copies(stage(text, name)), [])
        self.assertEqual(copies(stage(text, "binary")), [("build", "/home/runner/src/out/ /")])

    def test_the_gate_still_has_the_whole_checkout(self):
        text = DOCKERFILE.read_text()
        self.assertEqual((base(text, "gate"), base(text, "src")), ("src", "env"))
        self.assertEqual(copies(stage(text, "src")), [(None, ". .")])
        self.assertEqual(copies(stage(text, "gate")), [])
        for name in ("src", "gate"):
            self.assertNotRegex(stage(text, name), r"\brm\b.*\.git")
        ignored = (DOCKERFILE.parent / "gate.Dockerfile.dockerignore").read_text().split()
        self.assertNotIn(".git", ignored)

    def test_the_build_step_stamps_the_sources_before_it_builds(self):
        build = stage(DOCKERFILE.read_text(), "build")
        step = build[build.index("RUN "):]
        self.assertIn(STAMP, step)
        self.assertLess(step.index(STAMP), step.index("scripts/release/build.sh"))
        # target/ is the cache mount whose times cargo compares the sources with: the stamp must leave it alone.
        self.assertIn("target=/home/runner/src/target", step)
        self.assertEqual(build.count("RUN "), 1)

    def test_the_check_sees_a_copy_of_the_build_context(self):
        block = "WORKDIR /w\nCOPY --chown=1001:1001 . .\nCOPY --from=tree --chown=1001:1001 /home/runner/tree/ .\nCOPY a b\n"
        self.assertEqual(copies(block), [(None, ". ."), ("tree", "/home/runner/tree/ ."), (None, "a b")])

    def test_the_check_sees_a_shared_lock(self):
        block = ("RUN --mount=type=cache,id=shared,target=/x,sharing=locked "
                 "--mount=type=cache,id=own-${ARTIFACT},target=/y,sharing=locked true\n")
        self.assertEqual(sorted(locked_mounts(block)), ["own-${ARTIFACT}", "shared"])


if __name__ == "__main__":
    unittest.main()
