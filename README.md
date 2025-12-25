# ContextStream MCP Server

Persistent memory, semantic search, and code intelligence for any MCP-compatible AI tool.

ContextStream is a shared "brain" for your AI workflows. It stores decisions, preferences, and lessons, and lets your AI tools search and analyze your codebase with consistent context across sessions.

## Just Ask

**You don't need to memorize tool names.** Just describe what you want and your AI uses the right ContextStream tools automatically:

| You say... | ContextStream does... |
|------------|----------------------|
| "session summary" | Gets a summary of your workspace context |
| "what did we decide about auth?" | Recalls past decisions about authentication |
| "remember we're using PostgreSQL" | Saves this to memory for future sessions |
| "search for payment code" | Searches your codebase semantically |
| "what depends on UserService?" | Analyzes code dependencies |

No special syntax. No commands to learn. Just ask.

> **Tip:** For best results, add the [recommended editor rules](https://contextstream.io/docs/quickstart) so your AI consistently calls `session_init` / `context_smart` automatically.

![ContextStream in action](compare1.gif)

## Features

- Session-aware context loading (`session_init`, `context_smart`)
- Memory capture and recall (decisions, preferences, tasks, bugs, lessons)
- Code search (semantic, hybrid, keyword, pattern)
- Knowledge graph and code analysis (dependencies, impact, call paths, circular deps, unused code)
- Local repo ingestion for indexing (`projects_ingest_local`)
- Auto-context: on the first tool call in a new session, the server can auto-initialize context

## Requirements

- Node.js 18+
- A ContextStream account and either an API key or a JWT

## Quickstart

### Setup wizard (recommended)

This interactive wizard sets up authentication, installs editor rules, and writes MCP config files for the tools you select.

```bash
npx -y @contextstream/mcp-server setup
```

Notes:
- Uses browser/device login by default and creates an API key for you.
- To avoid re-auth prompts on subsequent runs, the wizard saves that API key to `~/.contextstream/credentials.json` (and also writes it into the MCP config files it generates). Delete that file to force a fresh login.
- Codex CLI MCP config is global-only (`~/.codex/config.toml`), so the wizard will always write Codex config globally when selected.
- Some tools still require UI/CLI-based MCP setup (the wizard will tell you when it can’t write a config).
- Preview changes without writing files: `npx -y @contextstream/mcp-server setup --dry-run`

### Run the server

Run directly (recommended for MCP configs):

```bash
npx -y @contextstream/mcp-server
```

Or install globally:

```bash
npm install -g @contextstream/mcp-server
contextstream-mcp
```

## Configure your MCP client

### Manual setup

If you ran the [setup wizard](#setup-wizard-recommended), you can usually skip this section.

If you prefer to configure things by hand (or your tool can’t be auto-configured), add the ContextStream MCP server to your client using one of the examples below.

### Cursor / Windsurf / Claude Desktop (JSON)

These clients use the `mcpServers` JSON schema:

- Cursor: `~/.cursor/mcp.json` (global) or `.cursor/mcp.json` (project)
- Windsurf: `~/.codeium/windsurf/mcp_config.json`
- Claude Desktop:
  - macOS: `~/Library/Application Support/Claude/claude_desktop_config.json`
  - Windows: `%APPDATA%\\Claude\\claude_desktop_config.json`

Many other MCP JSON clients also use this same `mcpServers` shape (including Claude Code project scope via `.mcp.json`).

```json
{
  "mcpServers": {
    "contextstream": {
      "command": "npx",
      "args": ["-y", "@contextstream/mcp-server"],
      "env": {
        "CONTEXTSTREAM_API_URL": "https://api.contextstream.io",
        "CONTEXTSTREAM_API_KEY": "your_api_key"
      }
    }
  }
}
```

### VS Code (`.vscode/mcp.json`)

VS Code uses a different schema with a top-level `servers` map:

```json
{
  "servers": {
    "contextstream": {
      "type": "stdio",
      "command": "npx",
      "args": ["-y", "@contextstream/mcp-server"],
      "env": {
        "CONTEXTSTREAM_API_URL": "https://api.contextstream.io",
        "CONTEXTSTREAM_API_KEY": "your_api_key"
      }
    }
  }
}
```

Strong recommendation: VS Code supports `inputs` so you don’t have to hardcode secrets in a committed file:

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

User scope (all projects):

```bash
claude mcp add --transport stdio contextstream --scope user \
  --env CONTEXTSTREAM_API_URL=https://api.contextstream.io \
  --env CONTEXTSTREAM_API_KEY=YOUR_KEY \
  --env CONTEXTSTREAM_TOOLSET=core -- \
  npx -y @contextstream/mcp-server
```

Tip: Claude Code warns on large tool contexts. The default toolset is `core`.
Set `CONTEXTSTREAM_TOOLSET=full` to expose everything.

Windows caveat (native Windows, not WSL): if `npx` isn’t found, use `cmd /c npx -y @contextstream/mcp-server` after `--`.

Alternative (JSON form):

```bash
claude mcp add-json contextstream \
'{"type":"stdio","command":"npx","args":["-y","@contextstream/mcp-server"],"env":{"CONTEXTSTREAM_API_URL":"https://api.contextstream.io","CONTEXTSTREAM_API_KEY":"your_api_key","CONTEXTSTREAM_TOOLSET":"core"}}'
```

### Codex CLI (`~/.codex/config.toml`)

```toml
[mcp_servers.contextstream]
command = "npx"
args = ["-y", "@contextstream/mcp-server"]

[mcp_servers.contextstream.env]
CONTEXTSTREAM_API_URL = "https://api.contextstream.io"
CONTEXTSTREAM_API_KEY = "your_api_key"
```

After editing, restart your MCP client so it reloads the server configuration.

## Authentication

You can authenticate using either:

- `CONTEXTSTREAM_API_KEY` (recommended for local/dev)
- `CONTEXTSTREAM_JWT` (useful for hosted or user-session flows)

## Environment variables

| Variable | Required | Description |
|----------|----------|-------------|
| `CONTEXTSTREAM_API_URL` | Yes | Base API URL (e.g. `https://api.contextstream.io`) |
| `CONTEXTSTREAM_API_KEY` | Yes* | API key (*required unless `CONTEXTSTREAM_JWT` is set) |
| `CONTEXTSTREAM_JWT` | Yes* | JWT (*required unless `CONTEXTSTREAM_API_KEY` is set) |
| `CONTEXTSTREAM_WORKSPACE_ID` | No | Default workspace ID fallback |
| `CONTEXTSTREAM_PROJECT_ID` | No | Default project ID fallback |
| `CONTEXTSTREAM_USER_AGENT` | No | Custom user agent string |
| `CONTEXTSTREAM_TOOLSET` | No | Tool bundle to expose (`core` default, or `full`) |
| `CONTEXTSTREAM_TOOL_ALLOWLIST` | No | Comma-separated tool names to expose (overrides toolset) |
| `CONTEXTSTREAM_PRO_TOOLS` | No | Comma-separated tool names treated as PRO (default: `ai_context,ai_enhanced_context,ai_context_budget,ai_embeddings,ai_plan,ai_tasks`) |
| `CONTEXTSTREAM_UPGRADE_URL` | No | Upgrade link shown when Free users call PRO tools (default: `https://contextstream.io/pricing`) |

### Server-side environment variables (API)

The following environment variables are configured on the ContextStream API server (not in your MCP client config):

| Variable | Required | Description |
|----------|----------|-------------|
| `QA_FILE_WRITE_ROOT` | No | Server-side root directory for `write_to_disk` file writes. When set, the API allows the `projects_ingest_local` tool to write ingested files to disk for testing/QA purposes. Files are written under `<QA_FILE_WRITE_ROOT>/<project_id>/<relative_path>`. If not set, `write_to_disk` requests are rejected. |

#### File write parameters for `projects_ingest_local`

The `projects_ingest_local` tool accepts two optional parameters for QA/testing scenarios:

| Parameter | Type | Default | Description |
|-----------|------|---------|-------------|
| `write_to_disk` | boolean | `false` | When `true`, writes ingested files to disk on the API server under `QA_FILE_WRITE_ROOT` before indexing. Requires the API to have `QA_FILE_WRITE_ROOT` configured. |
| `overwrite` | boolean | `false` | When `true` (and `write_to_disk` is enabled), allows overwriting existing files. Otherwise, existing files are skipped. |

**Example usage:**
```json
{
  "path": "/path/to/local/project",
  "write_to_disk": true,
  "overwrite": false
}
```

**Note:** The `write_to_disk` feature is intended for testing, QA, and development scenarios where you need to materialize files on a test server. In production, `QA_FILE_WRITE_ROOT` should typically be unset to disable file writes.

## Usage patterns

### Recommended flow for AI tools

1. Start of a conversation: call `session_init(folder_path="...", context_hint="<first user message>")`
2. Before subsequent responses: call `context_smart(user_message="<current user message>")`
3. After important outcomes: call `session_capture(...)` or `session_capture_lesson(...)`

### Omit workspace/project IDs (recommended)

Most tools accept omitted `workspace_id` / `project_id` and will use the current session defaults.

- If you see “workspace_id is required”, call `session_init` first (or pass the ID explicitly).
- If you regularly work in the same repo, use `workspace_associate` once so the server can auto-select the right workspace for that folder.

### First-time setup (no workspaces yet)

If your account has no workspaces, ContextStream will prompt your AI assistant to ask you for a workspace name.

- Provide a workspace name (e.g., your company/team/product)
- The current folder is created as a project inside that workspace
- Recommended: call `workspace_bootstrap(workspace_name="...", folder_path="...")`

## Free vs PRO tools

Tools are labeled as `(Free)` or `(PRO)` in the MCP tool list.

- Default PRO tools: `ai_context`, `ai_enhanced_context`, `ai_context_budget`, `ai_embeddings`, `ai_plan`, `ai_tasks`
- If a Free-plan user calls a PRO tool, the server returns an upgrade message with a link.
- Override the PRO list via `CONTEXTSTREAM_PRO_TOOLS` and the upgrade link via `CONTEXTSTREAM_UPGRADE_URL`.

## Troubleshooting

- Tools not appearing: restart the client after editing MCP config; confirm Node 18+ is available to the client runtime.
- Unauthorized errors: verify `CONTEXTSTREAM_API_URL` and `CONTEXTSTREAM_API_KEY` (or `CONTEXTSTREAM_JWT`).
- Wrong workspace/project: use `workspace_associate` to map the current repo folder to the correct workspace.

## Development

```bash
git clone https://github.com/contextstream/mcp-server.git
cd mcp-server
npm install
npm run dev
npm run typecheck
npm run build
```

## Links

- Website: https://contextstream.io
- Docs: https://contextstream.io/docs/mcp
- Pricing: https://contextstream.io/pricing
- npm: https://www.npmjs.com/package/@contextstream/mcp-server
- GitHub: https://github.com/contextstream/mcp-server

## License

MIT
