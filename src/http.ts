import type { Config } from "./config.js";
import { getAuthOverride } from "./auth-context.js";

export class HttpError extends Error {
  status: number;
  body: any;
  code: string;

  constructor(status: number, message: string, body?: any) {
    super(message);
    this.name = "HttpError";
    this.status = status;
    this.body = body;
    this.code = statusToCode(status);
  }

  toJSON() {
    return {
      error: this.message,
      status: this.status,
      code: this.code,
      details: this.body,
    };
  }
}

function statusToCode(status: number): string {
  switch (status) {
    case 0:
      return "NETWORK_ERROR";
    case 400:
      return "BAD_REQUEST";
    case 401:
      return "UNAUTHORIZED";
    case 403:
      return "FORBIDDEN";
    case 404:
      return "NOT_FOUND";
    case 409:
      return "CONFLICT";
    case 422:
      return "VALIDATION_ERROR";
    case 429:
      return "RATE_LIMITED";
    case 500:
      return "INTERNAL_ERROR";
    case 502:
      return "BAD_GATEWAY";
    case 503:
      return "SERVICE_UNAVAILABLE";
    case 504:
      return "GATEWAY_TIMEOUT";
    default:
      return "UNKNOWN_ERROR";
  }
}

export interface RequestOptions {
  method?: "GET" | "POST" | "PUT" | "DELETE" | "PATCH";
  body?: any;
  signal?: AbortSignal;
  retries?: number;
  retryDelay?: number;
  timeoutMs?: number;
  /**
   * Optional workspace ID for workspace-pooled rate limiting (Team/Enterprise plans).
   * If omitted, this is inferred from `body.workspace_id`, query `workspace_id`, or well-known URL paths,
   * and finally falls back to `config.defaultWorkspaceId` when present.
   */
  workspaceId?: string;
}

const RETRYABLE_STATUSES = new Set([408, 429, 500, 502, 503, 504]);
const MAX_RETRIES = 3;
const BASE_DELAY = 1000;

async function sleep(ms: number): Promise<void> {
  return new Promise((resolve) => setTimeout(resolve, ms));
}

export async function request<T>(
  config: Config,
  path: string,
  options: RequestOptions = {}
): Promise<T> {
  const { apiUrl, userAgent } = config;
  const authOverride = getAuthOverride();
  const apiKey = authOverride?.apiKey ?? config.apiKey;
  const jwt = authOverride?.jwt ?? config.jwt;
  // Ensure path has /api/v1 prefix
  const apiPath = path.startsWith("/api/") ? path : `/api/v1${path}`;
  const url = `${apiUrl.replace(/\/$/, "")}${apiPath}`;
  const maxRetries = options.retries ?? MAX_RETRIES;
  const baseDelay = options.retryDelay ?? BASE_DELAY;
  const timeoutMs =
    typeof options.timeoutMs === "number" && options.timeoutMs > 0 ? options.timeoutMs : 180_000;

  const headers: Record<string, string> = {
    "Content-Type": "application/json",
    "User-Agent": userAgent,
  };
  if (apiKey) headers["X-API-Key"] = apiKey;
  if (jwt) headers["Authorization"] = `Bearer ${jwt}`;
  const workspaceId =
    authOverride?.workspaceId ||
    options.workspaceId ||
    inferWorkspaceIdFromBody(options.body) ||
    inferWorkspaceIdFromPath(apiPath) ||
    config.defaultWorkspaceId;
  if (workspaceId) headers["X-Workspace-Id"] = workspaceId;

  const fetchOptions: RequestInit = {
    method: options.method || (options.body ? "POST" : "GET"),
    headers,
  };

  if (options.body !== undefined) {
    fetchOptions.body = JSON.stringify(options.body);
  }

  let lastError: HttpError | null = null;

  for (let attempt = 0; attempt <= maxRetries; attempt++) {
    const controller = new AbortController();
    const timeout = setTimeout(() => controller.abort(), timeoutMs);

    // Combine user signal with timeout
    if (options.signal) {
      options.signal.addEventListener("abort", () => controller.abort());
    }
    fetchOptions.signal = controller.signal;

    let response: Response;
    try {
      response = await fetch(url, fetchOptions);
    } catch (error: any) {
      clearTimeout(timeout);

      // Handle abort
      if (error.name === "AbortError") {
        if (options.signal?.aborted) {
          throw new HttpError(0, "Request cancelled by user");
        }
        const seconds = Math.ceil(timeoutMs / 1000);
        throw new HttpError(0, `Request timeout after ${seconds} seconds`);
      }

      lastError = new HttpError(0, error?.message || "Network error");

      // Retry on network errors
      if (attempt < maxRetries) {
        const delay = baseDelay * Math.pow(2, attempt);
        await sleep(delay);
        continue;
      }
      throw lastError;
    }

    clearTimeout(timeout);

    let payload: any = null;
    const contentType = response.headers.get("content-type") || "";
    if (contentType.includes("application/json")) {
      payload = await response.json().catch(() => null);
    } else {
      payload = await response.text().catch(() => null);
    }

    if (!response.ok) {
      const rateLimit = parseRateLimitHeaders(response.headers);
      const enrichedPayload = attachRateLimit(payload, rateLimit);

      const message = rewriteNotFoundMessage({
        status: response.status,
        path: apiPath,
        message: extractErrorMessage(enrichedPayload, response.statusText),
        payload: enrichedPayload,
      });
      lastError = new HttpError(response.status, message, enrichedPayload);

      const apiCode = extractErrorCode(enrichedPayload);
      if (apiCode) lastError.code = apiCode;

      // Retry on retryable status codes
      if (RETRYABLE_STATUSES.has(response.status) && attempt < maxRetries) {
        const retryAfter = response.headers.get("retry-after");
        const delay = retryAfter
          ? parseInt(retryAfter, 10) * 1000
          : baseDelay * Math.pow(2, attempt);
        await sleep(delay);
        continue;
      }

      throw lastError;
    }

    return payload as T;
  }

  throw lastError || new HttpError(0, "Request failed after retries");
}

