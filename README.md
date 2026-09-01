<!-- mcp-name: io.github.contextstream/mcp-server -->

<p align="center">
  <img src="assets/contextstream-logo.png" alt="ContextStream" width="112" />
</p>

<h1 align="center">ContextStream MCP Server</h1>

<p align="center">
  <strong>Give every coding agent persistent memory, semantic code search, and the right project context on every turn.</strong>
</p>

<p align="center">
  Works with Claude Code, Cursor, VS Code + Copilot, Windsurf, Cline, Roo Code, Kilo Code, Codex, OpenCode, Aider, Antigravity, and other MCP clients.
</p>

<p align="center">
  <a href="https://www.npmjs.com/package/@contextstream/mcp-server"><img src="https://img.shields.io/npm/v/@contextstream/mcp-server.svg" alt="npm version" /></a>
  <a href="https://github.com/contextstream/mcp-server/releases/latest"><img src="https://img.shields.io/github/v/release/contextstream/mcp-server" alt="latest release" /></a>
  <a href="https://github.com/contextstream/mcp-server/actions/workflows/ci.yml"><img src="https://github.com/contextstream/mcp-server/actions/workflows/ci.yml/badge.svg" alt="CI status" /></a>
  <a href="LICENSE"><img src="https://img.shields.io/badge/license-MIT-blue.svg" alt="MIT license" /></a>
</p>

<p align="center">
  <a href="#install-in-30-seconds"><strong>Install in 30 seconds</strong></a> ·
  <a href="https://contextstream.io/benchmarks">Benchmarks</a> ·
  <a href="https://contextstream.io/docs/mcp">Documentation</a> ·
  <a href="https://contextstream.io/signup">Start free</a>
</p>

---

Your AI should not have to rediscover the repository, re-learn the architecture,
or repeat last week's mistake every time a chat starts. ContextStream gives MCP
clients a shared context layer that remembers decisions and lessons, searches
code by meaning, maps dependencies, and restores the thread after compaction.

The server in this repository is the canonical, MIT-licensed Rust
implementation. The recommended editor transport is the hosted MCP at
`https://mcp.contextstream.io/mcp`; a small local helper keeps explicitly linked
checkouts in sync.

## Benchmarks: strong memory, strong retrieval, published methodology

We publish the full method, confidence intervals, comparison caveats, and known
losses—not just the winning number. The product-level figures below are from
the published July 2026 report; the LongMemEval-S result was published June 14,
2026.

| Benchmark | ContextStream result | What was measured |
|---|---:|---|
| **LongMemEval-S** | **90.0%** | 450/500 correct on the full suite; ~115k-token multi-session histories; official pinned GPT-4o judge; `gpt-5.5` reader with self-consistency `k=3` (89.6% single-shot); Wilson 95% CI [87.1%, 92.3%] |
| **Pinned-repo code search** | **99.38% Recall@10** | 93.83% Primary@10 and 95 ms successful-response p50 over 177 judged-valid queries, averaged across two cold and two warm runs; 0/708 failed requests; no local checkout |
| **Agentic project memory** | **96% task success** | Up from 58% without the memory layer; one repository, one agent model, three trials per cell, so this result is directional |

On the standard memory benchmark, 90.0% is statistically above supermemory's
published 85.4% and statistically tied with Zep's published 90.2%. Those
comparisons use each vendor's published result; they are not presented as a
private same-run head-to-head.

