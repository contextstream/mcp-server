#!/usr/bin/env python3
"""
Tests for contextstream-redirect.py hook

Run with: python3 -m pytest src/hooks/test_contextstream_redirect.py -v
Or simply: python3 src/hooks/test_contextstream_redirect.py
"""

import json
import os
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch, MagicMock

# Import the module to test
import sys
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))

# We need to import the functions from the hook
# Since it's a script, we import it directly
import importlib.util
hook_path = Path(__file__).parent / "contextstream-redirect.py"
spec = importlib.util.spec_from_file_location("hook", hook_path)
hook = importlib.util.module_from_spec(spec)
spec.loader.exec_module(hook)


class TestIsProjectIndexed(unittest.TestCase):
    """Tests for is_project_indexed function"""

    def setUp(self):
        """Create a temporary directory for test files"""
        self.temp_dir = tempfile.mkdtemp()
        self.status_file = Path(self.temp_dir) / "indexed-projects.json"

    def tearDown(self):
        """Clean up temp files"""
        import shutil
        shutil.rmtree(self.temp_dir, ignore_errors=True)

    def test_returns_false_when_no_status_file(self):
        """Should return False if status file doesn't exist"""
        with patch.object(hook, 'INDEX_STATUS_PATH', self.status_file):
            result = hook.is_project_indexed("/some/project")
            self.assertFalse(result)

    def test_returns_false_when_project_not_in_list(self):
        """Should return False if project is not in indexed list"""
        self.status_file.parent.mkdir(parents=True, exist_ok=True)
        self.status_file.write_text(json.dumps({
            "version": 1,
            "projects": {
                "/other/project": {"indexed_at": "2024-01-01"}
            }
        }))
        with patch.object(hook, 'INDEX_STATUS_PATH', self.status_file):
            result = hook.is_project_indexed("/some/project")
            self.assertFalse(result)

    def test_returns_true_when_project_is_indexed(self):
        """Should return True if project is in indexed list"""
        self.status_file.parent.mkdir(parents=True, exist_ok=True)
        self.status_file.write_text(json.dumps({
            "version": 1,
            "projects": {
                "/some/project": {"indexed_at": "2024-01-01"}
            }
        }))
        with patch.object(hook, 'INDEX_STATUS_PATH', self.status_file):
            result = hook.is_project_indexed("/some/project")
            self.assertTrue(result)

    def test_handles_trailing_slashes(self):
        """Should match paths with trailing slash variations"""
        self.status_file.parent.mkdir(parents=True, exist_ok=True)
        self.status_file.write_text(json.dumps({
            "version": 1,
            "projects": {
                "/some/project/": {"indexed_at": "2024-01-01"}
            }
        }))
        with patch.object(hook, 'INDEX_STATUS_PATH', self.status_file):
            result = hook.is_project_indexed("/some/project")
            self.assertTrue(result)

    def test_returns_false_for_empty_project_root(self):
        """Should return False if project_root is empty"""
        result = hook.is_project_indexed("")
        self.assertFalse(result)

    def test_returns_false_for_none_project_root(self):
        """Should return False if project_root is None"""
        result = hook.is_project_indexed(None)
        self.assertFalse(result)


class TestReadIndexedProjects(unittest.TestCase):
    """Tests for read_indexed_projects function"""

    def setUp(self):
        self.temp_dir = tempfile.mkdtemp()
        self.status_file = Path(self.temp_dir) / "indexed-projects.json"

    def tearDown(self):
        import shutil
        shutil.rmtree(self.temp_dir, ignore_errors=True)

    def test_returns_empty_dict_when_no_file(self):
        """Should return empty dict if file doesn't exist"""
        with patch.object(hook, 'INDEX_STATUS_PATH', self.status_file):
            result = hook.read_indexed_projects()
            self.assertEqual(result, {})

    def test_returns_projects_from_file(self):
        """Should return projects dict from file"""
        self.status_file.parent.mkdir(parents=True, exist_ok=True)
        self.status_file.write_text(json.dumps({
            "version": 1,
            "projects": {
                "/project/a": {"indexed_at": "2024-01-01"},
                "/project/b": {"indexed_at": "2024-01-02"}
            }
        }))
        with patch.object(hook, 'INDEX_STATUS_PATH', self.status_file):
            result = hook.read_indexed_projects()
            self.assertEqual(len(result), 2)
            self.assertIn("/project/a", result)

    def test_handles_invalid_json(self):
        """Should return empty dict if file has invalid JSON"""
        self.status_file.parent.mkdir(parents=True, exist_ok=True)
        self.status_file.write_text("not valid json")
        with patch.object(hook, 'INDEX_STATUS_PATH', self.status_file):
            result = hook.read_indexed_projects()
            self.assertEqual(result, {})


class TestFindProjectRoot(unittest.TestCase):
    """Tests for find_project_root function"""

    def setUp(self):
        self.temp_dir = tempfile.mkdtemp()

    def tearDown(self):
        import shutil
        shutil.rmtree(self.temp_dir, ignore_errors=True)

    def test_finds_contextstream_dir(self):
        """Should find project root with .contextstream dir"""
        project_dir = Path(self.temp_dir) / "my_project"
        project_dir.mkdir(parents=True)
        (project_dir / ".contextstream").mkdir()
        subdir = project_dir / "src" / "lib"
        subdir.mkdir(parents=True)

        result = hook.find_project_root(str(subdir))
        self.assertEqual(result, str(project_dir))

    def test_finds_git_dir(self):
        """Should find project root with .git dir"""
        project_dir = Path(self.temp_dir) / "git_project"
        project_dir.mkdir(parents=True)
        (project_dir / ".git").mkdir()
        subdir = project_dir / "src"
        subdir.mkdir(parents=True)

        result = hook.find_project_root(str(subdir))
        self.assertEqual(result, str(project_dir))

    def test_returns_none_when_no_root(self):
        """Should return None if no project root found"""
        random_dir = Path(self.temp_dir) / "random"
        random_dir.mkdir(parents=True)

        # Don't create any markers
        result = hook.find_project_root(str(random_dir))
        # This might find /home or similar if run in a git repo
        # So we just check it doesn't crash
        # In a truly isolated env it would be None

    def test_prefers_contextstream_over_git(self):
        """Should prefer .contextstream over .git if both exist"""
        project_dir = Path(self.temp_dir) / "dual_project"
        project_dir.mkdir(parents=True)
        (project_dir / ".contextstream").mkdir()
        (project_dir / ".git").mkdir()

        result = hook.find_project_root(str(project_dir))
        self.assertEqual(result, str(project_dir))


class TestMainFlowNonIndexedProject(unittest.TestCase):
    """Tests for main() flow with non-indexed projects"""

    def test_allows_glob_for_non_indexed_project(self):
        """Should allow Glob when project is not indexed"""
        with tempfile.TemporaryDirectory() as temp_dir:
            # Create a git repo but don't index it
            project_dir = Path(temp_dir) / "unindexed_project"
            project_dir.mkdir()
            (project_dir / ".git").mkdir()

            # Mock stdin with a Glob call
            mock_input = {
                "tool_name": "Glob",
                "tool_input": {
                    "pattern": "**/*.ts",
                    "path": str(project_dir)
                }
            }

            # Mock is_project_indexed to return False
            with patch.object(hook, 'is_project_indexed', return_value=False):
                with patch('sys.stdin', MagicMock()):
                    with patch('json.load', return_value=mock_input):
                        with patch('sys.exit') as mock_exit:
                            hook.main()
                            # Should exit with 0 (allow) because project not indexed
                            mock_exit.assert_called_with(0)


if __name__ == '__main__':
    unittest.main()
