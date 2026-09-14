<!-- mcp-name: io.github.contextstream/mcp-server -->

<p align="center">
  <img src="assets/contextstream-logo.png" alt="ContextStream" width="64" />
</p>

<h1 align="center">ContextStream MCP Server</h1>

<p align="center"><strong>Persistent memory. Millisecond code search.</strong></p>

## Your agent knows code. Give it the decisions behind yours.

Connect your code, docs, and conversations so your agents can find the right
files, recall saved decisions, and build on past work across sessions and tools.

<a id="install-in-30-seconds"></a>

## Start free with MCP

**10,000 monthly credits. No credit card required.**

Works with Claude Code, Cursor, Codex, and other supported MCP clients.
Create your account or sign in during MCP onboarding. No separate website signup needed.

**macOS and Linux**

```sh
curl -fsSL https://contextstream.io/mcp/install.sh | sh
```

**Windows PowerShell**

```powershell
irm https://contextstream.io/mcp/install.ps1 | iex
```

Paste the command into your terminal and follow onboarding to connect your
project and supported editor. Restart your editor after setup.

[What gets installed and indexed?](docs/data-handling.md) ·
[Setup guide](https://contextstream.io/docs/editors/setup) ·
[Prefer to start on the web?](https://contextstream.io/signup)

## Make the next session useful

Ask your connected agent:

> Use ContextStream to find the files relevant to my next change. Cite the
> sources and retrieve any saved project decisions that should guide the work.

Then ask it to save a real decision and its reason. Start a new session and
retrieve that decision without explaining it again. Indexing supplies code
context; it cannot recover every undocumented decision.

## Proof and data controls

**95 ms median code search in our published benchmark.** This is the measured
successful-response median for the disclosed configuration, not a latency guarantee.
[See results, methodology, and limitations](https://contextstream.io/benchmarks).

**Scoped access. Traceable sources. Configurable capture.**
Indexing sends eligible source contents to ContextStream for hosted search.
Transcript saving and local Git metadata capture are on by default and can be
turned off. Review [data handling and controls](docs/data-handling.md) before
connecting sensitive projects.

<details>
<summary><strong>Other installation options and troubleshooting</strong></summary>

### npm (Node.js 20+)

```bash
npx -y @contextstream/mcp-server@latest setup
```

The npm launcher downloads the native Rust binary for your platform and verifies
its SHA-256 checksum. Pin an exact package version for production automation.

### Hosted MCP without a local process

For clients supporting Streamable HTTP and OAuth, the hosted endpoint is:

```text
https://mcp.contextstream.io/mcp
```

Use the [MCP documentation](https://contextstream.io/docs/mcp) for your client's
configuration and stdio alternatives. A hosted connection alone does not sync
your local checkout.

### Preview or diagnose setup

```bash
contextstream-mcp setup --dry-run
contextstream-mcp doctor --scope=all --only-configured
```

</details>

## Open source and contributing

This repository contains the MIT-licensed Rust MCP server, client, editor setup,
and release tooling. The hosted backend is separate and is not included here.
See the [architecture](docs/architecture.md), [release integrity](docs/release.md),
and [latest release](https://github.com/contextstream/mcp-server/releases/latest).

```bash
cargo build --locked -p mcp-server --bin contextstream-mcp
cargo test --locked --workspace
```

Use the pinned Rust toolchain. For contribution checks and commit sign-off, read
[CONTRIBUTING.md](CONTRIBUTING.md). Report vulnerabilities using
[SECURITY.md](SECURITY.md). See [LICENSE](LICENSE), [NOTICE](NOTICE), and
[GOVERNANCE.md](GOVERNANCE.md) for licensing, trademarks, and project governance.

---

**Let humans be human.**

[Docs](https://contextstream.io/docs/mcp) ·
[Pricing](https://contextstream.io/pricing) ·
[Integrations](https://contextstream.io/integrations) ·
[Benchmarks](https://contextstream.io/benchmarks)
