#!/usr/bin/env python3
"""Fail-closed invariants for public ContextStream MCP releases."""

from __future__ import annotations

import argparse
from datetime import datetime, timezone
import hashlib
import json
from pathlib import Path
import re
import stat
import subprocess
import sys
from typing import Iterable

import tomllib


PROGRAM_NAME = "contextstream-mcp"
REPOSITORY_URL = "https://github.com/contextstream/mcp-server"
NPM_PACKAGE = "@contextstream/mcp-server"
MCP_NAME = "io.github.contextstream/mcp-server"
REMOTE_URL = "https://mcp.contextstream.io/mcp"

MANIFEST_FILES = {
    "linux-x64": "contextstream-mcp-linux-x64",
    "linux-x64-remote-acceleration": (
        "contextstream-mcp-linux-x64-remote-acceleration"
    ),
    "linux-arm64": "contextstream-mcp-linux-arm64",
    "darwin-x64": "contextstream-mcp-darwin-x64",
    "darwin-arm64": "contextstream-mcp-darwin-arm64",
    "win-x64": "contextstream-mcp-win-x64.exe",
}
RELEASE_ARTIFACTS = tuple(MANIFEST_FILES.values())
SBOM_FILE = "sbom.spdx.json"
VERSION_FILE = "version.json"
CHECKSUM_FILE = "checksums.txt"
CHECKSUM_TARGETS = (*RELEASE_ARTIFACTS, SBOM_FILE, VERSION_FILE)
PAYLOAD_FILES = (*CHECKSUM_TARGETS, CHECKSUM_FILE)

STABLE_SEMVER = re.compile(
    r"^(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)$"
)
OBJECT_ID = re.compile(r"^[0-9a-f]{40,64}$")
CHECKSUM_LINE = re.compile(r"^([0-9a-f]{64})  ([A-Za-z0-9._-]+)$")


class ReleaseError(RuntimeError):
    """A public release invariant was not proven."""


def canonical_stable_semver(version: str, label: str = "version") -> tuple[int, int, int]:
    match = STABLE_SEMVER.fullmatch(version)
    if match is None:
        raise ReleaseError(
            f"{label} must be canonical stable SemVer (MAJOR.MINOR.PATCH); "
            f"got {version!r}."
        )
    return tuple(int(part) for part in match.groups())  # type: ignore[return-value]


def _read_json(path: Path) -> dict[str, object]:
    try:
        value = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, UnicodeError, json.JSONDecodeError) as error:
        raise ReleaseError(f"Could not parse {path}: {error}") from error
    if not isinstance(value, dict):
        raise ReleaseError(f"{path} must contain a JSON object.")
    return value


def cargo_workspace_version(path: Path) -> str:
    try:
        manifest = tomllib.loads(path.read_text(encoding="utf-8"))
        version = manifest["workspace"]["package"]["version"]
    except (OSError, UnicodeError, KeyError, tomllib.TOMLDecodeError) as error:
        raise ReleaseError(f"Could not read workspace version from {path}: {error}") from error
    if not isinstance(version, str):
        raise ReleaseError("Cargo workspace version must be a string.")
    canonical_stable_semver(version, "Cargo workspace version")
    return version


def verify_repository_metadata(root: Path) -> str:
    """Require one identity across Cargo, npm, and MCP Registry metadata."""
    version = cargo_workspace_version(root / "Cargo.toml")
    package = _read_json(root / "package.json")
    server = _read_json(root / "server.json")

    if package.get("name") != NPM_PACKAGE:
        raise ReleaseError(f"package.json name must be {NPM_PACKAGE!r}.")
    if package.get("mcpName") != MCP_NAME:
        raise ReleaseError(f"package.json mcpName must be {MCP_NAME!r}.")
    if package.get("version") != version:
        raise ReleaseError("Cargo and npm versions do not match.")
    if package.get("bin") != {
        "mcp-server": "npm/bin/mcp-server.mjs",
        "contextstream-mcp": "npm/bin/contextstream-mcp.mjs",
        "contextstream-hook": "npm/bin/contextstream-hook.mjs",
    }:
        raise ReleaseError("package.json must preserve all three 1.x executable aliases.")

    if server.get("name") != MCP_NAME or server.get("version") != version:
        raise ReleaseError("server.json name/version do not match the public identity.")
    repository = server.get("repository")
    if not isinstance(repository, dict) or repository.get("url") != REPOSITORY_URL:
        raise ReleaseError("server.json repository does not name the canonical public repo.")
    remotes = server.get("remotes")
    if remotes != [{"type": "streamable-http", "url": REMOTE_URL}]:
        raise ReleaseError("server.json must declare the canonical remote transport.")
    packages = server.get("packages")
    if not isinstance(packages, list) or len(packages) != 1:
        raise ReleaseError("server.json must declare exactly one npm package.")
    registry_package = packages[0]
    if not isinstance(registry_package, dict):
        raise ReleaseError("server.json package entry must be an object.")
    if (
        registry_package.get("registryType") != "npm"
        or registry_package.get("identifier") != NPM_PACKAGE
        or registry_package.get("version") != version
        or registry_package.get("transport") != {"type": "stdio"}
    ):
        raise ReleaseError("server.json npm package metadata is inconsistent.")
    return version


