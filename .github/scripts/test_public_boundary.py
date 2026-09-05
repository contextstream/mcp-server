#!/usr/bin/env python3

from __future__ import annotations

import importlib.util
from pathlib import Path
import tempfile
import unittest


SCRIPT = Path(__file__).with_name("public_boundary.py")
SPEC = importlib.util.spec_from_file_location("public_boundary", SCRIPT)
assert SPEC is not None and SPEC.loader is not None
boundary = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(boundary)
REPOSITORY_ROOT = Path(__file__).resolve().parents[2]


class PublicBoundaryTest(unittest.TestCase):
    def test_repository_satisfies_public_boundary(self) -> None:
        self.assertEqual(boundary.verify(REPOSITORY_ROOT), "1.0.3")

    def test_forbidden_private_source_is_detected(self) -> None:
        with tempfile.TemporaryDirectory(prefix="public-boundary-") as temporary:
            root = Path(temporary)
            source = root / "leak.rs"
            source.write_text('const URI: &str = "MONGODB_ATLAS_URI";\n', encoding="utf-8")
            with self.assertRaisesRegex(boundary.BoundaryError, "private operator connection"):
                boundary.verify_paths_and_source(root)

    def test_excluded_directory_is_detected(self) -> None:
        with tempfile.TemporaryDirectory(prefix="public-boundary-") as temporary:
            root = Path(temporary)
            (root / "deploy").mkdir()
            with self.assertRaisesRegex(boundary.BoundaryError, "Excluded top-level"):
                boundary.verify_paths_and_source(root)

    def test_developer_checkout_and_private_fixture_are_detected(self) -> None:
        with tempfile.TemporaryDirectory(prefix="public-boundary-") as temporary:
            root = Path(temporary)
            source = root / "leak.rs"
            source.write_text(
                'const PATH: &str = "/home/developer/dev/maker/contextadmin";\n',
                encoding="utf-8",
            )
            with self.assertRaisesRegex(boundary.BoundaryError, "developer checkout layout"):
                boundary.verify_paths_and_source(root)

    def test_private_identifier_in_documentation_is_detected(self) -> None:
        with tempfile.TemporaryDirectory(prefix="public-boundary-") as temporary:
            root = Path(temporary)
            source = root / "architecture.md"
            source.write_text(
                "Clone github.com/contextstream/contextadmin for the backend.\n",
                encoding="utf-8",
            )
            with self.assertRaisesRegex(boundary.BoundaryError, "private repository fixture"):
                boundary.verify_paths_and_source(root)

    def test_unknown_uuid_literal_is_detected(self) -> None:
        with tempfile.TemporaryDirectory(prefix="public-boundary-") as temporary:
            root = Path(temporary)
            source = root / "leak.rs"
            source.write_text(
                'const ID: &str = "123e4567-e89b-42d3-a456-426614174000";\n',
                encoding="utf-8",
            )
            with self.assertRaisesRegex(boundary.BoundaryError, "non-documentation UUID"):
                boundary.verify_paths_and_source(root)


if __name__ == "__main__":
    unittest.main()
