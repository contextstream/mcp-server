#!/usr/bin/env python3

from __future__ import annotations

from pathlib import Path
import re
import unittest


ROOT = Path(__file__).resolve().parents[2]
WORKFLOWS = ROOT / ".github" / "workflows"
PINNED_ACTION = re.compile(r"^\s*uses:\s*[^#\s]+@([0-9a-f]{40})\s*(?:#.*)?$")


class WorkflowContractTest(unittest.TestCase):
    def test_every_remote_action_is_pinned_to_a_commit(self) -> None:
        violations: list[str] = []
        for workflow in sorted(WORKFLOWS.glob("*.yml")):
            for line_number, line in enumerate(workflow.read_text(encoding="utf-8").splitlines(), 1):
                if "uses:" not in line or "uses: ./" in line:
                    continue
                if PINNED_ACTION.match(line) is None:
                    violations.append(f"{workflow.name}:{line_number}: {line.strip()}")
        self.assertEqual(violations, [])

    def test_workflows_do_not_encode_private_topology(self) -> None:
        joined = "\n".join(
            path.read_text(encoding="utf-8") for path in sorted(WORKFLOWS.glob("*.yml"))
        )
        for forbidden in (
            "repository_dispatch",
            "harness-canary",
            "mcp-atlas-products",
            "MONGODB_ATLAS_URI",
            "github.com/contextstream/mcp.git",
        ):
            with self.subTest(forbidden=forbidden):
                self.assertNotIn(forbidden, joined)

    def test_ci_enforces_public_quality_and_supply_chain(self) -> None:
        ci = (WORKFLOWS / "ci.yml").read_text(encoding="utf-8")
        for required in (
            "public_boundary.py",
            "cargo fmt --all --check",
            "cargo clippy --locked --workspace --all-targets -- -D warnings",
            "cargo test --locked --workspace --all-targets",
            "cargo audit --deny warnings",
            "cargo deny --locked check",
            "gitleaks git . --redact --log-opts='--all'",
            "npm pack --dry-run",
            "https://static.modelcontextprotocol.io/schemas/2025-12-11/server.schema.json",
            "3fba09590c99f61735d234822279f4223fab9e300c0a81e81c91ab62a4114de0",
            "ajv validate --strict=false",
        ):
            with self.subTest(required=required):
                self.assertIn(required, ci)

    def test_release_is_approval_gated_and_cross_channel(self) -> None:
        release = (WORKFLOWS / "release.yml").read_text(encoding="utf-8")
        for required in (
            "environment: public-release",
            "PRIVACY_LEGAL_APPROVAL_EVIDENCE",
            "INITIAL_RELEASE_KEY_ROTATION_EVIDENCE",
            "actions/attest-build-provenance@",
            "sbom.spdx.json",
            "mcp/v$VERSION",
            "mcp/latest",
            "npm install --global npm@11.12.1",
            "npm publish \"$tarball\" --access public",
            "audit signatures",
            "./mcp-publisher login github-oidc",
            "./mcp-publisher validate server.json",
            "./mcp-publisher publish server.json",
            "MCP_REGISTRY_URL/v0.1/servers/",
            "release_contract.py verify-registry",
            "gh release edit \"$TAG\" --draft=false --latest",
        ):
            with self.subTest(required=required):
                self.assertIn(required, release)
        for artifact in (
            "contextstream-mcp-linux-x64",
            "contextstream-mcp-linux-x64-remote-acceleration",
            "contextstream-mcp-linux-arm64",
            "contextstream-mcp-darwin-x64",
            "contextstream-mcp-darwin-arm64",
            "contextstream-mcp-win-x64.exe",
        ):
            self.assertIn(artifact, release)

        self.assertIn(
            "github.event_name == 'push' && 'publication' || github.ref", release
        )

        self.assertNotIn("ref: ${{ needs.prepare.outputs.source_commit }}", release)
        self.assertEqual(release.count("ref: ${{ github.sha }}"), 6)

    def test_codeql_covers_workflows_and_source_languages(self) -> None:
        codeql = (WORKFLOWS / "codeql.yml").read_text(encoding="utf-8")
        self.assertIn(
            "language: [actions, javascript-typescript, python]",
            codeql,
        )

    def test_dco_workflow_checks_the_exact_pull_request_range(self) -> None:
        dco = (WORKFLOWS / "dco.yml").read_text(encoding="utf-8")
        for required in (
            "fetch-depth: 0",
            "github.event.pull_request.base.sha",
            "github.event.pull_request.head.sha",
            'dco_check.py . "$BASE_SHA" "$HEAD_SHA"',
        ):
            with self.subTest(required=required):
                self.assertIn(required, dco)


if __name__ == "__main__":
    unittest.main()
