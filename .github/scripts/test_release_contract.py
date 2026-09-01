#!/usr/bin/env python3

from __future__ import annotations

import json
import os
from pathlib import Path
import stat
import subprocess
import tempfile
import unittest
from unittest import mock

import release_contract as contract


class ReleaseContractTest(unittest.TestCase):
    def setUp(self) -> None:
        self.temporary = tempfile.TemporaryDirectory(prefix="release-contract-")
        self.root = Path(self.temporary.name)
        self.commit = "a" * 40

    def tearDown(self) -> None:
        self.temporary.cleanup()

    def write_repository_metadata(self, version: str = "1.2.3") -> None:
        (self.root / "Cargo.toml").write_text(
            (
                "[workspace]\n"
                'members = ["crate"]\n\n'
                "[workspace.package]\n"
                f'version = "{version}"\n'
                'edition = "2021"\n'
            ),
            encoding="utf-8",
        )
        (self.root / "package.json").write_text(
            json.dumps(
                {
                    "name": contract.NPM_PACKAGE,
                    "mcpName": contract.MCP_NAME,
                    "version": version,
                    "bin": {
                        "mcp-server": "npm/bin/mcp-server.mjs",
                        "contextstream-mcp": "npm/bin/contextstream-mcp.mjs",
                        "contextstream-hook": "npm/bin/contextstream-hook.mjs",
                    },
                }
            ),
            encoding="utf-8",
        )
        (self.root / "server.json").write_text(
            json.dumps(
                {
                    "name": contract.MCP_NAME,
                    "version": version,
                    "repository": {"url": contract.REPOSITORY_URL, "source": "github"},
                    "remotes": [{"type": "streamable-http", "url": contract.REMOTE_URL}],
                    "packages": [
                        {
                            "registryType": "npm",
                            "identifier": contract.NPM_PACKAGE,
                            "version": version,
                            "transport": {"type": "stdio"},
                        }
                    ],
                }
            ),
            encoding="utf-8",
        )

    def create_release_directory(self, version: str = "1.2.3") -> Path:
        release = self.root / "release"
        release.mkdir()
        for name in contract.RELEASE_ARTIFACTS:
            (release / name).write_bytes(f"binary:{name}\n".encode())
        (release / contract.SBOM_FILE).write_text(
            json.dumps({"spdxVersion": "SPDX-2.3", "name": "contextstream-mcp"}),
            encoding="utf-8",
        )
        contract.generate_release_files(release, version, self.commit)
        return release

    def git(self, repository: Path, *args: str, env: dict[str, str] | None = None) -> str:
        result = subprocess.run(
            ["git", "-C", str(repository), *args],
            check=True,
            capture_output=True,
            text=True,
            env=env,
        )
        return result.stdout.strip()

    def test_semver_and_ordering_are_strict(self) -> None:
        self.assertEqual(contract.canonical_stable_semver("1.2.3"), (1, 2, 3))
        self.assertEqual(contract.stable_version_relation("1.2.2", "1.2.3"), "older")
        self.assertEqual(contract.stable_version_relation("1.2.3", "1.2.3"), "equal")
        self.assertEqual(contract.stable_version_relation("1.2.4", "1.2.3"), "newer")
        contract.assert_newer("2.0.0", "1.999.999")
        for invalid in ("v1.2.3", "1.2", "01.2.3", "1.2.3-rc.1", "1.2.3+build", ""):
            with self.subTest(invalid=invalid), self.assertRaises(contract.ReleaseError):
                contract.canonical_stable_semver(invalid)
        for candidate in ("1.2.3", "1.2.2"):
            with self.assertRaises(contract.ReleaseError):
                contract.assert_newer(candidate, "1.2.3")

    def test_metadata_requires_one_version_and_all_aliases(self) -> None:
        self.write_repository_metadata()
        self.assertEqual(contract.verify_repository_metadata(self.root), "1.2.3")
        package_path = self.root / "package.json"
        package = json.loads(package_path.read_text(encoding="utf-8"))
        del package["bin"]["contextstream-hook"]
        package_path.write_text(json.dumps(package), encoding="utf-8")
        with self.assertRaisesRegex(contract.ReleaseError, "three 1.x executable aliases"):
            contract.verify_repository_metadata(self.root)

    def test_metadata_rejects_remote_or_package_drift(self) -> None:
        self.write_repository_metadata()
        server_path = self.root / "server.json"
        server = json.loads(server_path.read_text(encoding="utf-8"))
        server["packages"][0]["version"] = "1.2.4"
        server_path.write_text(json.dumps(server), encoding="utf-8")
        with self.assertRaises(contract.ReleaseError):
            contract.verify_repository_metadata(self.root)

    def test_registry_response_must_be_identical_and_active(self) -> None:
        self.write_repository_metadata()
        server_path = self.root / "server.json"
        expected = json.loads(server_path.read_text(encoding="utf-8"))
        expected["packages"][0]["environmentVariables"] = [
            {"name": "OPTIONAL", "isRequired": False}
        ]
        server_path.write_text(json.dumps(expected), encoding="utf-8")

        actual = json.loads(json.dumps(expected))
        del actual["packages"][0]["environmentVariables"][0]["isRequired"]
        response_path = self.root / "registry-response.json"
        response_path.write_text(
            json.dumps(
                {
                    "server": actual,
                    "_meta": {
                        "io.modelcontextprotocol.registry/official": {
                            "status": "active"
                        }
                    },
                }
            ),
            encoding="utf-8",
        )
        contract.verify_registry_response(server_path, response_path)

        actual["description"] = "different"
        response_path.write_text(
            json.dumps(
                {
                    "server": actual,
                    "_meta": {
                        "io.modelcontextprotocol.registry/official": {
                            "status": "active"
                        }
                    },
                }
            ),
            encoding="utf-8",
        )
        with self.assertRaisesRegex(contract.ReleaseError, "different metadata"):
            contract.verify_registry_response(server_path, response_path)

    def test_registry_response_rejects_non_active_status(self) -> None:
        self.write_repository_metadata()
        server_path = self.root / "server.json"
        response_path = self.root / "registry-response.json"
        response_path.write_text(
            json.dumps(
                {
                    "server": json.loads(server_path.read_text(encoding="utf-8")),
                    "_meta": {
                        "io.modelcontextprotocol.registry/official": {
                            "status": "deprecated"
                        }
                    },
                }
            ),
            encoding="utf-8",
        )
        with self.assertRaisesRegex(contract.ReleaseError, "not active"):
            contract.verify_registry_response(server_path, response_path)

    def test_tag_and_manual_identity(self) -> None:
        self.write_repository_metadata()
        self.assertEqual(
            contract.resolve_version(self.root, "push", "refs/tags/v1.2.3"), "1.2.3"
        )
        self.assertEqual(
            contract.resolve_version(self.root, "workflow_dispatch", "refs/heads/main"),
            "1.2.3",
        )
        for event, ref in (("push", "refs/tags/v1.2.4"), ("workflow_dispatch", "refs/heads/topic")):
            with self.subTest(event=event, ref=ref), self.assertRaises(contract.ReleaseError):
                contract.resolve_version(self.root, event, ref)

    def test_release_payload_is_exact_and_self_verifying(self) -> None:
        release = self.create_release_directory()
        contract.verify_release_files(release, "1.2.3", self.commit)
        self.assertEqual({item.name for item in release.iterdir()}, set(contract.PAYLOAD_FILES))
        manifest = json.loads((release / contract.VERSION_FILE).read_text(encoding="utf-8"))
        self.assertEqual(manifest["source"]["commit"], self.commit)
        self.assertEqual(manifest["files"], contract.MANIFEST_FILES)

    def test_release_payload_rejects_tampering_and_extras(self) -> None:
        release = self.create_release_directory()
        target = release / contract.RELEASE_ARTIFACTS[0]
        target.write_bytes(target.read_bytes() + b"tamper")
        with self.assertRaisesRegex(contract.ReleaseError, "Checksum mismatch"):
            contract.verify_release_files(release, "1.2.3", self.commit)

        release = self.root / "release-extra"
        release.mkdir()
        for name in contract.RELEASE_ARTIFACTS:
            (release / name).write_bytes(b"binary")
        (release / contract.SBOM_FILE).write_text(
            json.dumps({"spdxVersion": "SPDX-2.3"}), encoding="utf-8"
        )
        contract.generate_release_files(release, "1.2.3", self.commit)
        (release / "unexpected.txt").write_text("no", encoding="utf-8")
        with self.assertRaisesRegex(contract.ReleaseError, "payload mismatch"):
            contract.verify_release_files(release, "1.2.3", self.commit)

    def test_sbom_must_be_machine_readable(self) -> None:
        release = self.root / "bad-sbom"
        release.mkdir()
        for name in contract.RELEASE_ARTIFACTS:
            (release / name).write_bytes(b"binary")
        (release / contract.SBOM_FILE).write_text("{}", encoding="utf-8")
        with self.assertRaisesRegex(contract.ReleaseError, "SBOM"):
            contract.generate_release_files(release, "1.2.3", self.commit)

    def test_binary_version_is_exact(self) -> None:
        binary = self.root / "contextstream-mcp"
        binary.write_text(
            "#!/usr/bin/env sh\nprintf 'contextstream-mcp 1.2.3\\n'\n",
            encoding="utf-8",
        )
        binary.chmod(binary.stat().st_mode | stat.S_IXUSR)
        contract.verify_binary_version(binary, "1.2.3")
        with self.assertRaises(contract.ReleaseError):
            contract.verify_binary_version(binary, "1.2.4")

    def test_release_source_is_exact_main_history(self) -> None:
        repository = self.root / "source"
        repository.mkdir()
        self.git(repository, "init", "-b", "main")
        self.git(repository, "config", "user.name", "Release Test")
        self.git(repository, "config", "user.email", "release@example.invalid")
        self.git(repository, "config", "commit.gpgsign", "false")
        (repository / "source.txt").write_text("main\n", encoding="utf-8")
        self.git(repository, "add", "source.txt")
        dated = {
            **os.environ,
            "GIT_AUTHOR_DATE": "1700000000 +0000",
            "GIT_COMMITTER_DATE": "1700000000 +0000",
        }
        self.git(repository, "commit", "-m", "main", env=dated)
        main_sha = self.git(repository, "rev-parse", "HEAD")
        self.git(repository, "update-ref", "refs/remotes/origin/main", main_sha)
        self.assertEqual(contract.release_source_epoch(repository, "push", main_sha), 1700000000)
        self.assertEqual(
            contract.release_source_epoch(repository, "workflow_dispatch", main_sha),
            1700000000,
        )

        self.git(repository, "switch", "-c", "side")
        (repository / "source.txt").write_text("side\n", encoding="utf-8")
        self.git(repository, "add", "source.txt")
        self.git(repository, "commit", "-m", "side")
        side_sha = self.git(repository, "rev-parse", "HEAD")
        with self.assertRaises(contract.ReleaseError):
            contract.release_source_epoch(repository, "push", side_sha)
        with self.assertRaises(contract.ReleaseError):
            contract.release_source_epoch(repository, "workflow_dispatch", side_sha)

    def test_remote_tag_must_be_annotated_and_exact(self) -> None:
        expected = "a" * 40
        tag_object = "b" * 40
        resolved = subprocess.CompletedProcess([], 0, stdout=f"{expected}\n", stderr="")
        remote = subprocess.CompletedProcess(
            [],
            0,
            stdout=(
                f"{tag_object}\trefs/tags/v1.2.3\n"
                f"{expected}\trefs/tags/v1.2.3^{{}}\n"
            ),
            stderr="",
        )
        with mock.patch.object(contract, "_git", side_effect=[resolved, remote]):
            self.assertEqual(contract.verify_remote_tag(self.root, "v1.2.3", expected), expected)

        lightweight = subprocess.CompletedProcess(
            [], 0, stdout=f"{expected}\trefs/tags/v1.2.3\n", stderr=""
        )
        with mock.patch.object(contract, "_git", side_effect=[resolved, lightweight]):
            with self.assertRaisesRegex(contract.ReleaseError, "annotated"):
                contract.verify_remote_tag(self.root, "v1.2.3", expected)


if __name__ == "__main__":
    unittest.main()
