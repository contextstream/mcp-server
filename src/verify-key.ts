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

interface McpConfig {
  mcpServers?: {
    contextstream?: {
      env?: {
        CONTEXTSTREAM_API_KEY?: string;
        CONTEXTSTREAM_API_URL?: string;
      };
    };
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

function maskApiKey(key: string): string {
  if (!key || key.length < 10) return "***";
  const prefix = key.slice(0, 6);
  const suffix = key.slice(-4);
  return `${prefix}...${suffix}`;
}

function loadApiKey(): { apiKey: string | null; apiUrl: string; source: string } {
  let apiKey: string | null = null;
  let apiUrl = "https://api.contextstream.io";
  let source = "none";

  // Priority 1: Environment variables
  if (process.env.CONTEXTSTREAM_API_KEY) {
    apiKey = process.env.CONTEXTSTREAM_API_KEY;
    source = "environment";
    if (process.env.CONTEXTSTREAM_API_URL) {
      apiUrl = process.env.CONTEXTSTREAM_API_URL;
    }
    return { apiKey, apiUrl, source };
  }

  // Priority 2: ~/.contextstream/credentials.json
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
      // Continue to next source
    }
  }

  // Priority 3: ~/.mcp.json (global)
  const globalMcpPath = path.join(homedir(), ".mcp.json");
  if (fs.existsSync(globalMcpPath)) {
    try {
      const content = fs.readFileSync(globalMcpPath, "utf-8");
      const config = JSON.parse(content) as McpConfig;
      const csEnv = config.mcpServers?.contextstream?.env;
      if (csEnv?.CONTEXTSTREAM_API_KEY) {
        apiKey = csEnv.CONTEXTSTREAM_API_KEY;
        source = "~/.mcp.json (global)";
        if (csEnv.CONTEXTSTREAM_API_URL) {
          apiUrl = csEnv.CONTEXTSTREAM_API_URL;
        }
        return { apiKey, apiUrl, source };
      }
    } catch {
      // Continue to next source
    }
  }

  // Priority 4: .mcp.json (project - check cwd and parents)
  let searchDir = process.cwd();
  for (let i = 0; i < 5; i++) {
    const projectMcpPath = path.join(searchDir, ".mcp.json");
    if (fs.existsSync(projectMcpPath)) {
      try {
        const content = fs.readFileSync(projectMcpPath, "utf-8");
        const config = JSON.parse(content) as McpConfig;
        const csEnv = config.mcpServers?.contextstream?.env;
        if (csEnv?.CONTEXTSTREAM_API_KEY) {
          apiKey = csEnv.CONTEXTSTREAM_API_KEY;
          source = `${projectMcpPath} (project)`;
          if (csEnv.CONTEXTSTREAM_API_URL) {
            apiUrl = csEnv.CONTEXTSTREAM_API_URL;
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
        plan?: string;
        workspace?: { name?: string };
      };

      return {
        valid: true,
        masked_key: maskApiKey(apiKey),
        email: data.email,
        name: data.name,
        plan: data.plan || "free",
        workspace_name: data.workspace?.name,
      };
    } else if (response.status === 401) {
      return {
        valid: false,
        masked_key: maskApiKey(apiKey),
        error: "Invalid or expired API key",
      };
    } else {
      return {
        valid: false,
        masked_key: maskApiKey(apiKey),
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
    console.log(JSON.stringify({ ...result, source }));
  } else {
    console.log("");
    console.log("━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━");
    console.log("   ContextStream API Key");
    console.log("━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━");
    console.log("");
    console.log(`  Key:     ${result.masked_key}`);
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
