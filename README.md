<p align="center">
  <img src="https://contextstream.io/400logo.png" alt="ContextStream" width="80" />
</p>

<h1 align="center">ContextStream MCP Server</h1>

<p align="center">
  <strong>Give your AI coding assistant brilliant memory, deep context, and superpowers it never had.</strong>
</p>

<p align="center">
  <a href="https://www.npmjs.com/package/@contextstream/mcp-server"><img src="https://img.shields.io/npm/v/@contextstream/mcp-server.svg" alt="npm version" /></a>
  <a href="https://www.npmjs.com/package/@contextstream/mcp-server"><img src="https://img.shields.io/npm/dm/@contextstream/mcp-server.svg" alt="downloads" /></a>
  <a href="https://github.com/contextstream/mcp-server/blob/main/LICENSE"><img src="https://img.shields.io/npm/l/@contextstream/mcp-server.svg" alt="license" /></a>
</p>

<p align="center">
  <a href="https://contextstream.io/docs">Documentation</a> •
  <a href="https://contextstream.io/pricing">Pricing</a>
</p>

---

<div align="center">

```bash
npx --prefer-online -y @contextstream/mcp-server@latest setup
```

</div>

<p align="center">
  <img src="compare1.gif" alt="ContextStream in action" width="700" />
</p>

---

## Get Started (VS Code + Copilot)

### Option 1: Rust MCP (recommended)

```bash
curl -fsSL https://contextstream.io/scripts/mcp.sh | bash
```

```powershell
irm https://contextstream.io/scripts/mcp.ps1 | iex
```

Then run:

```bash
contextstream-mcp setup
```

### Option 2: Node MCP

```bash
npx --prefer-online -y @contextstream/mcp-server@latest setup
```

After setup, restart VS Code/Copilot.

**Works with:** Claude Code • Cursor • VS Code • Claude Desktop • Codex CLI • OpenCode • Antigravity

---

## This Isn't Just Memory. This Is Intelligence.

Other tools give your AI a notepad. **ContextStream gives it a brain.**

Your AI doesn't just remember things—it *understands* your entire codebase, learns from every conversation, pulls knowledge from your team's GitHub, Slack, and Notion, and delivers exactly the right context at exactly the right moment.

**One setup. Instant transformation.**

---

## What Changes When You Install This

| Before | After |
|--------|-------|
| AI searches files one-by-one, burning tokens | **Semantic search** finds code by meaning in milliseconds |
| Context lost when conversations get long | **Smart compression** preserves what matters before compaction |
| Team knowledge scattered across tools | **Unified intelligence** from GitHub, Slack, Notion—automatically |
| Same mistakes repeated across sessions | **Lessons system** ensures your AI learns from every failure |
| Generic responses, no project awareness | **Deep context** about your architecture, decisions, patterns |

---

## The Power Under the Hood

### Semantic Code Intelligence
Ask "where do we handle authentication?" and get the answer instantly. No grep chains. No reading 10 files. Your AI understands your code at a conceptual level.

### SmartRouter Context Delivery
Every message is analyzed. Risky refactor? Relevant lessons surface automatically. Making a decision? Your AI knows to capture it. The right context, every time, without you asking.

### Team Knowledge Fusion
Connect GitHub, Slack, and Notion. Discussions from months ago? Surfaced when relevant. That architecture decision buried in a PR comment? Your AI knows about it.

### Code Graph Analysis
"What depends on UserService?" "What's the impact of changing this function?" Your AI sees the connections across your entire codebase.

### Context Pressure Awareness
Long conversation? ContextStream tracks token usage, auto-saves critical state, and ensures nothing important is lost when context compacts.

---

## The Tools Your AI Gets

```
init            → Loads your workspace context instantly
context         → Delivers relevant context every single message
search          → Semantic, hybrid, keyword—find anything by meaning
session         → Captures decisions, preferences, lessons automatically
memory          → Builds a knowledge graph of your project
graph           → Maps dependencies and analyzes impact
project         → Indexes your codebase for semantic understanding
media           → Index and search video, audio, images (great for Remotion)
integration     → Queries GitHub, Slack, Notion directly
```

Your AI uses these automatically. You just code.

---

## Global Fallback Workspace (Unmapped Folders)

ContextStream now supports a catch-all mode for random folders (for example `~` or ad-hoc dirs) that are not associated with a project/workspace yet.

- `init(...)` resolves normal folder mappings first (`.contextstream/config.json`, parent/global mappings).
- If no mapping exists, it uses a single hidden global fallback workspace (`.contextstream-global`) in workspace-only mode.
- Context/memory/session tools continue to work without hard setup errors.
- Project-bound actions (for example `project(action="ingest_local")`) return guided remediation to create/select a project instead of failing with a raw `project_id required` error.
- As soon as you enter a mapped project folder, that real workspace/project is prioritized and replaces fallback scope.

---

## Manual Configuration

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
<summary><b>VS Code</b></summary>

For GitHub Copilot in VS Code, the easiest path is the hosted remote MCP with built-in OAuth. Marketplace installs should write this remote server definition automatically.

**Hosted remote MCP (recommended)**

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

On first use, VS Code should prompt you to authorize ContextStream in the browser and then complete setup without an API key in the config file.

