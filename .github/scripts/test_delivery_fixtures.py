"""CLI fixtures must not embed strings that look like issued workspace keys."""
from pathlib import Path
import re
import unittest


class DeliveryFixtures(unittest.TestCase):
    def test_no_literal_workspace_keys_in_cli_fixtures(self):
        root = Path(__file__).resolve().parents[2]
        fixture = root / "crates/mcp-server/tests/run_deliveries.rs"
        self.assertEqual(
            [n for n, line in enumerate(fixture.read_text().splitlines(), 1)
             if re.search(r'"cbiq_ws_[0-9a-f]{16,}"', line)],
            [],
        )
