# ContextStream MCP Server

Persistent memory, semantic search, and code intelligence for any MCP-compatible AI tool (Cursor, Claude Code, Windsurf, VS Code, Claude Desktop, Codex CLI, etc.).

ContextStream is a shared "brain" for AI-assisted development. It stores decisions, preferences, notes (including implementation notes), lessons, tasks, and project context — and lets your AI tools search and analyze your codebase with consistent context across sessions.

**v0.4.x:** Consolidated domain tools (~11 tools) for **~75% lower tool-registry token overhead** vs previous versions.

---

## Just Ask

**You don't need to memorize tool names.** Describe what you want and your AI will call the right ContextStream tools:

| You say… | ContextStream does… |
|----------|---------------------|
| "session summary" | Summarizes current workspace/project context |
| "what did we decide about auth?" | Recalls decisions related to authentication |
| "remember we're using PostgreSQL" | Captures that fact for future sessions |
| "search for payment code" | Searches your codebase semantically/hybrid/keyword |
| "what depends on UserService?" | Analyzes dependency graph & impact |

> **Tip:** For best results, add the **recommended editor rules** so your AI reliably calls `session_init` / `context_smart` when appropriate:
> https://contextstream.io/docs/quickstart

![ContextStream in action](compare1.gif)

---

## Choose Your Mode (Token Footprint)

MCP clients often inject the tool catalog into the model context. v0.4.x is designed to keep that overhead small.

| Mode | What it exposes | Best for | Enable |
|------|-----------------|----------|--------|
| **Consolidated** (default) | ~11 domain tools with `action` / `mode` dispatch | Most users (recommended) | `CONTEXTSTREAM_CONSOLIDATED=true` |
| **Router** (extreme minimization) | ~2 meta-tools (`contextstream`, `contextstream_help`) | Tight context budgets / many MCP servers | `CONTEXTSTREAM_PROGRESSIVE_MODE=true` |
| **Legacy** (granular tools) | Older `light/standard/complete` toolsets | Back-compat / old prompts | `CONTEXTSTREAM_CONSOLIDATED=false` |

> **Note:** The env var name for Router mode is `CONTEXTSTREAM_PROGRESSIVE_MODE` (historical naming). It enables the ~2-tool "router" surface.

---

## Features

- **Consolidated domain tools** (v0.4.x): short tool list with action/mode dispatch
- Session-aware context loading (`session_init`, `context_smart`)
- Memory capture + recall (decisions, preferences, notes, implementation notes, lessons, tasks, bugs)
- Code search (semantic, hybrid, keyword, pattern)
- Knowledge graph + code analysis (dependencies, impact, call paths, circular deps, unused code)
- Graph ingestion for full graph builds (`graph(action="ingest")`)
- Local repo ingestion for indexing (`project(action="ingest_local")`)
- Auto-context: on first tool call in a new session, the server can auto-initialize context

> **⚠️ Search-First Rule:** For best results, your AI should use ContextStream `search(mode="hybrid")` **before** local tools like Glob/Grep/Read. This is enforced via editor rules and `context_smart` responses.

---

## Graph Tiers

| Tier | Capabilities |
|------|--------------|
| **Pro (Graph-Lite)** | Module-level import graph, dependencies, and 1-hop impact |
| **Elite/Team (Full Graph)** | Module + call + dataflow + type layers, plus full graph ingestion |

---

## Requirements

- **Node.js 18+**
- A **ContextStream account**
- Auth via **API key** or **JWT**

Default API URL: `https://api.contextstream.io`

---

## Quickstart (2 minutes)

### 1) Run the Setup Wizard (recommended)

The wizard:
- Authenticates (browser/device login by default)
- Creates/stores an API key
- Installs recommended editor rules (optional)
- Writes MCP config files for supported tools

```bash
npx -y @contextstream/mcp-server setup
```

**Useful flags:**

| Flag | Description |
|------|-------------|
| `--dry-run` | Preview without writing files |