`npx @contextstream/mcp-server@latest setup` now defaults VS Code/Copilot to this hosted remote when you are using the production ContextStream cloud. To force a local runtime instead, run setup with `CONTEXTSTREAM_VSCODE_MCP_MODE=local`.

For self-hosted or non-default API deployments, local runtime remains the default:

**Rust MCP (recommended)**

```json
{
  "servers": {
    "contextstream": {
      "type": "stdio",
      "command": "contextstream-mcp",
      "args": [],
      "env": {
        "CONTEXTSTREAM_API_URL": "https://api.contextstream.io",
        "CONTEXTSTREAM_API_KEY": "your_key",
        "CONTEXTSTREAM_TOOLSET": "complete",
        "CONTEXTSTREAM_TRANSCRIPTS_ENABLED": "true",
        "CONTEXTSTREAM_HOOK_TRANSCRIPTS_ENABLED": "true",
        "CONTEXTSTREAM_SEARCH_LIMIT": "15",
        "CONTEXTSTREAM_SEARCH_MAX_CHARS": "2400"
      }
    }
  }
}
```

**Node MCP server**

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
        "CONTEXTSTREAM_TOOLSET": "complete",
        "CONTEXTSTREAM_TRANSCRIPTS_ENABLED": "true",
        "CONTEXTSTREAM_HOOK_TRANSCRIPTS_ENABLED": "true",
        "CONTEXTSTREAM_SEARCH_LIMIT": "15",
        "CONTEXTSTREAM_SEARCH_MAX_CHARS": "2400"
      }
    }
  }
}
```

</details>

<details>
<summary><b>GitHub Copilot CLI</b></summary>

Use the Copilot CLI to interactively add the MCP server:

```bash
/mcp add
```

Or add to `~/.copilot/mcp-config.json` (pick one runtime):

**Rust MCP (recommended)**

```json
{
  "mcpServers": {
    "contextstream": {
      "command": "contextstream-mcp",
      "args": [],
      "env": {
        "CONTEXTSTREAM_API_URL": "https://api.contextstream.io",
        "CONTEXTSTREAM_API_KEY": "your_key",
        "CONTEXTSTREAM_TOOLSET": "complete",
        "CONTEXTSTREAM_TRANSCRIPTS_ENABLED": "true",
        "CONTEXTSTREAM_HOOK_TRANSCRIPTS_ENABLED": "true",
        "CONTEXTSTREAM_SEARCH_LIMIT": "15",
        "CONTEXTSTREAM_SEARCH_MAX_CHARS": "2400"
      }
    }
  }
}
```

**Node MCP server**

```json
{
  "mcpServers": {
    "contextstream": {
      "command": "npx",
      "args": ["--prefer-online", "-y", "@contextstream/mcp-server@latest"],
      "env": {
        "CONTEXTSTREAM_API_URL": "https://api.contextstream.io",
        "CONTEXTSTREAM_API_KEY": "your_key",
        "CONTEXTSTREAM_TOOLSET": "complete",
        "CONTEXTSTREAM_TRANSCRIPTS_ENABLED": "true",
        "CONTEXTSTREAM_HOOK_TRANSCRIPTS_ENABLED": "true",
        "CONTEXTSTREAM_SEARCH_LIMIT": "15",
        "CONTEXTSTREAM_SEARCH_MAX_CHARS": "2400"
      }
    }
  }
}
```

For more information, see the [GitHub Copilot CLI documentation](https://docs.github.com/en/copilot/concepts/agents/about-copilot-cli).

</details>

---

## VS Code + Copilot Tips

- Run setup once and keep both config files:
  - `~/.copilot/mcp-config.json`
  - `.vscode/mcp.json`
- Rust install: use `contextstream-mcp` as the command.
- Node install: use `npx --prefer-online -y @contextstream/mcp-server@latest` as the command.
- Force local VS Code/Copilot setup with `CONTEXTSTREAM_VSCODE_MCP_MODE=local`.
- Force hosted remote VS Code/Copilot setup with `CONTEXTSTREAM_VSCODE_MCP_MODE=remote`.
- Use `mcpServers` in Copilot CLI config and `servers` in VS Code config.

## Quick Troubleshooting

- Remove duplicate ContextStream entries across Workspace/User config scopes.
- Check `CONTEXTSTREAM_API_URL` and `CONTEXTSTREAM_API_KEY` are set.
- Remove stale version pins like `@contextstream/mcp-server@0.3.xx`.
- Restart VS Code/Copilot after config changes.

## Marketplace Note

The MCP marketplace entry now targets the hosted remote MCP at `https://mcp.contextstream.io/mcp?default_context_mode=fast` so VS Code can use the native OAuth flow instead of writing a local npm-based stdio config.

Use the Rust or Node local runtime configs above only when you explicitly want local execution, custom/self-hosted endpoints, or editor environments that do not support the hosted remote flow.

---

## Links

**Website:** https://contextstream.io

**Docs:** https://contextstream.io/docs

---

<p align="center">
  <strong>Stop teaching your AI the same things over and over.</strong><br/>
  <sub>ContextStream makes it brilliant from the first message.</sub>
</p>