def _normalize_registry_value(value: object) -> object:
    """Normalize boolean defaults the Registry may omit when serializing."""
    if isinstance(value, dict):
        return {
            key: _normalize_registry_value(item)
            for key, item in value.items()
            if not (key in {"isRequired", "isSecret"} and item is False)
        }
    if isinstance(value, list):
        return [_normalize_registry_value(item) for item in value]
    return value


def verify_registry_response(server_path: Path, response_path: Path) -> None:
    """Require one identical, active MCP Registry version on release resume."""
    expected = _read_json(server_path)
    payload = _read_json(response_path)
    actual = payload.get("server")
    if not isinstance(actual, dict):
        raise ReleaseError("MCP Registry response does not contain a server object.")
    if _normalize_registry_value(actual) != _normalize_registry_value(expected):
        raise ReleaseError(
            "MCP Registry already contains different metadata for this immutable version."
        )
    metadata = payload.get("_meta")
    if not isinstance(metadata, dict):
        raise ReleaseError("MCP Registry response does not contain official metadata.")
    official = metadata.get("io.modelcontextprotocol.registry/official")
    if not isinstance(official, dict) or official.get("status") != "active":
        status = official.get("status") if isinstance(official, dict) else None
        raise ReleaseError(f"MCP Registry version is not active: {status!r}.")


def resolve_version(root: Path, event_name: str, git_ref: str) -> str:
    version = verify_repository_metadata(root)
    if event_name == "push":
        expected = f"refs/tags/v{version}"
        if git_ref != expected:
            raise ReleaseError(f"Release ref {git_ref!r} must equal {expected!r}.")
    elif event_name == "workflow_dispatch":
        if git_ref != "refs/heads/main":
            raise ReleaseError("Manual release validation is allowed only from main.")
    else:
        raise ReleaseError(f"Unsupported release event: {event_name!r}.")
    return version


def _git(repository: Path, *args: str, check: bool = True) -> subprocess.CompletedProcess[str]:
    try:
        result = subprocess.run(
            ["git", "-C", str(repository), *args],
            check=False,
            capture_output=True,
            text=True,
            encoding="utf-8",
            errors="strict",
            timeout=30,
        )
    except (OSError, subprocess.SubprocessError, UnicodeError) as error:
        raise ReleaseError(f"Could not inspect release source: {error}") from error
    if check and result.returncode != 0:
        detail = result.stderr.strip() or result.stdout.strip()
        raise ReleaseError(f"git {args!r} failed: {detail}")
    return result


def release_source_epoch(repository: Path, event_name: str, github_sha: str) -> int:
    normalized_sha = github_sha.lower()
    if not OBJECT_ID.fullmatch(normalized_sha):
        raise ReleaseError(f"GITHUB_SHA is not a canonical object id: {github_sha!r}.")
    head = _git(repository, "rev-parse", "--verify", "HEAD^{commit}").stdout.strip()
    source = _git(
        repository, "rev-parse", "--verify", f"{normalized_sha}^{{commit}}"
    ).stdout.strip()
    if head != source:
        raise ReleaseError(f"Checked-out HEAD {head} does not match GITHUB_SHA {source}.")

    main = _git(
        repository, "rev-parse", "--verify", "refs/remotes/origin/main^{commit}"
    ).stdout.strip()
    if event_name == "push":
        ancestry = _git(
            repository, "merge-base", "--is-ancestor", source, main, check=False
        )
        if ancestry.returncode != 0:
            raise ReleaseError("Tagged release commit is not an ancestor of origin/main.")
    elif event_name == "workflow_dispatch":
        if source != main:
            raise ReleaseError("Manual validation must run at the current origin/main commit.")
    else:
        raise ReleaseError(f"Unsupported release event: {event_name!r}.")

    raw = _git(repository, "show", "-s", "--format=%ct", source).stdout.strip()
    try:
        epoch = int(raw)
        if epoch <= 0:
            raise ValueError("not positive")
        datetime.fromtimestamp(epoch, timezone.utc)
    except (OSError, OverflowError, ValueError) as error:
        raise ReleaseError(f"Invalid source epoch {raw!r}.") from error
    return epoch


