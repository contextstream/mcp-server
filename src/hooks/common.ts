import * as fs from "node:fs";
import * as path from "node:path";
import { homedir } from "node:os";

export type HookApiConfig = {
  apiUrl: string;
  apiKey: string;
  jwt: string;
  workspaceId: string | null;
  projectId: string | null;
  sessionId?: string | null;
};

type McpConfig = {
  mcpServers?: {
    contextstream?: {
      env?: {
        CONTEXTSTREAM_API_KEY?: string;
        CONTEXTSTREAM_JWT?: string;
        CONTEXTSTREAM_API_URL?: string;
        CONTEXTSTREAM_WORKSPACE_ID?: string;
        CONTEXTSTREAM_PROJECT_ID?: string;
      };
    };
  };
};

type LocalConfig = {
  workspace_id?: string;
  project_id?: string;
};

const DEFAULT_API_URL = "https://api.contextstream.io";

export function readHookInput<T = Record<string, unknown>>(): T {
  try {
    return JSON.parse(fs.readFileSync(0, "utf8")) as T;
  } catch {
    return {} as T;
  }
}

export function writeHookOutput(output?: {
  additionalContext?: string;
  blocked?: boolean;
  reason?: string;
  hookEventName?: string;
}): void {
  const payload =
    output && (output.additionalContext || output.blocked || output.reason)
      ? {
          hookSpecificOutput: output.additionalContext
            ? {
                hookEventName: output.hookEventName,
                additionalContext: output.additionalContext,
              }
            : undefined,
          additionalContext: output.additionalContext,
          blocked: output.blocked,
          reason: output.reason,
        }
      : {};
  console.log(JSON.stringify(payload));
}

export function extractCwd(input: Record<string, unknown>): string {
  const cwd = typeof input.cwd === "string" && input.cwd.trim() ? input.cwd.trim() : process.cwd();
  return cwd;
}

export function loadHookConfig(cwd: string): HookApiConfig {
  let apiUrl = process.env.CONTEXTSTREAM_API_URL || DEFAULT_API_URL;
  let apiKey = process.env.CONTEXTSTREAM_API_KEY || "";
  let jwt = process.env.CONTEXTSTREAM_JWT || "";
  let workspaceId = process.env.CONTEXTSTREAM_WORKSPACE_ID || null;
  let projectId = process.env.CONTEXTSTREAM_PROJECT_ID || null;

  let searchDir = path.resolve(cwd);
  for (let i = 0; i < 6; i++) {
    if (!apiKey && !jwt) {
      const mcpPath = path.join(searchDir, ".mcp.json");
      if (fs.existsSync(mcpPath)) {
        try {
          const config = JSON.parse(fs.readFileSync(mcpPath, "utf8")) as McpConfig;
          const env = config.mcpServers?.contextstream?.env;
          if (env?.CONTEXTSTREAM_API_KEY) apiKey = env.CONTEXTSTREAM_API_KEY;
          if (env?.CONTEXTSTREAM_JWT) jwt = env.CONTEXTSTREAM_JWT;
          if (env?.CONTEXTSTREAM_API_URL) apiUrl = env.CONTEXTSTREAM_API_URL;
          if (env?.CONTEXTSTREAM_WORKSPACE_ID && !workspaceId) workspaceId = env.CONTEXTSTREAM_WORKSPACE_ID;
          if (env?.CONTEXTSTREAM_PROJECT_ID && !projectId) projectId = env.CONTEXTSTREAM_PROJECT_ID;
        } catch {
          // ignore invalid local config
        }
      }
    }

    if (!workspaceId || !projectId) {
      const localConfigPath = path.join(searchDir, ".contextstream", "config.json");
      if (fs.existsSync(localConfigPath)) {
        try {
          const localConfig = JSON.parse(fs.readFileSync(localConfigPath, "utf8")) as LocalConfig;
          if (localConfig.workspace_id && !workspaceId) workspaceId = localConfig.workspace_id;
          if (localConfig.project_id && !projectId) projectId = localConfig.project_id;
        } catch {
          // ignore invalid local config
        }
      }
    }

    const parentDir = path.dirname(searchDir);
    if (parentDir === searchDir) break;
    searchDir = parentDir;
  }

  if (!apiKey && !jwt) {
    const homeMcpPath = path.join(homedir(), ".mcp.json");
    if (fs.existsSync(homeMcpPath)) {
      try {
        const config = JSON.parse(fs.readFileSync(homeMcpPath, "utf8")) as McpConfig;
        const env = config.mcpServers?.contextstream?.env;
        if (env?.CONTEXTSTREAM_API_KEY) apiKey = env.CONTEXTSTREAM_API_KEY;
        if (env?.CONTEXTSTREAM_JWT) jwt = env.CONTEXTSTREAM_JWT;
        if (env?.CONTEXTSTREAM_API_URL) apiUrl = env.CONTEXTSTREAM_API_URL;
      } catch {
        // ignore invalid home config
      }
    }
  }

  return { apiUrl, apiKey, jwt, workspaceId, projectId };
}

