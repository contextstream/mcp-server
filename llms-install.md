# ContextStream MCP Server — LLM Installation Guide

This guide is for AI agents (Cline, Claude, Cursor agents, etc.) installing the ContextStream MCP server on a user's behalf.

ContextStream gives AI coding assistants persistent memory and cross-session learning: semantic code search, knowledge graphs, decisions, lessons, and session transcripts that survive across sessions and tools.

## Prerequisites

- A ContextStream API key. The user can create a free account at <https://contextstream.io> and copy the key from the dashboard (Settings → API Keys). If the user does not have a key yet, pause and ask them to create one — do not invent a value.
- For Option 1 (npx): Node.js 18+ available on PATH.

## Option 1 — npx (stdio, recommended for Cline)

Add this to the MCP settings file (for Cline: `cline_mcp_settings.json`):

```json
{
  "mcpServers": {
    "contextstream": {
      "command": "npx",
      "args": ["--prefer-online", "-y", "@contextstream/mcp-server@latest"],
      "env": {
        "CONTEXTSTREAM_API_URL": "https://api.contextstream.io",
        "CONTEXTSTREAM_API_KEY": "USER_API_KEY_HERE"
      }
    }
  }
}
```

Replace `USER_API_KEY_HERE` with the user's real API key.

## Option 2 — Hosted remote (streamable HTTP, zero install)

For clients that support remote MCP servers with OAuth (VS Code, Claude, etc.), no local install or API key in config is needed:

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

The endpoint uses built-in OAuth — the client will open a browser window for the user to authorize on first use.

## Verify the installation

After the server connects, call the `init` tool (no arguments needed beyond the project folder path if available). A successful response starts with `Session ready for workspace: …`. If tools do not appear, restart the MCP client.

## Troubleshooting

- **401 Unauthorized**: `CONTEXTSTREAM_API_KEY` is missing, mistyped, or revoked. Ask the user to re-copy it from the dashboard.
- **npx download issues**: run `npx -y @contextstream/mcp-server@latest --version` in a terminal to pre-warm the cache.
- **Corporate proxy/firewall**: the server needs outbound HTTPS to `api.contextstream.io` (Option 1) or `mcp.contextstream.io` (Option 2).

More documentation: <https://contextstream.io/docs/mcp>
