import { McpServer } from '@modelcontextprotocol/sdk/server/mcp.js';
import { StdioServerTransport } from '@modelcontextprotocol/sdk/server/stdio.js';
import { loadConfig } from './config.js';
import { ContextStreamClient } from './client.js';
import { registerTools, setupClientDetection } from './tools.js';
import { registerResources } from './resources.js';
import { registerPrompts } from './prompts.js';
import { SessionManager } from './session-manager.js';
import { runHttpGateway } from './http-gateway.js';
import { existsSync, mkdirSync, writeFileSync } from 'fs';
import { homedir } from 'os';
import { join } from 'path';
import { VERSION, checkForUpdates } from './version.js';
import { runSetupWizard } from './setup.js';

const ENABLE_PROMPTS = (process.env.CONTEXTSTREAM_ENABLE_PROMPTS || 'true').toLowerCase() !== 'false';

/**
 * Check if this is the first run and show star message if so.
 * Only shows once per install to avoid being annoying.
 */
function showFirstRunMessage(): void {
  const configDir = join(homedir(), '.contextstream');
  const starShownFile = join(configDir, '.star-shown');

  // Check if we've already shown the message
  if (existsSync(starShownFile)) {
    return;
  }

  // Create config directory if it doesn't exist
  if (!existsSync(configDir)) {
    try {
      mkdirSync(configDir, { recursive: true });
    } catch {
      // If we can't create the directory, skip the message
      return;
    }
  }

  // Show the star message
  console.error('');
  console.error('━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━');
  console.error('⭐ If ContextStream saves you time, please star the MCP server repo:');
  console.error('   https://github.com/contextstream/mcp-server');
  console.error('   It helps others discover it!');
  console.error('━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━');
  console.error('');

  // Mark as shown
  try {
    writeFileSync(starShownFile, new Date().toISOString());
  } catch {
    // Ignore write errors
  }
}

function printHelp() {
  // Keep help output on stdout so it is visible when run via npx
  console.log(`ContextStream MCP Server (contextstream-mcp) v${VERSION}

Usage:
  npx -y @contextstream/mcp-server
  contextstream-mcp
  contextstream-mcp setup
  contextstream-mcp http

Commands:
  setup   Interactive onboarding wizard (rules + workspace mapping)
  http    Run HTTP MCP gateway (streamable HTTP transport)

Environment variables:
  CONTEXTSTREAM_API_URL   Base API URL (e.g. https://api.contextstream.io)
  CONTEXTSTREAM_API_KEY   API key for authentication (or use CONTEXTSTREAM_JWT)
  CONTEXTSTREAM_JWT       JWT for authentication (alternative to API key)
  CONTEXTSTREAM_ALLOW_HEADER_AUTH  Allow header-based auth when no API key/JWT is set
  CONTEXTSTREAM_WORKSPACE_ID  Optional default workspace ID
  CONTEXTSTREAM_PROJECT_ID    Optional default project ID
  CONTEXTSTREAM_TOOLSET       Tool mode: light|standard|complete (default: standard)
  CONTEXTSTREAM_TOOL_ALLOWLIST Optional comma-separated tool names to expose (overrides toolset)
  CONTEXTSTREAM_AUTO_TOOLSET  Auto-detect client and adjust toolset (default: false)
  CONTEXTSTREAM_AUTO_HIDE_INTEGRATIONS  Auto-hide Slack/GitHub tools when not connected (default: true)
  CONTEXTSTREAM_SCHEMA_MODE   Schema verbosity: compact|full (default: full, compact reduces tokens)
  CONTEXTSTREAM_PROGRESSIVE_MODE  Progressive disclosure: true|false (default: false, starts with ~13 core tools)
  CONTEXTSTREAM_ROUTER_MODE   Router pattern: true|false (default: false, exposes only 2 meta-tools)
  CONTEXTSTREAM_OUTPUT_FORMAT Output verbosity: compact|pretty (default: compact, ~30% fewer tokens)
  CONTEXTSTREAM_PRO_TOOLS     Optional comma-separated PRO tool names (default: AI tools)
  CONTEXTSTREAM_UPGRADE_URL   Optional upgrade URL shown for PRO tools on Free plan
  CONTEXTSTREAM_ENABLE_PROMPTS Enable MCP prompts list (default: true)
  MCP_HTTP_HOST          HTTP gateway host (default: 0.0.0.0)
  MCP_HTTP_PORT          HTTP gateway port (default: 8787)
  MCP_HTTP_PATH          HTTP gateway path (default: /mcp)
  MCP_HTTP_REQUIRE_AUTH  Require auth headers for HTTP gateway (default: true)
  MCP_HTTP_JSON_RESPONSE Enable JSON responses (default: false)

Examples:
  CONTEXTSTREAM_API_URL="https://api.contextstream.io" \\
  CONTEXTSTREAM_API_KEY="your_api_key" \\
  npx -y @contextstream/mcp-server

Setup wizard:
  npx -y @contextstream/mcp-server setup

Notes:
  - When used from an MCP client (e.g. Codex, Cursor, VS Code),
    set these env vars in the client's MCP server configuration.
  - The server communicates over stdio; logs are written to stderr.`);
}

async function main() {
  const args = process.argv.slice(2);

  if (args.includes('--help') || args.includes('-h')) {
    printHelp();
    return;
  }

  if (args.includes('--version') || args.includes('-v')) {
    console.log(`contextstream-mcp v${VERSION}`);
    return;
  }

  if (args[0] === 'setup') {
    await runSetupWizard(args.slice(1));
    return;
  }

  if (args[0] === 'http') {
    if (
      !process.env.CONTEXTSTREAM_API_KEY &&
      !process.env.CONTEXTSTREAM_JWT &&
      !process.env.CONTEXTSTREAM_ALLOW_HEADER_AUTH
    ) {
      process.env.CONTEXTSTREAM_ALLOW_HEADER_AUTH = 'true';
    }
    await runHttpGateway();
    return;
  }

  const config = loadConfig();
  const client = new ContextStreamClient(config);

  const server = new McpServer({
    name: 'contextstream-mcp',
    version: VERSION,
  });

  // Set up client detection callback (Strategy 3 - Option B Primary)
  // This will detect token-sensitive clients (Claude Code, Claude Desktop) on MCP initialize
  setupClientDetection(server);

  // Create session manager for auto-context feature
  // This enables automatic context loading on the FIRST tool call of any session
  const sessionManager = new SessionManager(server, client);

  // Register all MCP components with auto-context enabled
  registerTools(server, client, sessionManager);
  registerResources(server, client, config.apiUrl);
  if (ENABLE_PROMPTS) {
    registerPrompts(server);
  }

  // Log startup info (to stderr to not interfere with stdio protocol)
  console.error(`ContextStream MCP server v${VERSION} starting...`);
  console.error(`API URL: ${config.apiUrl}`);
  console.error(`Auth: ${config.apiKey ? 'API Key' : config.jwt ? 'JWT' : 'None'}`);
  console.error(`Auto-Context: ENABLED (context loads on first tool call)`);

  // Start stdio transport (works with Claude Code, Cursor, VS Code MCP config, Inspector)
  const transport = new StdioServerTransport();
  await server.connect(transport);

  console.error('ContextStream MCP server connected and ready');

  // Show first-run star message (only once per install)
  showFirstRunMessage();

  // Check for updates in the background (non-blocking)
  checkForUpdates().catch(() => {
    // Silently ignore update check errors
  });
}

main().catch((err) => {
  console.error('ContextStream MCP server failed to start:', err?.message || err);
  process.exit(1);
});