export function isConfigured(config: HookApiConfig): boolean {
  return Boolean(config.apiKey || config.jwt);
}

function authHeaders(config: HookApiConfig): Record<string, string> {
  if (config.apiKey) {
    return { "X-API-Key": config.apiKey };
  }
  if (config.jwt) {
    return { Authorization: `Bearer ${config.jwt}` };
  }
  return {};
}

async function apiRequest(
  config: HookApiConfig,
  apiPath: string,
  init: { method?: string; body?: unknown } = {}
): Promise<any> {
  const response = await fetch(`${config.apiUrl}${apiPath}`, {
    method: init.method || "GET",
    headers: {
      "Content-Type": "application/json",
      ...authHeaders(config),
    },
    body: init.body !== undefined ? JSON.stringify(init.body) : undefined,
  });

  if (!response.ok) {
    throw new Error(`${response.status} ${response.statusText}`);
  }

  const json = await response.json();
  if (json && typeof json === "object" && "data" in json) {
    return (json as any).data;
  }
  return json;
}

export async function postMemoryEvent(
  config: HookApiConfig,
  title: string,
  content: unknown,
  tags: string[],
  eventType = "operation"
): Promise<void> {
  if (!isConfigured(config) || !config.workspaceId) return;

  await apiRequest(config, "/memory/events", {
    method: "POST",
    body: {
      workspace_id: config.workspaceId,
      project_id: config.projectId || undefined,
      event_type: eventType,
      title,
      content: typeof content === "string" ? content : JSON.stringify(content),
      metadata: {
        tags,
        source: "mcp_hook",
        captured_at: new Date().toISOString(),
      },
    },
  });
}

export async function createPlan(
  config: HookApiConfig,
  title: string,
  description: string
): Promise<string | null> {
  if (!isConfigured(config) || !config.workspaceId) return null;
  try {
    const result = await apiRequest(config, "/plans", {
      method: "POST",
      body: {
        workspace_id: config.workspaceId,
        project_id: config.projectId || undefined,
        title,
        description,
      },
    });
    return typeof result?.id === "string" ? result.id : null;
  } catch {
    return null;
  }
}

export async function createTask(
  config: HookApiConfig,
  params: {
    title: string;
    description?: string;
    planId?: string | null;
    status?: "pending" | "in_progress" | "completed" | "blocked" | "cancelled";
  }
): Promise<string | null> {
  if (!isConfigured(config) || !config.workspaceId) return null;
  try {
    const result = await apiRequest(config, "/tasks", {
      method: "POST",
      body: {
        workspace_id: config.workspaceId,
        project_id: config.projectId || undefined,
        title: params.title,
        description: params.description,
        plan_id: params.planId || undefined,
        status: params.status,
      },
    });
    return typeof result?.id === "string" ? result.id : null;
  } catch {
    return null;
  }
}

export async function updateTaskStatus(
  config: HookApiConfig,
  taskId: string,
  status: "pending" | "in_progress" | "completed" | "blocked" | "cancelled",
  title?: string,
  description?: string
): Promise<boolean> {
  if (!isConfigured(config) || !taskId) return false;
  try {
    await apiRequest(config, `/tasks/${taskId}`, {
      method: "PATCH",
      body: {
        status,
        ...(title ? { title } : {}),
        ...(description ? { description } : {}),
      },
    });
    return true;
  } catch {
    return false;
  }
}

export async function listPendingTasks(config: HookApiConfig, limit = 5): Promise<any[]> {
  if (!isConfigured(config) || !config.workspaceId) return [];
  try {
    const params = new URLSearchParams();
    params.set("workspace_id", config.workspaceId);
    params.set("status", "pending");
    params.set("limit", String(limit));
    if (config.projectId) params.set("project_id", config.projectId);
    const result = await apiRequest(config, `/tasks?${params.toString()}`);
    if (Array.isArray(result)) return result;
    if (Array.isArray(result?.items)) return result.items;
    if (Array.isArray(result?.tasks)) return result.tasks;
    return [];
  } catch {
    return [];
  }
}

export async function fetchFastContext(
  config: HookApiConfig,
  body: Record<string, unknown>
): Promise<string | null> {
  if (!isConfigured(config)) return null;
  try {
    const result = await apiRequest(config, "/context/hook", {
      method: "POST",
      body: {
        workspace_id: config.workspaceId || undefined,
        project_id: config.projectId || undefined,
        ...body,
      },
    });
    if (typeof result?.context === "string") return result.context;
    if (typeof result?.data?.context === "string") return result.data.context;
    return null;
  } catch {
    return null;
  }
}
