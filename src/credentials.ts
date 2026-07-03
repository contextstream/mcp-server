import * as fs from "node:fs/promises";
import * as path from "node:path";
import { homedir } from "node:os";

export type SavedCredentialsV1 = {
  version: 1;
  api_url: string;
  api_key: string;
  email?: string;
  created_at: string;
  updated_at: string;
};

export function normalizeApiUrl(input: string): string {
  return (
    String(input ?? "")
      .trim()
      // Avoid accidental mismatches like https://api.example.com vs https://api.example.com/
      .replace(/\/+$/, "")
  );
}

export function credentialsFilePath(): string {
  return path.join(homedir(), ".contextstream", "credentials.json");
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

export async function readSavedCredentials(): Promise<SavedCredentialsV1 | null> {
  const filePath = credentialsFilePath();
  try {
    const raw = await fs.readFile(filePath, "utf8");
    const parsed: unknown = JSON.parse(raw);
    if (!isRecord(parsed)) return null;

    // Other ContextStream components write a legacy shape ({api_key, saved_at}
    // with no version/api_url). Accept it — a key saved without a URL belongs
    // to the default API — so a key saved by one component works in all.
    const version = parsed.version;
    if (version !== undefined && version !== 1) return null;

    const apiKey = typeof parsed.api_key === "string" ? parsed.api_key.trim() : "";
    if (!apiKey) return null;
    const apiUrl =
      typeof parsed.api_url === "string" && parsed.api_url.trim()
        ? normalizeApiUrl(parsed.api_url)
        : normalizeApiUrl(process.env.CONTEXTSTREAM_API_URL || "https://api.contextstream.io");

    const email = typeof parsed.email === "string" ? parsed.email.trim() : "";
    const createdAt = typeof parsed.created_at === "string" ? parsed.created_at : "";
    const updatedAt = typeof parsed.updated_at === "string" ? parsed.updated_at : "";
    const now = new Date().toISOString();

    return {
      version: 1,
      api_url: apiUrl,
      api_key: apiKey,
      email: email || undefined,
      created_at: createdAt || now,
      updated_at: updatedAt || now,
    };
  } catch {
    return null;
  }
}

export async function writeSavedCredentials(input: {
  apiUrl: string;
  apiKey: string;
  email?: string;
}): Promise<{ path: string; value: SavedCredentialsV1 }> {
  const filePath = credentialsFilePath();
  await fs.mkdir(path.dirname(filePath), { recursive: true });

  const now = new Date().toISOString();
  const existing = await readSavedCredentials();

  const value: SavedCredentialsV1 = {
    version: 1,
    api_url: normalizeApiUrl(input.apiUrl),
    api_key: input.apiKey.trim(),
    email: input.email?.trim() || undefined,
    created_at: existing?.created_at || now,
    updated_at: now,
  };

  const body = JSON.stringify(value, null, 2) + "\n";
  await fs.writeFile(filePath, body, { encoding: "utf8", mode: 0o600 });
  try {
    await fs.chmod(filePath, 0o600);
  } catch {
    // Best-effort only (e.g., Windows).
  }

  return { path: filePath, value };
}

export async function deleteSavedCredentials(): Promise<boolean> {
  const filePath = credentialsFilePath();
  try {
    await fs.unlink(filePath);
    return true;
  } catch {
    return false;
  }
}
