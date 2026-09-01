#!/usr/bin/env python3
"""Enforce the source, package, and release boundary of the public repository."""

from __future__ import annotations

import argparse
import importlib.util
from pathlib import Path
import re
import sys
import tomllib


EXPECTED_MEMBERS = (
    "crates/mcp-server",
    "crates/mcp-tools",
    "crates/mcp-client",
    "crates/mcp-session",
    "crates/mcp-types",
    "crates/mcp-model-registry",
    "crates/mcp-acceleration-products",
)
EXCLUDED_TOP_LEVEL = {
    ".antigravity",
    ".cursor",
    "deploy",
    "eval",
    "infra",
    "k8s",
    "pulumi",
}
SOURCE_SUFFIXES = {
    ".json",
    ".js",
    ".lock",
    ".md",
    ".mjs",
    ".ps1",
    ".py",
    ".rs",
    ".sh",
    ".toml",
    ".txt",
    ".yaml",
    ".yml",
}
FORBIDDEN_SOURCE_PATTERNS = {
    "excluded Atlas crate": re.compile(r"mcp[-_]atlas[-_]products"),
    "excluded Atlas feature": re.compile(r"atlas-products"),
    "private operator connection": re.compile(r"MONGODB_ATLAS_URI"),
    "private operator project": re.compile(r"ATLAS_(?:PROJECT|ORG)_ID"),
    "private deployment dispatch": re.compile(r"repository_dispatch"),
    "private canonical-source URL": re.compile(
        r"github\.com/contextstream/mcp(?:\.git)?(?:[/'\"]|$)"
    ),
    "private repository fixture": re.compile(
        r"(?:github\.com/contextstream/(?:contextstream|contextadmin)|"
        r"(?:crates|maker)[\\/]+contextstream-api(?:[\\/]+|$)|"
        r"\bcontextadmin\b|\bstreampilot-sp-remote\b)",
        re.IGNORECASE,
    ),
    "developer checkout layout": re.compile(
        r"(?:/(?:home/[^/\s]+|data)/dev/maker(?:/|$)|"
        r"/Users/[^/\s]+/dev/maker(?:/|$)|"
        r"[A-Za-z]:[\\/]+Users[\\/]+[^\\/\s]+[\\/]+dev[\\/]+maker(?:[\\/]+|$))"
    ),
    "internal implementation plan id": re.compile(
        r"\b(?:plan|Plans?)\s+[0-9a-f]{8}\b", re.IGNORECASE
    ),
}
UUID_LITERAL = re.compile(
    r"\b[0-9a-f]{8}-[0-9a-f]{4}-[1-5][0-9a-f]{3}-"
    r"[89ab][0-9a-f]{3}-[0-9a-f]{12}\b",
    re.IGNORECASE,
)
ALLOWED_UUID_LITERALS = {
    # Synthetic high-entropy fixtures for realistic tokenizer coverage.
    "17ab6543-59d3-4a97-a57e-6add460d98ae",
    "fe106dc3-6903-4d62-b0b3-c33d33f19f71",
    "00000000-0000-4000-8000-000000000042",
    "00000000-0000-4000-8000-000000000062",
    "01234567-89ab-4cde-8fab-0123456789ab",
    "10101010-1010-4010-8010-101010101010",
    "11111111-1111-4111-8111-111111111111",
    "11111111-2222-4333-8444-555555555555",
    "20202020-2020-4020-8020-202020202020",
    "22222222-2222-4222-8222-222222222222",
    "30303030-3030-4030-8030-303030303030",
    "33333333-3333-4333-8333-333333333333",
    "40404040-4040-4040-8040-404040404040",
    "44444444-4444-4444-8444-444444444444",
    "50505050-5050-4050-8050-505050505050",
    "550e8400-e29b-41d4-a716-446655440000",
    "550e8400-e29b-41d4-a716-446655440001",
    "550e8400-e29b-41d4-a716-446655440002",
    "55555555-5555-4555-8555-555555555555",
    "60606060-6060-4060-8060-606060606060",
    "650e8400-e29b-41d4-a716-446655440001",
    "660e8400-e29b-41d4-a716-446655440000",
    "66666666-6666-4666-8666-666666666666",
    "6ba7b810-9dad-11d1-80b4-00c04fd430c8",
    "70707070-7070-4070-8070-707070707070",
    "77777777-7777-4777-8777-777777777777",
    "80808080-8080-4080-8080-808080808080",
    "88888888-8888-4888-8888-888888888888",
    "90909090-9090-4090-8090-909090909090",
    "99999999-9999-4999-8999-999999999999",
    "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa",
    "aaaaaaaa-bbbb-4ccc-8ddd-eeeeeeeeeeee",
    "bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb",
    "cccccccc-cccc-4ccc-8ccc-cccccccccccc",
    "dddddddd-dddd-4ddd-8ddd-dddddddddddd",
    "eeeeeeee-eeee-4eee-8eee-eeeeeeeeeeee",
    "ffffffff-ffff-4fff-bfff-ffffffffffff",
}