def verify_remote_tag(repository: Path, tag: str, expected_object: str) -> str:
    if not tag.startswith("v"):
        raise ReleaseError("Release tag must start with v.")
    canonical_stable_semver(tag[1:], "tag version")
    expected = expected_object.lower()
    if not OBJECT_ID.fullmatch(expected):
        raise ReleaseError("Expected release object is not a canonical object id.")
    expected_commit = _git(
        repository, "rev-parse", "--verify", f"{expected}^{{commit}}"
    ).stdout.strip()

    tag_ref = f"refs/tags/{tag}"
    peeled_ref = f"{tag_ref}^{{}}"
    remote = _git(
        repository,
        "ls-remote",
        "--exit-code",
        "origin",
        tag_ref,
        peeled_ref,
    )
    refs: dict[str, str] = {}
    for line in remote.stdout.splitlines():
        fields = line.split()
        if len(fields) != 2 or fields[1] not in {tag_ref, peeled_ref}:
            raise ReleaseError(f"Malformed remote tag row: {line!r}.")
        object_id = fields[0].lower()
        if not OBJECT_ID.fullmatch(object_id) or fields[1] in refs:
            raise ReleaseError(f"Malformed remote tag row: {line!r}.")
        refs[fields[1]] = object_id
    if set(refs) != {tag_ref, peeled_ref}:
        raise ReleaseError("Release tags must be annotated and remotely visible.")
    if refs[peeled_ref] != expected_commit:
        raise ReleaseError("Remote release tag does not peel to the expected commit.")
    if refs[tag_ref] == refs[peeled_ref]:
        raise ReleaseError("Release tag must use a distinct annotated tag object.")
    return expected_commit


def stable_version_relation(candidate: str, current: str) -> str:
    candidate_parts = canonical_stable_semver(candidate, "candidate version")
    current_parts = canonical_stable_semver(current, "current version")
    if candidate_parts < current_parts:
        return "older"
    if candidate_parts == current_parts:
        return "equal"
    return "newer"


def assert_newer(candidate: str, current: str) -> None:
    if stable_version_relation(candidate, current) != "newer":
        raise ReleaseError(f"v{candidate} would not advance current stable v{current}.")


def _regular_nonempty(path: Path) -> None:
    try:
        details = path.lstat()
    except OSError as error:
        raise ReleaseError(f"Required release file is missing: {path.name}.") from error
    if not stat.S_ISREG(details.st_mode) or path.is_symlink() or details.st_size == 0:
        raise ReleaseError(f"Release file must be a non-empty regular file: {path.name}.")


def sha256(path: Path) -> str:
    digest = hashlib.sha256()
    try:
        with path.open("rb") as handle:
            for chunk in iter(lambda: handle.read(1024 * 1024), b""):
                digest.update(chunk)
    except OSError as error:
        raise ReleaseError(f"Could not hash {path}: {error}") from error
    return digest.hexdigest()


def _canonical_manifest(version: str, source_commit: str) -> dict[str, object]:
    canonical_stable_semver(version)
    normalized_commit = source_commit.lower()
    if not OBJECT_ID.fullmatch(normalized_commit):
        raise ReleaseError("Source commit must be a canonical full object id.")
    return {
        "version": version,
        "source": {"repository": REPOSITORY_URL, "commit": normalized_commit},
        "files": MANIFEST_FILES,
        "sbom": SBOM_FILE,
    }


def _validate_sbom(path: Path) -> None:
    sbom = _read_json(path)
    if not (isinstance(sbom.get("spdxVersion"), str) or sbom.get("bomFormat") == "CycloneDX"):
        raise ReleaseError("SBOM must be SPDX JSON or CycloneDX JSON.")


def generate_release_files(directory: Path, version: str, source_commit: str) -> None:
    directory.mkdir(parents=True, exist_ok=True)
    for name in RELEASE_ARTIFACTS:
        _regular_nonempty(directory / name)
    _regular_nonempty(directory / SBOM_FILE)
    _validate_sbom(directory / SBOM_FILE)

    manifest = _canonical_manifest(version, source_commit)
    (directory / VERSION_FILE).write_text(
        json.dumps(manifest, indent=2, sort_keys=True) + "\n", encoding="utf-8"
    )
    checksums = "".join(
        f"{sha256(directory / name)}  {name}\n" for name in sorted(CHECKSUM_TARGETS)
    )
    (directory / CHECKSUM_FILE).write_text(checksums, encoding="utf-8")


