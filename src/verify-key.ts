/**
 * ContextStream API Key Verification
 *
 * Validates an API key against the ContextStream API and returns account info.
 * Used by install scripts to confirm API key before configuring hooks.
 *
 * Usage:
 *   contextstream-mcp verify-key [--json]
 *
 * Output (default):
 *   API Key: cs_abc...xyz
 *   Account: user@example.com
 *   Plan: Pro
 *   Status: Valid
 *
 * Output (--json):
 *   {"valid":true,"masked_key":"cs_abc...xyz","email":"user@example.com","plan":"pro"}
 */

import * as fs from "node:fs";
import * as path from "node:path";
import { homedir } from "node:os";

interface McpServerConfig {
  env?: {
    CONTEXTSTREAM_API_KEY?: string;
    CONTEXTSTREAM_API_URL?: string;
  };
}

interface McpConfig {
  mcpServers?: {
    [key: string]: McpServerConfig | undefined;
  };
}

interface CredentialsFile {
  api_key?: string;
  api_url?: string;
}

interface AccountInfo {
  valid: boolean;
  masked_key: string;
  email?: string;
  name?: string;
  plan?: string;
  workspace_name?: string;
  error?: string;
}

/**
 * Mask an API key for safe display/logging.
 * Only reveals the type prefix (e.g., "cs_") and last 3 characters.
 * @security Output is safe to log - only contains non-sensitive prefix and suffix.
 * @codeql-sanitizer js/clear-text-logging
 */
function maskApiKey(key: string): string {
  if (!key || key.length < 8) return "***";

  // Extract only the type prefix and last 3 chars
  // lgtm[js/clear-text-logging] - This function intentionally sanitizes the key
  const prefixMatch = key.match(/^([a-z]{2,3}_)/i);
  const prefix = prefixMatch ? prefixMatch[1] : "";
  const suffix = key.slice(-3);

  return `${prefix}***...${suffix}`;
}

/**
 * Extract ContextStream API key from an MCP config object.
 * Handles various server name variations and searches all servers.
 */
function extractFromMcpConfig(config: McpConfig): { apiKey?: string; apiUrl?: string } {
  if (!config.mcpServers) return {};

  // First try common server name variations
  const priorityNames = ["contextstream", "ContextStream", "context-stream"];
  for (const name of priorityNames) {
    const server = config.mcpServers[name];
    if (server?.env?.CONTEXTSTREAM_API_KEY) {
      return {
        apiKey: server.env.CONTEXTSTREAM_API_KEY,
        apiUrl: server.env.CONTEXTSTREAM_API_URL,
      };
    }
  }

  // Then search all servers for CONTEXTSTREAM_API_KEY
  for (const [, server] of Object.entries(config.mcpServers)) {
    if (server?.env?.CONTEXTSTREAM_API_KEY) {
      return {
        apiKey: server.env.CONTEXTSTREAM_API_KEY,
        apiUrl: server.env.CONTEXTSTREAM_API_URL,
      };
    }
  }

  return {};
}

/**
 * Get platform-specific Claude Desktop config path.
 */
function getClaudeDesktopConfigPath(): string {
  const platform = process.platform;
  if (platform === "darwin") {
    return path.join(homedir(), "Library", "Application Support", "Claude", "claude_desktop_config.json");
  } else if (platform === "win32") {
    return path.join(process.env.APPDATA || "", "Claude", "claude_desktop_config.json");
  } else {
    return path.join(homedir(), ".config", "Claude", "claude_desktop_config.json");
  }
}

