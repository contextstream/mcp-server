import { McpServer } from '@modelcontextprotocol/sdk/server/mcp.js';
import { StdioServerTransport } from '@modelcontextprotocol/sdk/server/stdio.js';
import { loadConfig } from './config.js';
import { ContextStreamClient } from './client.js';
import { registerTools } from './tools.js';
import { registerResources } from './resources.js';
// Note: Prompts are disabled as they're confusing - users can just use natural language directly
// import { registerPrompts } from './prompts.js';
import { SessionManager } from './session-manager.js';
import { existsSync, mkdirSync, writeFileSync } from 'fs';
import { homedir } from 'os';
import { join } from 'path';
import { VERSION } from './version.js';
import { runSetupWizard } from './setup.js';

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

Commands:
  setup   Interactive onboarding wizard (rules + workspace mapping)

Environment variables:
  CONTEXTSTREAM_API_URL   Base API URL (e.g. https://api.contextstream.io)
  CONTEXTSTREAM_API_KEY   API key for authentication (or use CONTEXTSTREAM_JWT)
  CONTEXTSTREAM_JWT       JWT for authentication (alternative to API key)
  CONTEXTSTREAM_WORKSPACE_ID  Optional default workspace ID
  CONTEXTSTREAM_PROJECT_ID    Optional default project ID
  CONTEXTSTREAM_TOOLSET       Optional tool bundle (core|full). Defaults to core to reduce tool context size.
  CONTEXTSTREAM_TOOL_ALLOWLIST Optional comma-separated tool names to expose (overrides toolset)
  CONTEXTSTREAM_PRO_TOOLS     Optional comma-separated PRO tool names (default: AI tools)
  CONTEXTSTREAM_UPGRADE_URL   Optional upgrade URL shown for PRO tools on Free plan

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

  const config = loadConfig();
  const client = new ContextStreamClient(config);

  const server = new McpServer({
    name: 'contextstream-mcp',
    version: VERSION,
  });

  // Create session manager for auto-context feature
  // This enables automatic context loading on the FIRST tool call of any session
  const sessionManager = new SessionManager(server, client);

  // Register all MCP components with auto-context enabled
  registerTools(server, client, sessionManager);
  registerResources(server, client, config.apiUrl);
  // registerPrompts(server); // Disabled - users can just use natural language directly

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
}

main().catch((err) => {
  console.error('ContextStream MCP server failed to start:', err?.message || err);
  process.exit(1);
});
