import { McpServer } from "@modelcontextprotocol/sdk/server/mcp.js";
import { StdioServerTransport } from "@modelcontextprotocol/sdk/server/stdio.js";
import { loadConfig, isMissingCredentialsError } from "./config.js";
import { ContextStreamClient } from "./client.js";
import { registerTools, setupClientDetection, registerLimitedTools } from "./tools.js";
import { registerResources } from "./resources.js";
import { registerPrompts } from "./prompts.js";
import { SessionManager } from "./session-manager.js";
import { runHttpGateway } from "./http-gateway.js";
import { existsSync, mkdirSync, writeFileSync } from "fs";
import { homedir } from "os";
import { join } from "path";
import { VERSION, checkForUpdates } from "./version.js";
import { runSetupWizard } from "./setup.js";
import { readSavedCredentials } from "./credentials.js";

const ENABLE_PROMPTS =
  (process.env.CONTEXTSTREAM_ENABLE_PROMPTS || "true").toLowerCase() !== "false";

/**
 * Check if this is the first run and show star message if so.
 * Only shows once per install to avoid being annoying.
 */
function showFirstRunMessage(): void {
  const configDir = join(homedir(), ".contextstream");
  const starShownFile = join(configDir, ".star-shown");

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
  console.error("");
  console.error("━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━");
  console.error("⭐ If ContextStream saves you time, please star the MCP server repo:");
  console.error("   https://github.com/contextstream/mcp-server");
  console.error("   It helps others discover it!");
  console.error("━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━");
  console.error("");

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
  npx --prefer-online -y @contextstream/mcp-server@latest
  contextstream-mcp
  contextstream-mcp setup
  contextstream-mcp http
  contextstream-mcp hook <hook-name>

Commands:
  setup                      Interactive onboarding wizard (rules + workspace mapping)
  verify-key [--json]        Verify API key and show account info
  update-hooks [flags]       Update hooks for all editors (Claude, Cursor, Cline, Roo, Kilo)
    --scope=global           Install hooks globally (default)
    --scope=project, -p      Install hooks for current project only
    --path=/path             Specify project path (implies --scope=project)
  http                       Run HTTP MCP gateway (streamable HTTP transport)
  hook pre-tool-use          PreToolUse hook - blocks discovery tools, redirects to ContextStream
  hook user-prompt-submit    UserPromptSubmit hook - injects ContextStream rules reminder
  hook media-aware           Media-aware hook - detects media prompts, injects media tool guidance
  hook pre-compact           PreCompact hook - saves conversation state before compaction
  hook post-compact          PostCompact hook - restores context after compaction
  hook post-write            PostToolUse hook - real-time file indexing after Edit/Write
  hook auto-rules            PostToolUse hook - auto-updates rules when behind (silent)
  hook on-bash               PostToolUse hook - captures bash commands, learns from errors
  hook on-task               PostToolUse hook - tracks Task agent work
  hook on-read               PostToolUse hook - tracks file exploration (Read/Glob/Grep)
  hook on-web                PostToolUse hook - captures web research (WebFetch/WebSearch)
  hook session-init          SessionStart hook - full context injection on session start
  hook session-end           Stop hook - finalizes session, saves state
  hook on-save-intent        UserPromptSubmit hook - redirects doc saves to ContextStream

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
  CONTEXTSTREAM_SEARCH_LIMIT  Default MCP search limit (default: 3)
  CONTEXTSTREAM_SEARCH_MAX_CHARS  Max chars per search result content (default: 400)
  CONTEXTSTREAM_CONSOLIDATED  Consolidated domain tools: true|false (default: true in v0.4.x, ~75% token reduction)
  CONTEXTSTREAM_CONTEXT_PACK  Enable Context Pack in context_smart: true|false (default: true)
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
  npx --prefer-online -y @contextstream/mcp-server@latest

Setup wizard:
  npx --prefer-online -y @contextstream/mcp-server@latest setup

Notes:
  - When used from an MCP client (e.g. Codex, Cursor, VS Code),
    set these env vars in the client's MCP server configuration.
  - The server communicates over stdio; logs are written to stderr.`);
}

/**
 * Run the MCP server in limited mode (no credentials).
 * Only exposes a setup helper tool so users know how to configure.
 */
async function runLimitedModeServer(): Promise<void> {
  const server = new McpServer({
    name: "contextstream-mcp",
    version: VERSION,
  });

  registerLimitedTools(server);

  console.error(`ContextStream MCP server v${VERSION} (limited mode)`);
  console.error('Run "npx --prefer-online -y @contextstream/mcp-server@latest setup" to enable all tools.');

  const transport = new StdioServerTransport();
  await server.connect(transport);

  console.error("ContextStream MCP server connected (limited mode - setup required)");
}

async function main() {
  const args = process.argv.slice(2);

  if (args.includes("--help") || args.includes("-h")) {
    printHelp();
    return;
  }

  if (args.includes("--version") || args.includes("-v")) {
    console.log(`contextstream-mcp v${VERSION}`);
    return;
  }

  if (args[0] === "setup") {
    await runSetupWizard(args.slice(1));
    return;
  }

  if (args[0] === "http") {
    if (
      !process.env.CONTEXTSTREAM_API_KEY &&
      !process.env.CONTEXTSTREAM_JWT &&
      !process.env.CONTEXTSTREAM_ALLOW_HEADER_AUTH
    ) {
      process.env.CONTEXTSTREAM_ALLOW_HEADER_AUTH = "true";
    }
    await runHttpGateway();
    return;
  }

  // Hook command: runs editor hooks (Node.js-based, no Python dependency)
  if (args[0] === "hook") {
    const hookName = args[1];
    switch (hookName) {
      case "post-write": {
        const { runPostWriteHook } = await import("./hooks/post-write.js");
        await runPostWriteHook();
        return;
      }
      case "pre-tool-use": {
        const { runPreToolUseHook } = await import("./hooks/pre-tool-use.js");
        await runPreToolUseHook();
        return;
      }
      case "user-prompt-submit": {
        const { runUserPromptSubmitHook } = await import("./hooks/user-prompt-submit.js");
        await runUserPromptSubmitHook();
        return;
      }
      case "media-aware": {
        const { runMediaAwareHook } = await import("./hooks/media-aware.js");
        await runMediaAwareHook();
        return;
      }
      case "pre-compact": {
        const { runPreCompactHook } = await import("./hooks/pre-compact.js");
        await runPreCompactHook();
        return;
      }
      case "auto-rules": {
        const { runAutoRulesHook } = await import("./hooks/auto-rules.js");
        await runAutoRulesHook();
        return;
      }
      case "post-compact": {
        const { runPostCompactHook } = await import("./hooks/post-compact.js");
        await runPostCompactHook();
        return;
      }
      case "on-bash": {
        const { runOnBashHook } = await import("./hooks/on-bash.js");
        await runOnBashHook();
        return;
      }
      case "on-task": {
        const { runOnTaskHook } = await import("./hooks/on-task.js");
        await runOnTaskHook();
        return;
      }
      case "on-read": {
        const { runOnReadHook } = await import("./hooks/on-read.js");
        await runOnReadHook();
        return;
      }
      case "on-web": {
        const { runOnWebHook } = await import("./hooks/on-web.js");
        await runOnWebHook();
        return;
      }
      case "session-init": {
        const { runSessionInitHook } = await import("./hooks/session-init.js");
        await runSessionInitHook();
        return;
      }
      case "session-end": {
        const { runSessionEndHook } = await import("./hooks/session-end.js");
        await runSessionEndHook();
        return;
      }
      case "on-save-intent": {
        const { runOnSaveIntentHook } = await import("./hooks/on-save-intent.js");
        await runOnSaveIntentHook();
        return;
      }
      default:
        console.error(`Unknown hook: ${hookName}`);
        console.error("Available hooks: pre-tool-use, user-prompt-submit, media-aware, pre-compact, post-compact, post-write, auto-rules, on-bash, on-task, on-read, on-web, session-init, session-end, on-save-intent");
        process.exit(1);
    }
  }

  // Verify API key command: validate key and show account info
  // Usage: contextstream-mcp verify-key [--json]
  if (args[0] === "verify-key") {
    const { runVerifyKey } = await import("./verify-key.js");
    const outputJson = args.includes("--json");
    const result = await runVerifyKey(outputJson);
    process.exit(result.valid ? 0 : 1);
  }

  // Update hooks command: non-interactive hook installation for all editors
  // Usage: contextstream-mcp update-hooks [--scope=global|project] [--path=/project/path]
  if (args[0] === "update-hooks") {
    const { installAllEditorHooks } = await import("./hooks-config.js");

    // Parse flags
    let scope: "global" | "project" = "global";
    let projectPath: string | undefined;

    for (const arg of args.slice(1)) {
      if (arg === "--scope=project" || arg === "-p") {
        scope = "project";
        projectPath = projectPath || process.cwd();
      } else if (arg === "--scope=global" || arg === "-g") {
        scope = "global";
      } else if (arg.startsWith("--path=")) {
        projectPath = arg.replace("--path=", "");
        scope = "project";
      }
    }

    const scopeLabel = scope === "project" ? `project (${projectPath || process.cwd()})` : "global";
    console.error(`Updating hooks for all editors (${scopeLabel})...`);

    try {
      const results = await installAllEditorHooks({
        scope,
        projectPath: scope === "project" ? (projectPath || process.cwd()) : undefined
      });
      for (const result of results) {
        console.error(`✓ ${result.editor}: ${result.installed.length} hooks installed`);
      }
      console.error("✓ Hooks updated successfully");
    } catch (error) {
      console.error("Failed to update hooks:", error);
      process.exit(1);
    }
    return;
  }

  // Try to load saved credentials if env vars not set
  if (!process.env.CONTEXTSTREAM_API_KEY && !process.env.CONTEXTSTREAM_JWT) {
    const saved = await readSavedCredentials();
    if (saved) {
      process.env.CONTEXTSTREAM_API_URL = saved.api_url;
      process.env.CONTEXTSTREAM_API_KEY = saved.api_key;
    }
  }

  // Try to load config - may fail if still no credentials
  let config;
  try {
    config = loadConfig();
  } catch (err) {
    if (isMissingCredentialsError(err)) {
      // Run limited mode server instead of exiting with error
      await runLimitedModeServer();
      return;
    }
    throw err;
  }

  const client = new ContextStreamClient(config);

  const server = new McpServer({
    name: "contextstream-mcp",
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

  // Log startup info (respects CONTEXTSTREAM_LOG_LEVEL)
  const logLevel = (process.env.CONTEXTSTREAM_LOG_LEVEL || "normal").toLowerCase();
  const logQuiet = logLevel === "quiet";
  const logVerbose = logLevel === "verbose";

  if (!logQuiet) {
    console.error(`━━━ ContextStream v${VERSION} ━━━`);
  }
  if (logVerbose) {
    console.error(`  API: ${config.apiUrl}`);
    console.error(`  Auth: ${config.apiKey ? "API Key" : config.jwt ? "JWT" : "None"}`);
  }

  // Start stdio transport (works with Claude Code, Cursor, VS Code MCP config, Inspector)
  const transport = new StdioServerTransport();
  await server.connect(transport);

  if (!logQuiet) {
    console.error("✓ ready");
  }

  // Show first-run star message (only once per install)
  showFirstRunMessage();

  // Check for updates in the background (non-blocking)
  checkForUpdates().catch(() => {
    // Silently ignore update check errors
  });
}

main().catch((err) => {
  console.error("ContextStream MCP server failed to start:", err?.message || err);
  process.exit(1);
});