function loadApiKey(): { apiKey: string | null; apiUrl: string; source: string } {
  let apiKey: string | null = null;
  let apiUrl = "https://api.contextstream.io";
  let source = "none";

  // Priority 1: Environment variables (explicit override)
  if (process.env.CONTEXTSTREAM_API_KEY) {
    apiKey = process.env.CONTEXTSTREAM_API_KEY;
    source = "environment";
    if (process.env.CONTEXTSTREAM_API_URL) {
      apiUrl = process.env.CONTEXTSTREAM_API_URL;
    }
    return { apiKey, apiUrl, source };
  }

  // Priority 2: Project .mcp.json (check cwd and parents - what editors actually use)
  let searchDir = process.cwd();
  for (let i = 0; i < 5; i++) {
    const projectMcpPath = path.join(searchDir, ".mcp.json");
    if (fs.existsSync(projectMcpPath)) {
      try {
        const content = fs.readFileSync(projectMcpPath, "utf-8");
        const config = JSON.parse(content) as McpConfig;
        const extracted = extractFromMcpConfig(config);
        if (extracted.apiKey) {
          apiKey = extracted.apiKey;
          source = `${projectMcpPath}`;
          if (extracted.apiUrl) {
            apiUrl = extracted.apiUrl;
          }
          return { apiKey, apiUrl, source };
        }
      } catch {
        // Continue
      }
    }
    const parentDir = path.dirname(searchDir);
    if (parentDir === searchDir) break;
    searchDir = parentDir;
  }

  // Priority 3: ~/.mcp.json (Claude Code global)
  const globalMcpPath = path.join(homedir(), ".mcp.json");
  if (fs.existsSync(globalMcpPath)) {
    try {
      const content = fs.readFileSync(globalMcpPath, "utf-8");
      const config = JSON.parse(content) as McpConfig;
      const extracted = extractFromMcpConfig(config);
      if (extracted.apiKey) {
        apiKey = extracted.apiKey;
        source = "~/.mcp.json";
        if (extracted.apiUrl) {
          apiUrl = extracted.apiUrl;
        }
        return { apiKey, apiUrl, source };
      }
    } catch {
      // Continue to next source
    }
  }

  // Priority 4: Cursor config locations
  const cursorPaths = [
    path.join(process.cwd(), ".cursor", "mcp.json"),
    path.join(homedir(), ".cursor", "mcp.json"),
  ];
  for (const cursorPath of cursorPaths) {
    if (fs.existsSync(cursorPath)) {
      try {
        const content = fs.readFileSync(cursorPath, "utf-8");
        const config = JSON.parse(content) as McpConfig;
        const extracted = extractFromMcpConfig(config);
        if (extracted.apiKey) {
          apiKey = extracted.apiKey;
          source = cursorPath;
          if (extracted.apiUrl) {
            apiUrl = extracted.apiUrl;
          }
          return { apiKey, apiUrl, source };
        }
      } catch {
        // Continue
      }
    }
  }

  // Priority 5: Claude Desktop config
  const claudeDesktopPath = getClaudeDesktopConfigPath();
  if (fs.existsSync(claudeDesktopPath)) {
    try {
      const content = fs.readFileSync(claudeDesktopPath, "utf-8");
      const config = JSON.parse(content) as McpConfig;
      const extracted = extractFromMcpConfig(config);
      if (extracted.apiKey) {
        apiKey = extracted.apiKey;
        source = claudeDesktopPath;
        if (extracted.apiUrl) {
          apiUrl = extracted.apiUrl;
        }
        return { apiKey, apiUrl, source };
      }
    } catch {
      // Continue
    }
  }

  // Priority 6: VS Code / Windsurf / Cline settings
  const vscodePaths = [
    path.join(homedir(), ".vscode", "mcp.json"),
    path.join(homedir(), ".codeium", "windsurf", "mcp_config.json"),
    path.join(homedir(), ".continue", "config.json"),
  ];
  for (const vsPath of vscodePaths) {
    if (fs.existsSync(vsPath)) {
      try {
        const content = fs.readFileSync(vsPath, "utf-8");
        const config = JSON.parse(content) as McpConfig;
        const extracted = extractFromMcpConfig(config);
        if (extracted.apiKey) {
          apiKey = extracted.apiKey;
          source = vsPath;
          if (extracted.apiUrl) {
            apiUrl = extracted.apiUrl;
          }
          return { apiKey, apiUrl, source };
        }
      } catch {
        // Continue
      }
    }
  }

  // Priority 7: ~/.contextstream/credentials.json (fallback from setup wizard)
  const credentialsPath = path.join(homedir(), ".contextstream", "credentials.json");
  if (fs.existsSync(credentialsPath)) {
    try {
      const content = fs.readFileSync(credentialsPath, "utf-8");
      const creds = JSON.parse(content) as CredentialsFile;
      if (creds.api_key) {
        apiKey = creds.api_key;
        source = "~/.contextstream/credentials.json";
        if (creds.api_url) {
          apiUrl = creds.api_url;
        }
        return { apiKey, apiUrl, source };
      }
    } catch {
      // Continue
    }
  }

  return { apiKey, apiUrl, source };
}

