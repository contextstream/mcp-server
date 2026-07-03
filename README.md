<p align="center">
  <img src="https://contextstream.io/400logo.png" alt="ContextStream — persistent memory and semantic code search for AI coding assistants" width="80" />
</p>

<h1 align="center">ContextStream MCP Server</h1>

<p align="center">
  <strong>Persistent memory, semantic code search, and team context for Claude Code, Cursor, VS Code Copilot, Windsurf — and every MCP-compatible AI coding assistant.</strong>
</p>

<p align="center">
  🏆 <b>90.0% on LongMemEval-S</b> — the full 500-instance suite with the official judge.<br/>
  Beats supermemory with statistical significance; matches Zep. <a href="https://contextstream.io/benchmarks">See the benchmarks →</a>
</p>

<p align="center">
  <a href="https://www.npmjs.com/package/@contextstream/mcp-server"><img src="https://img.shields.io/npm/v/@contextstream/mcp-server.svg" alt="npm version" /></a>
  <a href="https://www.npmjs.com/package/@contextstream/mcp-server"><img src="https://img.shields.io/npm/dm/@contextstream/mcp-server.svg" alt="npm downloads" /></a>
  <a href="https://github.com/contextstream/mcp-server/blob/main/LICENSE"><img src="https://img.shields.io/npm/l/@contextstream/mcp-server.svg" alt="MIT license" /></a>
  <img src="https://img.shields.io/node/v/@contextstream/mcp-server.svg" alt="node version" />
</p>

<p align="center">
  <a href="https://contextstream.io/docs">Documentation</a> •
  <a href="https://contextstream.io/pricing">Pricing</a> •
  <a href="#faq">FAQ</a>
</p>

---

## Try it in 30 seconds

```bash
npx --prefer-online -y @contextstream/mcp-server@latest setup
```

That one command detects your AI editors, writes their MCP configs and rules, installs lifecycle hooks where supported, indexes your project in the background, and **verifies everything it just did** — then you restart your editor and your AI has memory. Free tier available.

Automating it? Zero-prompt mode takes every default:

```bash
npx --prefer-online -y @contextstream/mcp-server@latest setup --yes
```

<p align="center">
  <img src="compare1.gif" alt="Side-by-side comparison: an AI coding assistant with ContextStream memory and semantic search vs. without" width="700" />
</p>

---

## We win on memory benchmarks — measured honestly

**ContextStream scores 90.0% on the full LongMemEval-S benchmark**, the standard test of conversational memory over ~115k-token multi-session histories. That's the complete 500-instance suite with the official GPT-4o judge — 450/500 correct (89.6% single-shot, 90.0% with self-consistency k=3; Wilson 95% CI [87.1%, 92.3%]). Published June 14, 2026.

| System | LongMemEval-S | Notes |
|---|:---:|---|
| **ContextStream** | **90.0%** | Full 500 instances, official GPT-4o judge |
| Zep | 90.2% | Vendor-published — a statistical tie (0.2 pt is inside measurement noise) |
| supermemory | 85.4% | Vendor-published — ContextStream wins with statistical significance |

On **multi-session recall** — the memory that actually matters for a coding agent working across days of sessions — ContextStream scores **81.2% vs Zep's published 57.9%** in the per-family comparison. And on the agentic project-memory benchmark, the same memory raises **agent task success from 58% to 96%**.