def _parse_checksums(path: Path) -> dict[str, str]:
    try:
        lines = path.read_text(encoding="utf-8").splitlines()
    except (OSError, UnicodeError) as error:
        raise ReleaseError(f"Could not read {path}: {error}") from error
    parsed: dict[str, str] = {}
    for line in lines:
        match = CHECKSUM_LINE.fullmatch(line)
        if match is None or match.group(2) in parsed:
            raise ReleaseError("checksums.txt contains an invalid or duplicate row.")
        parsed[match.group(2)] = match.group(1)
    if set(parsed) != set(CHECKSUM_TARGETS):
        raise ReleaseError("checksums.txt does not cover the exact release payload.")
    return parsed


def verify_release_files(directory: Path, version: str, source_commit: str) -> None:
    canonical = _canonical_manifest(version, source_commit)
    actual_names = {item.name for item in directory.iterdir() if item.is_file()}
    if actual_names != set(PAYLOAD_FILES):
        missing = sorted(set(PAYLOAD_FILES) - actual_names)
        extra = sorted(actual_names - set(PAYLOAD_FILES))
        raise ReleaseError(f"Release payload mismatch; missing={missing}, extra={extra}.")
    for name in PAYLOAD_FILES:
        _regular_nonempty(directory / name)
    if _read_json(directory / VERSION_FILE) != canonical:
        raise ReleaseError("version.json is not the canonical release manifest.")
    _validate_sbom(directory / SBOM_FILE)
    checksums = _parse_checksums(directory / CHECKSUM_FILE)
    for name, expected in checksums.items():
        if sha256(directory / name) != expected:
            raise ReleaseError(f"Checksum mismatch for {name}.")


def verify_binary_version(binary: Path, version: str) -> None:
    canonical_stable_semver(version)
    _regular_nonempty(binary)
    try:
        result = subprocess.run(
            [str(binary), "--version"],
            check=False,
            capture_output=True,
            text=True,
            encoding="utf-8",
            errors="strict",
            timeout=20,
        )
    except (OSError, subprocess.SubprocessError, UnicodeError) as error:
        raise ReleaseError(f"Could not inspect {binary.name}: {error}") from error
    if result.returncode != 0 or result.stdout.strip() != f"{PROGRAM_NAME} {version}":
        raise ReleaseError(f"{binary.name} does not report exact version {version}.")


def _parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(description=__doc__)
    subparsers = parser.add_subparsers(dest="command", required=True)

    metadata = subparsers.add_parser("metadata")
    metadata.add_argument("root", type=Path)

    resolve = subparsers.add_parser("resolve-version")
    resolve.add_argument("root", type=Path)
    resolve.add_argument("event_name")
    resolve.add_argument("git_ref")

    epoch = subparsers.add_parser("source-epoch")
    epoch.add_argument("repository", type=Path)
    epoch.add_argument("event_name")
    epoch.add_argument("github_sha")

    remote_tag = subparsers.add_parser("verify-remote-tag")
    remote_tag.add_argument("repository", type=Path)
    remote_tag.add_argument("tag")
    remote_tag.add_argument("expected_object")

    relation = subparsers.add_parser("stable-version-relation")
    relation.add_argument("candidate")
    relation.add_argument("current")

    generate = subparsers.add_parser("generate-release-files")
    generate.add_argument("directory", type=Path)
    generate.add_argument("version")
    generate.add_argument("source_commit")

    verify = subparsers.add_parser("verify-release")
    verify.add_argument("directory", type=Path)
    verify.add_argument("version")
    verify.add_argument("source_commit")

    binary = subparsers.add_parser("verify-binary")
    binary.add_argument("binary", type=Path)
    binary.add_argument("version")

    registry = subparsers.add_parser("verify-registry")
    registry.add_argument("server", type=Path)
    registry.add_argument("response", type=Path)
    return parser


def main(argv: Iterable[str] | None = None) -> int:
    args = _parser().parse_args(argv)
    try:
        if args.command == "metadata":
            print(verify_repository_metadata(args.root))
        elif args.command == "resolve-version":
            print(resolve_version(args.root, args.event_name, args.git_ref))
        elif args.command == "source-epoch":
            print(release_source_epoch(args.repository, args.event_name, args.github_sha))
        elif args.command == "verify-remote-tag":
            print(verify_remote_tag(args.repository, args.tag, args.expected_object))
        elif args.command == "stable-version-relation":
            print(stable_version_relation(args.candidate, args.current))
        elif args.command == "generate-release-files":
            generate_release_files(args.directory, args.version, args.source_commit)
        elif args.command == "verify-release":
            verify_release_files(args.directory, args.version, args.source_commit)
        elif args.command == "verify-binary":
            verify_binary_version(args.binary, args.version)
        elif args.command == "verify-registry":
            verify_registry_response(args.server, args.response)
        else:  # pragma: no cover
            raise ReleaseError(f"Unknown command {args.command!r}.")
    except ReleaseError as error:
        print(f"::error::{error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