const UUID_RE = /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/i;

function isUuid(value: unknown): value is string {
  return typeof value === "string" && UUID_RE.test(value);
}

function inferWorkspaceIdFromBody(body: unknown): string | undefined {
  if (!body || typeof body !== "object") return undefined;
  const maybe = (body as any).workspace_id;
  return isUuid(maybe) ? maybe : undefined;
}

function inferWorkspaceIdFromPath(apiPath: string): string | undefined {
  // Query param (e.g., /projects?workspace_id=...)
  const qIndex = apiPath.indexOf("?");
  if (qIndex >= 0) {
    try {
      const query = apiPath.slice(qIndex + 1);
      const params = new URLSearchParams(query);
      const ws = params.get("workspace_id");
      if (isUuid(ws)) return ws;
    } catch {
      // ignore
    }
  }

  // Common path patterns:
  // - /workspaces/:id
  // - /memory/events/workspace/:id
  // - /memory/nodes/workspace/:id
  const match = apiPath.match(
    /\/(?:workspaces|workspace)\/([0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12})/i
  );
  return match?.[1];
}

type RateLimitHeaders = {
  limit: number;
  remaining: number;
  reset: number;
  scope: string;
  plan: string;
  group: string;
  retryAfter?: number;
};

function parseRateLimitHeaders(headers: Headers): RateLimitHeaders | null {
  const limit = headers.get("X-RateLimit-Limit");
  if (!limit) return null;

  const retryAfter = headers.get("Retry-After");

  return {
    limit: parseInt(limit, 10),
    remaining: parseInt(headers.get("X-RateLimit-Remaining") || "0", 10),
    reset: parseInt(headers.get("X-RateLimit-Reset") || "0", 10),
    scope: headers.get("X-RateLimit-Scope") || "unknown",
    plan: headers.get("X-RateLimit-Plan") || "unknown",
    group: headers.get("X-RateLimit-Group") || "default",
    retryAfter: retryAfter ? parseInt(retryAfter, 10) : undefined,
  };
}

function attachRateLimit(payload: any, rateLimit: RateLimitHeaders | null): any {
  if (!rateLimit) return payload;

  if (payload && typeof payload === "object") {
    return { ...payload, rate_limit: rateLimit };
  }

  return { error: payload, rate_limit: rateLimit };
}

function extractErrorMessage(payload: any, fallback: string): string {
  if (!payload) return fallback;

  // ContextStream API error format: { error: { code, message, details }, ... }
  const nested = payload?.error;
  if (nested && typeof nested === "object" && typeof nested.message === "string") {
    return nested.message;
  }

  if (typeof payload.message === "string") return payload.message;
  if (typeof payload.error === "string") return payload.error;
  if (typeof payload.detail === "string") return payload.detail;

  return fallback;
}

function extractErrorCode(payload: any): string | null {
  if (!payload) return null;
  const nested = payload?.error;
  if (
    nested &&
    typeof nested === "object" &&
    typeof nested.code === "string" &&
    nested.code.trim()
  ) {
    return nested.code.trim();
  }
  if (typeof payload.code === "string" && payload.code.trim()) return payload.code.trim();
  return null;
}

function detectIntegrationProvider(path: string): "github" | "slack" | null {
  if (/\/github(\/|$)/i.test(path)) return "github";
  if (/\/slack(\/|$)/i.test(path)) return "slack";
  return null;
}

function rewriteNotFoundMessage(input: {
  status: number;
  path: string;
  message: string;
  payload: any;
}): string {
  if (input.status !== 404) return input.message;

  const provider = detectIntegrationProvider(input.path);
  if (!provider) return input.message;
  if (!/\/workspaces\//i.test(input.path)) return input.message;

  const label = provider === "github" ? "GitHub" : "Slack";
  return `${label} integration is not connected for this workspace. Connect ${label} in workspace integrations and retry. If you intended a different workspace, pass workspace_id.`;
}