class BoundaryError(RuntimeError):
    pass


def _toml(path: Path) -> dict[str, object]:
    try:
        return tomllib.loads(path.read_text(encoding="utf-8"))
    except (OSError, UnicodeError, tomllib.TOMLDecodeError) as error:
        raise BoundaryError(f"Could not parse {path}: {error}") from error


def _release_contract(root: Path):
    path = root / ".github" / "scripts" / "release_contract.py"
    spec = importlib.util.spec_from_file_location("public_release_contract", path)
    if spec is None or spec.loader is None:
        raise BoundaryError("Could not load the public release contract.")
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def _source_files(root: Path):
    ignored = {".git", "node_modules", "target", "__pycache__"}
    for path in root.rglob("*"):
        if any(part in ignored for part in path.parts) or not path.is_file():
            continue
        if path.name in {"public_boundary.py", "test_public_boundary.py", "test_workflows.py"}:
            continue
        if path.suffix in SOURCE_SUFFIXES or path.name in {"Dockerfile.http-gateway"}:
            yield path


def verify_workspace(root: Path) -> None:
    cargo = _toml(root / "Cargo.toml")
    workspace = cargo.get("workspace")
    if not isinstance(workspace, dict):
        raise BoundaryError("Cargo.toml must define a workspace.")
    if tuple(workspace.get("members", ())) != EXPECTED_MEMBERS:
        raise BoundaryError("Cargo workspace members do not match the public allowlist.")
    package_defaults = workspace.get("package")
    if not isinstance(package_defaults, dict) or package_defaults.get("publish") is not False:
        raise BoundaryError("Public workspace crates must default to publish=false.")

    dependencies = workspace.get("dependencies")
    if not isinstance(dependencies, dict):
        raise BoundaryError("Cargo workspace dependencies are missing.")
    workspace_version = package_defaults.get("version")
    for name in EXPECTED_MEMBERS:
        manifest = _toml(root / name / "Cargo.toml")
        package = manifest.get("package")
        if not isinstance(package, dict):
            raise BoundaryError(f"{name}/Cargo.toml has no package table.")
        for inherited in ("version", "license", "repository", "publish"):
            if package.get(inherited) != {"workspace": True}:
                raise BoundaryError(f"{name} must inherit package.{inherited} from the workspace.")

    for dependency_name, dependency in dependencies.items():
        if not dependency_name.startswith("mcp-"):
            continue
        if not isinstance(dependency, dict):
            raise BoundaryError(f"Workspace crate dependency {dependency_name} must be explicit.")
        if dependency.get("version") != f"={workspace_version}":
            raise BoundaryError(f"Workspace crate dependency {dependency_name} must pin ={workspace_version}.")
        expected_path = f"crates/{dependency_name}"
        if dependency.get("path") != expected_path or expected_path not in EXPECTED_MEMBERS:
            raise BoundaryError(f"Workspace crate dependency {dependency_name} is outside the allowlist.")


def verify_paths_and_source(root: Path) -> None:
    present_excluded = sorted(name for name in EXCLUDED_TOP_LEVEL if (root / name).exists())
    if present_excluded:
        raise BoundaryError(f"Excluded top-level paths are present: {present_excluded}.")
    if (root / "crates" / "mcp-atlas-products").exists():
        raise BoundaryError("The excluded Atlas provider crate is present.")

    violations: list[str] = []
    for path in _source_files(root):
        relative = path.relative_to(root)
        try:
            contents = path.read_text(encoding="utf-8")
        except (OSError, UnicodeError) as error:
            raise BoundaryError(f"Could not inspect {relative}: {error}") from error
        for label, pattern in FORBIDDEN_SOURCE_PATTERNS.items():
            if pattern.search(contents):
                violations.append(f"{relative}: {label}")
        unknown_uuids = sorted(
            {
                match.group(0).lower()
                for match in UUID_LITERAL.finditer(contents)
                if match.group(0).lower() not in ALLOWED_UUID_LITERALS
            }
        )
        if unknown_uuids:
            violations.append(
                f"{relative}: non-documentation UUID literal(s) {unknown_uuids}"
            )
    if violations:
        raise BoundaryError("Public-boundary violations:\n  " + "\n  ".join(sorted(violations)))


def verify(root: Path) -> str:
    if not root.is_dir():
        raise BoundaryError(f"Repository root does not exist: {root}.")
    verify_workspace(root)
    verify_paths_and_source(root)
    try:
        return _release_contract(root).verify_repository_metadata(root)
    except Exception as error:
        if isinstance(error, BoundaryError):
            raise
        raise BoundaryError(str(error)) from error


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("root", nargs="?", type=Path, default=Path.cwd())
    args = parser.parse_args()
    try:
        version = verify(args.root.resolve())
    except BoundaryError as error:
        print(f"::error::{error}", file=sys.stderr)
        return 1
    print(f"Public boundary verified for v{version}.")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
