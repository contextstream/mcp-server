import { z } from 'zod';
import { VERSION } from './version.js';

const DEFAULT_API_URL = 'https://api.contextstream.io';

function parseBooleanEnv(value?: string): boolean | undefined {
  if (value === undefined) return undefined;
  const normalized = value.trim().toLowerCase();
  if (['1', 'true', 'yes', 'on'].includes(normalized)) return true;
  if (['0', 'false', 'no', 'off'].includes(normalized)) return false;
  return undefined;
}

const configSchema = z.object({
  apiUrl: z.string().url().default(DEFAULT_API_URL),
  apiKey: z.string().min(1).optional(),
  jwt: z.string().min(1).optional(),
  defaultWorkspaceId: z.string().uuid().optional(),
  defaultProjectId: z.string().uuid().optional(),
  userAgent: z.string().default(`contextstream-mcp/${VERSION}`),
  allowHeaderAuth: z.boolean().optional(),
  contextPackEnabled: z.boolean().default(true),
});

export type Config = z.infer<typeof configSchema>;

const MISSING_CREDENTIALS_ERROR = 'Set CONTEXTSTREAM_API_KEY or CONTEXTSTREAM_JWT for authentication (or CONTEXTSTREAM_ALLOW_HEADER_AUTH=true for header-based auth).';

export function isMissingCredentialsError(err: unknown): boolean {
  if (err instanceof Error) {
    return err.message === MISSING_CREDENTIALS_ERROR;
  }
  return false;
}

export function loadConfig(): Config {
  const allowHeaderAuth =
    process.env.CONTEXTSTREAM_ALLOW_HEADER_AUTH === '1' ||
    process.env.CONTEXTSTREAM_ALLOW_HEADER_AUTH === 'true' ||
    process.env.CONTEXTSTREAM_ALLOW_HEADER_AUTH === 'yes';
  const contextPackEnabled = parseBooleanEnv(
    process.env.CONTEXTSTREAM_CONTEXT_PACK ?? process.env.CONTEXTSTREAM_CONTEXT_PACK_ENABLED
  );
  const parsed = configSchema.safeParse({
    apiUrl: process.env.CONTEXTSTREAM_API_URL,
    apiKey: process.env.CONTEXTSTREAM_API_KEY,
    jwt: process.env.CONTEXTSTREAM_JWT,
    defaultWorkspaceId: process.env.CONTEXTSTREAM_WORKSPACE_ID,
    defaultProjectId: process.env.CONTEXTSTREAM_PROJECT_ID,
    userAgent: process.env.CONTEXTSTREAM_USER_AGENT,
    allowHeaderAuth,
    contextPackEnabled,
  });

  if (!parsed.success) {
    const missing = parsed.error.errors.map((e) => e.path.join('.')).join(', ');
    throw new Error(
      `Invalid configuration. Set CONTEXTSTREAM_API_URL (and API key or JWT). Missing/invalid: ${missing}`
    );
  }

  if (!parsed.data.apiKey && !parsed.data.jwt && !parsed.data.allowHeaderAuth) {
    throw new Error(MISSING_CREDENTIALS_ERROR);
  }

  return parsed.data;
}
