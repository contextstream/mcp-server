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


def instructions(block):
    """The instructions of `block`, one string each: comments dropped, a line ending in a backslash joined to the next
    with one space."""
    found, pending = [], ""
    for line in block.splitlines():
        if not pending and (not line.strip() or line.lstrip().startswith("#")):
            continue
        pending += line.strip()
        if pending.endswith("\\"):
            pending = pending[:-1].rstrip() + " "
            continue
        found.append(pending)
        pending = ""
    return found


def arguments(block):
    """The names a stage declares with ARG, in order (a default after `=` is not part of the name)."""
    return [line.split(None, 1)[1].split("=", 1)[0] for line in instructions(block) if line.startswith("ARG ")]


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

    # What a run's build step is keyed by, pinned as text. A release reuses its dry run's build only while every input of
    # the step is the same in both runs: an argument that differs per run (a run id, a mode, a time), or a flag on a COPY
    # that makes the copy depend on the run, turns every release into a full build again, silently. Whoever changes one
    # of these on purpose changes the test with it, and says in the change why the reuse survives.
    TREE = ["COPY --chown=1001:1001 . /home/runner/tree", "RUN rm -rf /home/runner/tree/.git"]
    BUILD = [
        "ENV CARGO_HOME=/home/runner/.cargo CI=true",
        "WORKDIR /home/runner/src",
        "COPY --from=tree --chown=1001:1001 /home/runner/tree/ .",
        "ARG ARTIFACT",
        "ARG VERSION",
        "ARG SOURCE_DATE_EPOCH",
    ]
    BUILD_RUN = ("RUN --mount=type=cache,id=mcp-cargo-home,target=/home/runner/.cargo,uid=1001,gid=1001 "
                 "--mount=type=cache,id=mcp-build-target-${ARTIFACT},target=/home/runner/src/target,uid=1001,gid=1001,sharing=locked "
                 "--mount=type=cache,id=mcp-build-xwin-${ARTIFACT},target=/home/runner/.cache/cargo-xwin,uid=1001,gid=1001,sharing=locked "
                 "find . -path ./target -prune -o -type f -exec touch {} + "
                 "&& git config --global --add safe.directory '*' && bash scripts/release/build.sh \"$ARTIFACT\" \"$VERSION\"")

    def test_the_tree_stage_is_exactly_the_checkout_without_git(self):
        self.assertEqual(instructions(stage(DOCKERFILE.read_text(), "tree")), self.TREE)

    def test_the_build_stage_takes_exactly_its_three_arguments(self):
        text = DOCKERFILE.read_text()
        build = stage(text, "build")
        self.assertEqual(arguments(build), ["ARTIFACT", "VERSION", "SOURCE_DATE_EPOCH"])
        self.assertEqual(instructions(build), self.BUILD + [self.BUILD_RUN])
        # The stages the build stands on declare no argument but the image's own: none of them is per run.
        self.assertEqual({name: arguments(stage(text, name)) for name in ("env", "build-env", "tree", "binary")},
                         {"env": ["NODE_VERSION", "RUST_CHANNEL"], "build-env": [], "tree": [], "binary": []})
        self.assertEqual(re.findall(r"^ARG (\w+)", text.split("\nFROM ", 1)[0], re.M), ["UBUNTU"])
        # The per-run arguments (PART, RUN_ID) belong to the gate, whose step must run every time.
        self.assertEqual(arguments(stage(text, "gate")), ["PART", "RUN_ID"])
        self.assertEqual(arguments(stage(text, "src")), [])

    def test_the_binary_stage_exports_exactly_the_build_output(self):
        self.assertEqual(instructions(stage(DOCKERFILE.read_text(), "binary")), ["COPY --from=build /home/runner/src/out/ /"])

    def test_the_check_sees_an_added_argument_and_an_added_flag(self):
        self.assertEqual(arguments("WORKDIR /w\nARG ARTIFACT\nARG RUN_ID\nARG NODE=22\nRUN true\n"), ["ARTIFACT", "RUN_ID", "NODE"])
        self.assertEqual(instructions("# a comment\nCOPY --chown=1001:1001 --link . /t\n\nRUN a \\\n && b\n"),
                         ["COPY --chown=1001:1001 --link . /t", "RUN a && b"])
        self.assertNotEqual(instructions("COPY --chown=1001:1001 --link . /home/runner/tree\nRUN rm -rf /home/runner/tree/.git\n"),
                            self.TREE)

    def test_the_check_sees_a_copy_of_the_build_context(self):
        block = "WORKDIR /w\nCOPY --chown=1001:1001 . .\nCOPY --from=tree --chown=1001:1001 /home/runner/tree/ .\nCOPY a b\n"
        self.assertEqual(copies(block), [(None, ". ."), ("tree", "/home/runner/tree/ ."), (None, "a b")])

    def test_the_check_sees_a_shared_lock(self):
        block = ("RUN --mount=type=cache,id=shared,target=/x,sharing=locked "
                 "--mount=type=cache,id=own-${ARTIFACT},target=/y,sharing=locked true\n")
        self.assertEqual(sorted(locked_mounts(block)), ["own-${ARTIFACT}", "shared"])


if __name__ == "__main__":
    unittest.main()
