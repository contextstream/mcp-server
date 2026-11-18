#!/usr/bin/env python3
"""The release gate's advisory checks (cargo audit and cargo deny) ignore the same advisories, each for a stated reason
and only until a review date.

An ignored advisory is a decision with an end: each entry in deny.toml carries a reason that names the review date
(`review by YYYY-MM-DD`), .cargo/audit.toml ignores exactly the same ids, and no review date has passed, so a lapsed
ignore stops the release until someone looks again."""
import datetime
import re
import tomllib
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
REVIEW = re.compile(r"review by (\d{4}-\d{2}-\d{2})")
# chrono is unmaintained (not a vulnerability) and has no maintained release to move to yet; a ticket tracks moving off it.
EXPECTED = {"RUSTSEC-2026-0543"}


def deny_ignores():
    entries = tomllib.loads((ROOT / "deny.toml").read_text())["advisories"].get("ignore", [])
    return {e["id"]: e.get("reason", "") for e in entries if isinstance(e, dict)}, [e for e in entries if not isinstance(e, dict)]


def audit_ignores():
    path = ROOT / ".cargo" / "audit.toml"
    if not path.exists():
        return set()
    return set(tomllib.loads(path.read_text()).get("advisories", {}).get("ignore", []))


class AdvisoryIgnores(unittest.TestCase):
    def test_both_tools_ignore_the_same_advisories(self):
        deny, _ = deny_ignores()
        self.assertEqual(set(deny), audit_ignores())

    def test_the_expected_advisories_are_ignored(self):
        deny, _ = deny_ignores()
        self.assertEqual(set(deny), EXPECTED)

    def test_every_deny_ignore_has_a_reason_and_a_review_date_not_yet_passed(self):
        deny, bare = deny_ignores()
        self.assertEqual(bare, [], "deny.toml ignores must be tables with an id and a reason")
        today = datetime.date.today()
        for advisory, reason in deny.items():
            match = REVIEW.search(reason)
            self.assertIsNotNone(match, "%s: the reason must say 'review by YYYY-MM-DD'" % advisory)
            self.assertGreaterEqual(datetime.date.fromisoformat(match.group(1)), today, "%s: review date has passed" % advisory)


if __name__ == "__main__":
    unittest.main()