**[Read the full benchmark report, methodology, per-category results, and losses →](https://contextstream.io/benchmarks)**

The repository also includes Criterion suites for JSON-RPC parsing, tool
dispatch, response serialization, and stdio/HTTP transport primitives. Run
them locally with `cargo bench --locked -p mcp-server`.

## Install in 30 seconds

Pick the command for your platform. Each installer downloads the current native
release and launches the guided setup wizard.

### macOS and Linux

```bash
curl -fsSL https://contextstream.io/scripts/setup.sh | bash
```

Native releases are published for macOS Apple Silicon and Intel, plus Linux
ARM64 and x64.

### Windows PowerShell

```powershell
irm https://contextstream.io/scripts/setup.ps1 | iex
```

The Windows installer uses the native x64 binary and installs it in your user
profile, so it does not need an administrator shell.

### npm alternative (Node.js 20+)

```bash
npx -y @contextstream/mcp-server@latest setup
```

The dependency-free npm launcher selects the matching release artifact,
verifies its SHA-256 checksum, caches it by exact package version, and starts
the Rust binary. It supports macOS ARM64/x64, Linux ARM64/x64, and Windows x64.

### What the wizard does

The wizard signs you in, detects supported editors, writes their hosted MCP
configuration and managed rules, installs lifecycle hooks where supported,
links the exact project folder you choose, starts indexing, and checks the
result. You review the choices before it makes changes.

After setup, restart the editor and try:

> Use ContextStream to summarize this repository, cite the files you relied on,
> and tell me one active decision or risk that should affect my next change.

Want to inspect the changes first? Every setup write can be previewed:

```bash
contextstream-mcp setup --dry-run
```

Already configured and troubleshooting? Run:

```bash
contextstream-mcp doctor --scope=all --only-configured
```

## No-install hosted connection

If your MCP client supports Streamable HTTP and OAuth, connect directly—there
is no local MCP process to keep running:

```json
{
  "mcpServers": {
    "contextstream": {
      "type": "http",
      "url": "https://mcp.contextstream.io/mcp"
    }
  }
}
```

Some clients use a different root key or config format, which is why the setup
wizard is the easiest path. The hosted gateway is the default; the installed
helper only syncs the local checkouts you explicitly link.

Clients that require stdio can use the verified npm launcher:

```json
{
  "mcpServers": {
    "contextstream": {
      "command": "npx",
      "args": ["-y", "@contextstream/mcp-server@latest"],
      "env": {
        "CONTEXTSTREAM_API_KEY": "your-api-key"
      }
    }
  }
}
```

For production automation, pin an exact 1.x package version instead of
`latest`.

## What your agent gets

| Capability | What changes in practice |
|---|---|
| **Grounded context** | Each turn starts with the relevant workspace rules, prior decisions, lessons, and current project state. |
| **Semantic code search** | Find implementations by intent, with hybrid, keyword, pattern, exhaustive, and refactor-aware modes when precision matters. |
| **Durable memory** | Decisions, plans, tasks, runbooks, preferences, and session transcripts remain queryable across chats and tools. |
| **Compaction survival** | Important state is checkpointed before long conversations compact and restored afterward. |
| **Dependency intelligence** | Trace blast radius, dependencies, circular imports, complexity, and unused code without manually rebuilding the graph. |
| **Agent handoffs** | Package verified state and next steps into durable handoffs and portable ContextCapsules. |
| **Team context** | Bring shared workspace knowledge and supported GitHub, Slack, and Notion context into the same MCP surface. |

The default tool surface is grouped into focused domains—`context`, `search`,
`memory`, `session`, `graph`, `entity`, `capsule`, `project`, `workspace`,
`skill`, `media`, `vcs`, and more—so agents get broad capability without an
enormous tool-registration prompt.

## Supported editors and agents

The wizard currently detects and configures:

- Claude Code
- Cursor
- Windsurf
- VS Code + GitHub Copilot
- Cline
- Kilo Code
- Roo Code
- OpenAI Codex
- Aider
- Antigravity
- OpenCode

Anything else that speaks the Model Context Protocol can connect to the hosted
URL or use the stdio launcher.

## What is open source?

This repository contains the canonical Rust MCP server, protocol types, client
and session layers, editor setup, hooks, public transport code, release
automation, and the MongoDB-free acceleration foundation. It builds as seven
workspace crates:

- `mcp-server`
- `mcp-tools`
- `mcp-client`
- `mcp-session`
- `mcp-types`
- `mcp-model-registry`
- `mcp-acceleration-products`

The hosted backend, credentials, production deployment configuration, and
premium provider implementations are not included. Public-build compatibility
traits remain no-op so the core protocol surface stays stable. See
[`docs/architecture.md`](docs/architecture.md) for the boundary.

## Privacy and data controls

Setup shows a disclosure before changing editor configuration or indexing a
project. You choose the folder and workspace relationship; only matching source
files from a validated project can be indexed. Transcript capture, local Git
metadata capture, ignore rules, kill switches, retention behavior, and deletion
paths are documented in [`docs/data-handling.md`](docs/data-handling.md).

Review that document before using `setup --yes` in automation. The CLI also
provides `--dry-run` for setup, repair, migration, and uninstall workflows.

## Build and test from source

The workspace uses the Rust toolchain pinned in `rust-toolchain.toml`.

```bash
cargo build --locked -p mcp-server --bin contextstream-mcp
cargo test --locked --workspace
cargo bench --locked -p mcp-server
```

To build the hosted HTTP gateway foundation:

```bash
cargo build --locked --release -p mcp-server --bin contextstream-mcp \
  --features remote-acceleration
```

## Release integrity

Every release publishes native binaries, `checksums.txt`, `version.json`, and
an SPDX SBOM. The npm launcher downloads only from the immutable path for its
exact package version, verifies manifests and binary SHA-256, uses an atomic
versioned cache, and supports verified offline reuse with
`CONTEXTSTREAM_MCP_OFFLINE=1`.

See [`docs/release.md`](docs/release.md) for the release contract and
[the latest GitHub release](https://github.com/contextstream/mcp-server/releases/latest)
for artifacts.

All 1.x npm releases preserve these executable aliases:

- `mcp-server`
- `contextstream-mcp`
- `contextstream-hook` (forwards to the Rust `hook` subcommand)

## Contributing and security

Issues and pull requests are welcome. See [CONTRIBUTING.md](CONTRIBUTING.md),
[SECURITY.md](SECURITY.md), and [GOVERNANCE.md](GOVERNANCE.md). Contributions
must include the Developer Certificate of Origin sign-off (`git commit -s`).

The code is MIT licensed. ContextStream trademarks and the hosted-service
boundary are described in [NOTICE](NOTICE).

---

<p align="center">
  <strong>Stop teaching your coding agent the same project twice.</strong><br />
  <a href="#install-in-30-seconds">Install ContextStream</a> ·
  <a href="https://contextstream.io/signup">Start free</a> ·
  <a href="https://contextstream.io/docs/mcp">Read the docs</a>
</p>