**Notes:**
- The wizard stores credentials at `~/.contextstream/credentials.json` for convenience. Delete it to force a fresh login.
- Codex CLI MCP config is global-only (`~/.codex/config.toml`), so the wizard writes Codex config globally when selected.
- Some tools still require UI/CLI setup (the wizard will tell you when it can't write a config).

### 2) Run the MCP Server

**Recommended** (works well with MCP configs):

```bash
npx -y @contextstream/mcp-server
```

**Or install globally:**

```bash
npm install -g @contextstream/mcp-server
contextstream-mcp
```

### 3) Keeping Updated

**If you use `npx`:** Restart your AI tool/editor and run ContextStream again
(or pin the version: `npx -y @contextstream/mcp-server@0.4.3`)

**If you installed globally:**

```bash
npm update -g @contextstream/mcp-server
```

After updating, restart your AI tool/editor so it reloads the tool catalog.

---

## Configure Your MCP Client (Manual)

> If you ran the setup wizard, you can usually skip this.

### Cursor / Windsurf / Claude Desktop (JSON)

These clients use an `mcpServers` JSON config:

| Client | Config path |
|--------|-------------|
| **Cursor** | `~/.cursor/mcp.json` (global) or `.cursor/mcp.json` (project) |
| **Windsurf** | `~/.codeium/windsurf/mcp_config.json` |
| **Claude Desktop (macOS)** | `~/Library/Application Support/Claude/claude_desktop_config.json` |
| **Claude Desktop (Windows)** | `%APPDATA%\Claude\claude_desktop_config.json` |

**Consolidated (default):**

```json
{
  "mcpServers": {
    "contextstream": {
      "command": "npx",
      "args": ["-y", "@contextstream/mcp-server"],
      "env": {
        "CONTEXTSTREAM_API_URL": "https://api.contextstream.io",
        "CONTEXTSTREAM_API_KEY": "YOUR_API_KEY"
      }
    }
  }
}
```

**Router mode (~2 meta-tools):**

```json
{
  "mcpServers": {
    "contextstream": {
      "command": "npx",
      "args": ["-y", "@contextstream/mcp-server"],
      "env": {
        "CONTEXTSTREAM_API_URL": "https://api.contextstream.io",
        "CONTEXTSTREAM_API_KEY": "YOUR_API_KEY",
        "CONTEXTSTREAM_PROGRESSIVE_MODE": "true"
      }
    }
  }
}
```

### VS Code (`.vscode/mcp.json`)

VS Code uses a top-level `servers` map:

```json
{
  "servers": {
    "contextstream": {
      "type": "stdio",
      "command": "npx",
      "args": ["-y", "@contextstream/mcp-server"],
      "env": {
        "CONTEXTSTREAM_API_URL": "https://api.contextstream.io",
        "CONTEXTSTREAM_API_KEY": "YOUR_API_KEY"
      }
    }
  }
}
```

**Strong recommendation:** Use `inputs` so you don't commit secrets:

```json
{
  "servers": {
    "contextstream": {
      "type": "stdio",
      "command": "npx",
      "args": ["-y", "@contextstream/mcp-server"],
      "env": {
        "CONTEXTSTREAM_API_URL": "https://api.contextstream.io",
        "CONTEXTSTREAM_API_KEY": "${input:contextstreamApiKey}"
      }
    }
  },
  "inputs": [
    {
      "id": "contextstreamApiKey",
      "type": "promptString",
      "description": "ContextStream API Key",
      "password": true
    }
  ]
}
```

### Claude Code (CLI)

**User scope (all projects):**

```bash
claude mcp add --transport stdio contextstream --scope user \
  --env CONTEXTSTREAM_API_URL=https://api.contextstream.io \
  --env CONTEXTSTREAM_API_KEY=YOUR_KEY \
  -- npx -y @contextstream/mcp-server
```

**Router mode:**

```bash
claude mcp add --transport stdio contextstream --scope user \
  --env CONTEXTSTREAM_API_URL=https://api.contextstream.io \
  --env CONTEXTSTREAM_API_KEY=YOUR_KEY \
  --env CONTEXTSTREAM_PROGRESSIVE_MODE=true \
  -- npx -y @contextstream/mcp-server
```

> **Windows caveat** (native Windows, not WSL): if `npx` isn't found, use `cmd /c npx -y @contextstream/mcp-server` after `--`.

### Codex CLI (`~/.codex/config.toml`)

```toml
[mcp_servers.contextstream]
command = "npx"
args = ["-y", "@contextstream/mcp-server"]

[mcp_servers.contextstream.env]
CONTEXTSTREAM_API_URL = "https://api.contextstream.io"
CONTEXTSTREAM_API_KEY = "YOUR_API_KEY"
```

---

## Tool Overview (v0.4.x Consolidated)

In consolidated mode, you call **domain tools** with `action` / `mode`:

### Core

| Tool | Description |
|------|-------------|
| `session_init` | Initialize workspace/project context |
| `context_smart` | Retrieve the best bounded context for the current message |

### Domain Tools

| Tool | Description |
|------|-------------|
| `search` | `mode=semantic\|hybrid\|keyword\|pattern` |
| `session` | `action=capture\|recall\|remember\|get_lessons\|capture_lesson\|...` |
| `memory` | Events + nodes CRUD, decisions, lessons, etc. |
| `graph` | Dependencies, impact, call_path, ingest, etc. |
| `project` | Indexing, ingest_local, stats, files, etc. |
| `workspace` | List, get, associate, bootstrap |
| `reminder` | List, create, snooze, complete, dismiss |
| `integration` | `provider=slack\|github`, search, activity, etc. |
| `help` | Tools, auth, version, editor_rules |

### Examples

```
search(mode="semantic", query="auth middleware", limit=3)
memory(action="create_node", node_type="decision", title="Auth strategy", content="...")
graph(action="impact", target="UserService")
```

**Full tool catalog:** https://contextstream.io/docs/mcp/tools

---

## Authentication

Set **one** of:

| Variable | Use case |
|----------|----------|
| `CONTEXTSTREAM_API_KEY` | Recommended for local/dev |
| `CONTEXTSTREAM_JWT` | Useful for hosted/user-session flows |

---

## Environment Variables

### Required

| Variable | Description |
|----------|-------------|
| `CONTEXTSTREAM_API_URL` | Base API URL (default `https://api.contextstream.io`) |
| `CONTEXTSTREAM_API_KEY` | API key (unless using JWT) |
| `CONTEXTSTREAM_JWT` | JWT (unless using API key) |

### Token + Tool Surface Controls

| Variable | Description |
|----------|-------------|
| `CONTEXTSTREAM_CONSOLIDATED` | `true` (default in v0.4.x) uses consolidated domain tools |
| `CONTEXTSTREAM_PROGRESSIVE_MODE` | Enables Router mode (~2 meta-tools) |
| `CONTEXTSTREAM_CONTEXT_PACK` | Enable Context Pack for `context_smart` (code + graph + distill). Defaults to `true` |
| `CONTEXTSTREAM_TOOLSET` | Legacy granular tool bundle: `light` / `standard` / `complete` (only when consolidated is off) |
| `CONTEXTSTREAM_TOOL_ALLOWLIST` | Comma-separated tool names to expose (legacy granular mode) |
| `CONTEXTSTREAM_SCHEMA_MODE` | Reduce schema verbosity; e.g., `compact` |
| `CONTEXTSTREAM_OUTPUT_FORMAT` | Output formatting; e.g., `compact` / `pretty` |
| `CONTEXTSTREAM_SEARCH_LIMIT` | Default MCP search limit (default: 3) |
| `CONTEXTSTREAM_SEARCH_MAX_CHARS` | Max chars per search result content (default: 400) |

### Optional Defaults

| Variable | Description |
|----------|-------------|
| `CONTEXTSTREAM_WORKSPACE_ID` | Default workspace fallback |
| `CONTEXTSTREAM_PROJECT_ID` | Default project ID fallback |
| `CONTEXTSTREAM_USER_AGENT` | Custom user agent string |
| `CONTEXTSTREAM_PRO_TOOLS` | Comma-separated tool names treated as PRO |
| `CONTEXTSTREAM_UPGRADE_URL` | Upgrade link for Free users calling PRO tools |

---

## Migration Notes (pre-0.4.x → 0.4.x)

Most workflows **just work**, but tool names change in consolidated mode.

| Before (granular) | After (consolidated) |
|-------------------|----------------------|
| `search_semantic(query="auth")` | `search(mode="semantic", query="auth")` |
| `session_capture(...)` | `session(action="capture", ...)` |
| `graph_dependencies(...)` | `graph(action="dependencies", ...)` |

If you rely on granular tool names, you can temporarily set:

```bash
CONTEXTSTREAM_CONSOLIDATED=false
```

---

## Troubleshooting

| Issue | Solution |
|-------|----------|
| **Tools not appearing** | Restart the client after editing MCP config; confirm Node 18+ is available in the client runtime |
| **Unauthorized / 401** | Verify `CONTEXTSTREAM_API_URL` + `CONTEXTSTREAM_API_KEY` (or JWT) |
| **Wrong workspace/project** | Run `session_init` and/or associate your folder with the correct workspace |
| **Client warns about tool context size** | Use Router mode (`CONTEXTSTREAM_PROGRESSIVE_MODE=true`), or keep consolidated mode and reduce schema/output verbosity |

---

## Development

```bash
git clone https://github.com/contextstream/mcp-server.git
cd mcp-server
npm install
npm run dev
npm run typecheck
npm run build
```

---

## Links

| Resource | URL |
|----------|-----|
| **Website** | https://contextstream.io |
| **Docs** | https://contextstream.io/docs/mcp |
| **Tool Catalog** | https://contextstream.io/docs/mcp/tools |
| **Pricing** | https://contextstream.io/pricing |
| **npm** | https://www.npmjs.com/package/@contextstream/mcp-server |
| **GitHub** | https://github.com/contextstream/mcp-server |

---

## License

MIT