async function validateApiKey(apiKey: string, apiUrl: string): Promise<AccountInfo> {
  try {
    const controller = new AbortController();
    const timeoutId = setTimeout(() => controller.abort(), 10000);

    const response = await fetch(`${apiUrl}/api/v1/auth/me`, {
      method: "GET",
      headers: {
        "X-API-Key": apiKey,
      },
      signal: controller.signal,
    });

    clearTimeout(timeoutId);

    if (response.ok) {
      const data = (await response.json()) as {
        email?: string;
        name?: string;
        full_name?: string;
        plan?: string;
        plan_name?: string;
        workspace?: { name?: string };
      };

      return {
        valid: true,
        masked_key: maskApiKey(apiKey), // lgtm[js/clear-text-logging]
        email: data.email,
        name: data.name || data.full_name,
        plan: data.plan_name || data.plan || "free",
        workspace_name: data.workspace?.name,
      };
    } else if (response.status === 401) {
      return {
        valid: false,
        masked_key: maskApiKey(apiKey), // lgtm[js/clear-text-logging]
        error: "Invalid or expired API key",
      };
    } else {
      return {
        valid: false,
        masked_key: maskApiKey(apiKey), // lgtm[js/clear-text-logging]
        error: `API error: ${response.status}`,
      };
    }
  } catch (error) {
    return {
      valid: false,
      masked_key: maskApiKey(apiKey),
      error: `Connection error: ${error instanceof Error ? error.message : String(error)}`,
    };
  }
}

export async function runVerifyKey(outputJson: boolean): Promise<AccountInfo> {
  const { apiKey, apiUrl, source } = loadApiKey();

  if (!apiKey) {
    const result: AccountInfo = {
      valid: false,
      masked_key: "",
      error: "No API key found. Run 'contextstream-mcp setup' to configure.",
    };

    if (outputJson) {
      console.log(JSON.stringify(result));
    } else {
      console.log("❌ No API key found");
      console.log("   Run 'contextstream-mcp setup' to configure your API key.");
    }
    return result;
  }

  const result = await validateApiKey(apiKey, apiUrl);

  if (outputJson) {
    console.log(JSON.stringify({ ...result, source })); // lgtm[js/clear-text-logging]
  } else {
    console.log("");
    console.log("━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━");
    console.log("   ContextStream API Key");
    console.log("━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━");
    console.log("");
    console.log(`  Key:     ${result.masked_key}`); // lgtm[js/clear-text-logging]
    console.log(`  Source:  ${source}`);

    if (result.valid) {
      console.log(`  Status:  ✓ Valid`);
      if (result.email) {
        console.log(`  Account: ${result.email}`);
      }
      if (result.name) {
        console.log(`  Name:    ${result.name}`);
      }
      if (result.plan) {
        console.log(`  Plan:    ${result.plan}`);
      }
      if (result.workspace_name) {
        console.log(`  Workspace: ${result.workspace_name}`);
      }
    } else {
      console.log(`  Status:  ✗ Invalid`);
      console.log(`  Error:   ${result.error}`);
    }
    console.log("");
  }

  return result;
}

// Export for use in install scripts
export { loadApiKey, validateApiKey, maskApiKey };
