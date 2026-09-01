#!/usr/bin/env python3

from __future__ import annotations

from pathlib import Path
import re
import subprocess
import sys


SHA = re.compile(r"^[0-9a-fA-F]{40}$")
SIGN_OFF = re.compile(
    r"^Signed-off-by:\s*([^<>\r\n]+?)\s*<([^<>\s]+@[^<>\s]+)>\s*$",
    re.IGNORECASE | re.MULTILINE,
)
MAX_COMMITS = 1_000


def matching_signoff(author_name: str, author_email: str, message: str) -> bool:
    expected_name = author_name.strip().casefold()
    expected_email = author_email.strip().casefold()
    return any(
        name.strip().casefold() == expected_name
        and email.strip().casefold() == expected_email
        for name, email in SIGN_OFF.findall(message)
    )


def git(repo: Path, *args: str) -> bytes:
    return subprocess.check_output(
        ["git", *args],
        cwd=repo,
        stderr=subprocess.STDOUT,
    )


def checked_commits(repo: Path, base: str, head: str) -> list[str]:
    for label, revision in (("base", base), ("head", head)):
        if SHA.fullmatch(revision) is None:
            raise ValueError(f"{label} must be a full 40-character commit SHA")
        git(repo, "rev-parse", "--verify", f"{revision}^{{commit}}")

    commits = git(repo, "rev-list", "--reverse", f"{base}..{head}").decode().splitlines()
    if len(commits) > MAX_COMMITS:
        raise ValueError(f"refusing to inspect more than {MAX_COMMITS} commits")
    return commits


def commit_identity_and_message(repo: Path, revision: str) -> tuple[str, str, str]:
    record = git(repo, "show", "-s", "--format=%an%x00%ae%x00%B", revision)
    fields = record.decode("utf-8", errors="replace").split("\0", 2)
    if len(fields) != 3:
        raise RuntimeError(f"could not parse commit metadata for {revision}")
    return fields[0], fields[1], fields[2]


def main(argv: list[str]) -> int:
    if len(argv) != 4:
        print("usage: dco_check.py REPOSITORY BASE_SHA HEAD_SHA", file=sys.stderr)
        return 2

    repo = Path(argv[1]).resolve()
    try:
        commits = checked_commits(repo, argv[2], argv[3])
        failures: list[str] = []
        for revision in commits:
            author_name, author_email, message = commit_identity_and_message(repo, revision)
            if not matching_signoff(author_name, author_email, message):
                failures.append(
                    f"{revision}: missing a Signed-off-by trailer matching the commit author"
                )
    except (OSError, subprocess.CalledProcessError, ValueError, RuntimeError) as error:
        print(f"DCO check failed: {error}", file=sys.stderr)
        return 2

    if failures:
        print("Developer Certificate of Origin check failed:", file=sys.stderr)
        for failure in failures:
            print(f"  {failure}", file=sys.stderr)
        print("Amend each commit with `git commit --signoff` and update the branch.", file=sys.stderr)
        return 1

    print(f"DCO sign-offs verified for {len(commits)} commit(s).")
    return 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv))