Full methodology, per-family breakdowns, and judge-comparability notes (competitor numbers are cited from each vendor's own publications): **[contextstream.io/benchmarks](https://contextstream.io/benchmarks)**

---

## What is ContextStream?

**ContextStream is a Model Context Protocol (MCP) server that gives AI coding assistants long-term memory and deep codebase understanding.** It indexes your code for semantic search, records your decisions, lessons, and plans across sessions, maps your dependency graph, and pulls in team knowledge from GitHub, Slack, and Notion — then delivers exactly the right slice of all that to your AI on every message.

It works with any MCP client: Claude Code, Cursor, VS Code + GitHub Copilot, Windsurf, Cline, Roo Code, Kilo Code, Codex CLI, OpenCode, Aider, Antigravity, and Claude Desktop.

---

## Why do AI coding assistants forget everything?

Because every conversation starts from zero. Your AI re-reads the same files, re-derives the same architecture, repeats last week's mistake, and loses the thread the moment the context window compacts. ContextStream fixes the whole class of problem:

| Without ContextStream | With ContextStream |
|--------|-------|
| AI greps files one-by-one, burning tokens | **Semantic code search** finds code by meaning in milliseconds |
| Context lost when conversations get long | **Pre-compaction capture** saves critical state before it's gone — and restores it after |
| Same mistakes repeated across sessions | **Lessons system** surfaces past failures *before* your AI repeats them |
| "Why did we choose X?" — nobody remembers | **Decisions and plans** persist and resurface when relevant |
| Team knowledge scattered across tools | **GitHub, Slack, and Notion knowledge**, queried automatically |
| Generic answers with no project awareness | **Workspace context** on every single message |

---

## What your AI can do after setup

### 🔍 Find code by meaning, not keywords
Ask *"where do we handle authentication?"* and get ranked, snippet-level answers instantly. Hybrid semantic + keyword search with exact-token fusion, so a symbol lookup like `resolveWriteScope` lands the definition — not lookalikes. Search works the moment setup finishes: keyword results come back immediately while the semantic index builds in the background.

### 🧠 Remember everything that matters
Decisions, lessons, preferences, plans, tasks, docs, runbooks — captured during work and surfaced automatically on later turns, in later sessions, even after context compaction. Every prior session's transcript is indexed and queryable: *"what did we decide about the id format last week?"* just works.

### 💬 Ask the workspace when stuck
The built-in **Agent Q&A** tool lets your AI ask your workspace's knowledge base — prior decisions, conventions, runbooks, guardrails — and get a grounded answer with citations for every claim.

### 🕸️ See the whole graph
*"What depends on `UserService`?" "What breaks if I change this function?"* Dependency mapping, impact analysis, circular-dependency and dead-code detection over your whole codebase.

### 📦 Hand off context between agents
**ContextCapsule** packages project state into a portable, shareable snapshot — bootstrap a fresh agent, hand off to a teammate, or share a token-gated link with an external agent.

### 🛡️ Survive long sessions
Token pressure is tracked continuously (with thresholds sized to your model's context window). Before compaction hits, critical state is checkpointed; after it, context restores.

---

## Which AI editors and agents does it support?

| Editor / Agent | Managed rules | MCP config | Lifecycle hooks |
|---|:---:|:---:|:---:|
| Claude Code | ✅ | ✅ | ✅ |
| Cursor | ✅ (`.cursor/rules/*.mdc`) | ✅ | ✅ |
| Windsurf | ✅ | ✅ | ✅ |
| Cline | ✅ | ✅ | ✅ |
| Roo Code | ✅ | ✅ | ✅ |
| Kilo Code | ✅ | ✅ | rules-based |
| VS Code + GitHub Copilot | ✅ | ✅ (incl. hosted OAuth) | rules-based |
| Codex CLI | ✅ | ✅ | rules-based |
| OpenCode | ✅ | ✅ | rules-based |
| Aider | ✅ | ✅ | rules-based |
| Antigravity | ✅ | ✅ | rules-based |
| Claude Desktop | — | ✅ | — |

Anything that speaks the Model Context Protocol can connect — the table just shows what the setup wizard configures automatically.

---

## The tools your AI gets

36 tools in the default surface, organized as consolidated domains so they cost ~75% fewer tokens than individual registrations:

```
init / context   → workspace state + the right context on every message
search           → semantic, hybrid, keyword, pattern, exhaustive, refactor modes
memory           → events, decisions, docs, runbooks, tasks, todos, diagrams, transcripts
session          → capture decisions & lessons, recall past sessions, plans, retroactive capture
qa               → grounded Q&A over your workspace knowledge base, with citations
graph            → dependencies, impact analysis, circular deps, unused code
capsule          → portable, shareable context snapshots for agent handoffs
entity           → tickets, incidents, releases, sprints, OKRs, risks
project / workspace / skill / media / vcs / reminder / integration / help
```

Plus focused write tools (`capture_plan`, `memory_create_doc`, …) so agents that display tool names show *what* they're doing. Your AI uses all of this automatically — you just code.

---

## CLI commands

```bash
contextstream-mcp setup            # interactive onboarding wizard
contextstream-mcp setup --yes      # zero-prompt setup with sane defaults (great for CI/dotfiles)
contextstream-mcp doctor           # ✓/✗ diagnostics: auth, scope, index health, rules, hooks
contextstream-mcp index [path]     # index a project folder on demand
```

`setup --editors=claude,cursor` limits configuration to specific editors; `doctor` tells you exactly what's misconfigured and how to fix it — it runs automatically at the end of every setup.

---

## Manual configuration

> Skip this if you ran the setup wizard.

<details>
<summary><b>Claude Code</b></summary>

```bash
claude mcp add contextstream -- npx --prefer-online -y @contextstream/mcp-server@latest
claude mcp update contextstream -e CONTEXTSTREAM_API_URL=https://api.contextstream.io -e CONTEXTSTREAM_API_KEY=your_key
```

</details>

<details>
<summary><b>Cursor / Claude Desktop</b></summary>

```json
{
  "mcpServers": {
    "contextstream": {
      "command": "npx",
      "args": ["--prefer-online", "-y", "@contextstream/mcp-server@latest"],
      "env": {
        "CONTEXTSTREAM_API_URL": "https://api.contextstream.io",
        "CONTEXTSTREAM_API_KEY": "your_key"
      }
    }
  }
}
```

**Locations:** `~/.cursor/mcp.json` • `~/Library/Application Support/Claude/claude_desktop_config.json`

</details>

<details>
<summary><b>OpenCode</b></summary>

Local server:

```json
{
  "$schema": "https://opencode.ai/config.json",
  "mcp": {
    "contextstream": {
      "type": "local",
      "command": ["npx", "-y", "contextstream-mcp"],
      "environment": {
        "CONTEXTSTREAM_API_KEY": "{env:CONTEXTSTREAM_API_KEY}"
      },
      "enabled": true
    }
  }
}
```

Remote server:

```json
{
  "$schema": "https://opencode.ai/config.json",
  "mcp": {
    "contextstream": {
      "type": "remote",
      "url": "https://mcp.contextstream.com",
      "enabled": true
    }
  }
}
```

For the local variant, export `CONTEXTSTREAM_API_KEY` before launching OpenCode.

**Locations:** `./opencode.json` • `~/.config/opencode/opencode.json`

</details>

<details>
<summary><b>VS Code + GitHub Copilot</b></summary>

The easiest path is the hosted remote MCP with built-in OAuth — no API key in the config file:

```json
{
  "servers": {
    "contextstream": {
      "type": "http",
      "url": "https://mcp.contextstream.io/mcp?default_context_mode=fast"
    }
  }
}
```

`setup` defaults VS Code/Copilot to this hosted remote on the production cloud. To force a local runtime, run setup with `CONTEXTSTREAM_VSCODE_MCP_MODE=local`, or use stdio directly:

```json
{
  "servers": {
    "contextstream": {
      "type": "stdio",
      "command": "npx",
      "args": ["--prefer-online", "-y", "@contextstream/mcp-server@latest"],
      "env": {
        "CONTEXTSTREAM_API_URL": "https://api.contextstream.io",
        "CONTEXTSTREAM_API_KEY": "your_key",
        "CONTEXTSTREAM_TOOLSET": "complete"
      }
    }
  }
}
```

Keep both `~/.copilot/mcp-config.json` (uses `mcpServers`) and `.vscode/mcp.json` (uses `servers`) in sync — setup writes both.

</details>

<details>
<summary><b>GitHub Copilot CLI</b></summary>

Use `/mcp add` interactively, or add to `~/.copilot/mcp-config.json`:

```json
{
  "mcpServers": {
    "contextstream": {
      "command": "npx",
      "args": ["--prefer-online", "-y", "@contextstream/mcp-server@latest"],
      "env": {
        "CONTEXTSTREAM_API_URL": "https://api.contextstream.io",
        "CONTEXTSTREAM_API_KEY": "your_key",
        "CONTEXTSTREAM_TOOLSET": "complete"
      }
    }
  }
}
```

See the [GitHub Copilot CLI documentation](https://docs.github.com/en/copilot/concepts/agents/about-copilot-cli) for details.

</details>

<details>
<summary><b>Unmapped folders (global fallback workspace)</b></summary>

Folders that aren't associated with any project (your home directory, ad-hoc scratch dirs) still work: `init` falls back to a hidden catch-all workspace in workspace-only mode, memory/session/context tools keep functioning, and project-bound actions return guided remediation instead of raw errors. The moment you enter a mapped project folder, the real workspace/project takes over.

</details>

---

## FAQ

### What is an MCP server?

An MCP (Model Context Protocol) server exposes tools and context to AI assistants over a standard protocol. Claude Code, Cursor, VS Code Copilot, Windsurf, and most modern AI coding tools are MCP clients — install one server, and every client you use gets the same capabilities.

### Does ContextStream work with Claude Code / Cursor / Copilot / Windsurf?

Yes — all of them, plus Cline, Roo Code, Kilo Code, Codex CLI, OpenCode, Aider, Antigravity, and Claude Desktop. The setup wizard configures each editor's MCP config, managed rules, and (where the editor supports them) lifecycle hooks automatically.

### Is my code private?

Your code is private and securely stored, isolated per workspace with no cross-tenant access. You control exclusions with `.contextstream/ignore` (gitignore syntax), and `project(action="purge")` completely de-indexes a project on demand — without touching your captured memory.

### How much does it cost?

There's a free tier to start with. Larger indexes, the full code graph, and team features are on paid plans: see [pricing](https://contextstream.io/pricing).

### How does ContextStream score on memory benchmarks?

90.0% on the full 500-instance LongMemEval-S suite with the official GPT-4o judge (89.6% single-shot) — beating supermemory's published 85.4% with statistical significance and matching Zep's published 90.2% within the confidence interval. On multi-session recall specifically, ContextStream scores 81.2% vs Zep's published 57.9%. Methodology and per-family breakdowns: [contextstream.io/benchmarks](https://contextstream.io/benchmarks).

### How is this different from other AI memory tools?

Most tools store notes. ContextStream combines **memory** (decisions, lessons, plans, session transcripts), **semantic code search** over your indexed codebase, a **dependency/knowledge graph**, **grounded Q&A with citations**, and **team integrations** (GitHub, Slack, Notion) behind one MCP server — and proactively delivers the relevant slice on every message instead of waiting to be asked.

### Can I use it in CI or scripted environments?

Yes: `setup --yes` runs the entire wizard with zero prompts (set `CONTEXTSTREAM_API_KEY` in the environment), `--editors=<list>` scopes it, and `contextstream-mcp doctor` gives scriptable ✓/✗ diagnostics.

### Do I have to wait for indexing?

No. Project indexing runs in the background — keyword search works immediately, and semantic results fill in as the index builds. Check progress anytime with `contextstream-mcp doctor`.

### How do I uninstall or disable it?

Remove the `contextstream` entry from your editor's MCP config, and set `CONTEXTSTREAM_HOOK_ENABLED=false` (or re-run setup) to disable hooks. `project(action="forget_local")` unbinds a folder locally without touching server-side data.

---

## Troubleshooting

- **Start with `contextstream-mcp doctor`** — it checks auth, API reachability, folder scope, index health, rule files, and hooks, with a fix hint per failure.
- Remove duplicate ContextStream entries across Workspace/User config scopes.
- Check `CONTEXTSTREAM_API_URL` and `CONTEXTSTREAM_API_KEY` are set; remove stale version pins like `@contextstream/mcp-server@0.3.xx`.
- Restart your editor after config changes.
- **Hosted HTTP OAuth:** the remote transport's OAuth flow routes through `vscode.dev`; where that's blocked (corporate networks), use stdio with an API key instead.
- **SDK compatibility:** `@modelcontextprotocol/sdk` 1.28.0+ introduces breaking changes; this package pins `>=1.25.1 <1.28.0`. If you see Zod schema errors on startup, check your SDK resolution.

---

## Links

**Website:** https://contextstream.io • **Docs:** https://contextstream.io/docs • **Changelog:** [CHANGELOG.md](CHANGELOG.md)

---

<p align="center">
  <strong>Stop teaching your AI the same things over and over.</strong><br/>
  <sub>ContextStream makes it brilliant from the first message.</sub>
</p>
