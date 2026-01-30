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
npx @contextstream/mcp-server@latest setup
```

</div>

<p align="center">
  <img src="compare1.gif" alt="ContextStream in action" width="700" />
</p>

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

## Setup Takes 30 Seconds

```bash
npx @contextstream/mcp-server@latest setup
```

The wizard handles everything: authentication, configuration, editor integration, and optional hooks that supercharge your workflow.

**Works with:** Claude Code • Cursor • VS Code • Claude Desktop • Codex CLI • Antigravity

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

## Manual Configuration

> Skip this if you ran the setup wizard.

<details>
<summary><b>Claude Code</b></summary>

```bash
claude mcp add contextstream -- npx @contextstream/mcp-server
claude mcp update contextstream -e CONTEXTSTREAM_API_KEY=your_key
```

</details>

<details>
<summary><b>Cursor / Claude Desktop</b></summary>

```json
{
  "mcpServers": {
    "contextstream": {
      "command": "npx",
      "args": ["-y", "@contextstream/mcp-server"],
      "env": { "CONTEXTSTREAM_API_KEY": "your_key" }
    }
  }
}
```

**Locations:** `~/.cursor/mcp.json` • `~/Library/Application Support/Claude/claude_desktop_config.json`

</details>

<details>
<summary><b>VS Code</b></summary>

```json
{
  "servers": {
    "contextstream": {
      "type": "stdio",
      "command": "npx",
      "args": ["-y", "@contextstream/mcp-server"],
      "env": { "CONTEXTSTREAM_API_KEY": "your_key" }
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

Or add to `~/.copilot/mcp-config.json`:

```json
{
  "mcpServers": {
    "contextstream": {
      "command": "npx",
      "args": ["-y", "@contextstream/mcp-server"],
      "env": { "CONTEXTSTREAM_API_KEY": "your_key" }
    }
  }
}
```

For more information, see the [GitHub Copilot CLI documentation](https://docs.github.com/en/copilot/concepts/agents/about-copilot-cli).

</details>

---

## Links

**Website:** https://contextstream.io

**Docs:** https://contextstream.io/docs

---

<p align="center">
  <strong>Stop teaching your AI the same things over and over.</strong><br/>
  <sub>ContextStream makes it brilliant from the first message.</sub>
</p>
