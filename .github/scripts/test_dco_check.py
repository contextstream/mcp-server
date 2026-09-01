#!/usr/bin/env python3

from __future__ import annotations

import unittest

from dco_check import matching_signoff


class MatchingSignoffTest(unittest.TestCase):
    def test_accepts_matching_author_trailer(self) -> None:
        self.assertTrue(
            matching_signoff(
                "Ada Lovelace",
                "ada@example.com",
                "Implement engine\n\nSigned-off-by: Ada Lovelace <ada@example.com>\n",
            )
        )

    def test_name_and_email_comparison_is_case_insensitive(self) -> None:
        self.assertTrue(
            matching_signoff(
                "Ada Lovelace",
                "ADA@EXAMPLE.COM",
                "Signed-off-by: ada lovelace <ada@example.com>",
            )
        )

    def test_rejects_missing_trailer(self) -> None:
        self.assertFalse(matching_signoff("Ada", "ada@example.com", "No trailer"))

    def test_rejects_malformed_trailer(self) -> None:
        self.assertFalse(
            matching_signoff(
                "Ada",
                "ada@example.com",
                "Signed-off-by: Ada ada@example.com",
            )
        )

    def test_rejects_trailer_for_another_author(self) -> None:
        self.assertFalse(
            matching_signoff(
                "Ada",
                "ada@example.com",
                "Signed-off-by: Grace <grace@example.com>",
            )
        )

    def test_accepts_matching_trailer_among_multiple_signoffs(self) -> None:
        message = (
            "Signed-off-by: Grace <grace@example.com>\n"
            "Signed-off-by: Ada <ada@example.com>\n"
        )
        self.assertTrue(matching_signoff("Ada", "ada@example.com", message))


if __name__ == "__main__":
    unittest.main()
