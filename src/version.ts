import { createRequire } from "module";
import { existsSync, readFileSync, writeFileSync, mkdirSync } from "fs";
import { homedir } from "os";
import { join } from "path";

const UPGRADE_COMMAND = "npm install -g @contextstream/mcp-server@latest";
const NPM_LATEST_URL = "https://registry.npmjs.org/@contextstream/mcp-server/latest";

// This gets replaced at build time by Bun's --define flag for binary builds
declare const __CONTEXTSTREAM_VERSION__: string | undefined;

export function getVersion(): string {
  // First check if version was embedded at build time (for binary builds)
  if (typeof __CONTEXTSTREAM_VERSION__ !== "undefined" && __CONTEXTSTREAM_VERSION__) {
    return __CONTEXTSTREAM_VERSION__;
  }

  // Fallback to reading from package.json (for npm installs)
  try {
    const require = createRequire(import.meta.url);
    const pkg = require("../package.json") as { version?: string } | undefined;
    const version = pkg?.version;
    if (typeof version === "string" && version.trim()) return version.trim();
  } catch {
    // ignore
  }
  return "unknown";
}

export const VERSION = getVersion();

/**
 * Compare two semver version strings.
 * Returns: -1 if v1 < v2, 0 if v1 === v2, 1 if v1 > v2
 */
export function compareVersions(v1: string, v2: string): number {
  const parts1 = v1.split(".").map(Number);
  const parts2 = v2.split(".").map(Number);

  for (let i = 0; i < Math.max(parts1.length, parts2.length); i++) {
    const p1 = parts1[i] ?? 0;
    const p2 = parts2[i] ?? 0;
    if (p1 < p2) return -1;
    if (p1 > p2) return 1;
  }
  return 0;
}

interface VersionCache {
  latestVersion: string;
  checkedAt: number;
}

export interface VersionNotice {
  current: string;
  latest: string;
  behind: true;
  upgrade_command: string;
}

const CACHE_TTL_MS = 24 * 60 * 60 * 1000; // 24 hours
let latestVersionPromise: Promise<string | null> | null = null;

function getCacheFilePath(): string {
  return join(homedir(), ".contextstream", "version-cache.json");
}

function readCache(): VersionCache | null {
  try {
    const cacheFile = getCacheFilePath();
    if (!existsSync(cacheFile)) return null;
    const data = JSON.parse(readFileSync(cacheFile, "utf-8")) as VersionCache;
    // Check if cache is expired
    if (Date.now() - data.checkedAt > CACHE_TTL_MS) return null;
    return data;
  } catch {
    return null;
  }
}

function writeCache(latestVersion: string): void {
  try {
    const configDir = join(homedir(), ".contextstream");
    if (!existsSync(configDir)) {
      mkdirSync(configDir, { recursive: true });
    }
    const cacheFile = getCacheFilePath();
    writeFileSync(
      cacheFile,
      JSON.stringify({
        latestVersion,
        checkedAt: Date.now(),
      })
    );
  } catch {
    // ignore cache write errors
  }
}

async function fetchLatestVersion(): Promise<string | null> {
  try {
    const controller = new AbortController();
    const timeout = setTimeout(() => controller.abort(), 5000); // 5 second timeout

    const response = await fetch(NPM_LATEST_URL, {
      signal: controller.signal,
      headers: { Accept: "application/json" },
    });
    clearTimeout(timeout);

    if (!response.ok) return null;

    const data = (await response.json()) as { version?: string };
    return typeof data.version === "string" ? data.version : null;
  } catch {
    return null;
  }
}

async function resolveLatestVersion(): Promise<string | null> {
  const cached = readCache();
  if (cached) return cached.latestVersion;

  if (!latestVersionPromise) {
    latestVersionPromise = fetchLatestVersion().finally(() => {
      latestVersionPromise = null;
    });
  }

  const latestVersion = await latestVersionPromise;
  if (latestVersion) {
    writeCache(latestVersion);
  }
  return latestVersion;
}

/**
 * Check npm registry for the latest version and compare against current.
 * Shows a warning to stderr if a newer version is available.
 * Uses a 24-hour cache to avoid hitting npm on every startup.
 */
export async function checkForUpdates(): Promise<void> {
  const notice = await getUpdateNotice();
  if (notice?.behind) {
    showUpdateWarning(notice.current, notice.latest);
  }
}

function showUpdateWarning(currentVersion: string, latestVersion: string): void {
  console.error("");
  console.error("━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━");
  console.error(`⚠️  Update available: v${currentVersion} → v${latestVersion}`);
  console.error("");
  console.error(`   Run: ${UPGRADE_COMMAND}`);
  console.error("");
  console.error("   Then restart your AI tool to use the new version.");
  console.error("━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━");
  console.error("");
}

export async function getUpdateNotice(): Promise<VersionNotice | null> {
  const currentVersion = VERSION;
  if (currentVersion === "unknown") return null;

  try {
    const latestVersion = await resolveLatestVersion();
    if (!latestVersion) return null;

    if (compareVersions(currentVersion, latestVersion) < 0) {
      return {
        current: currentVersion,
        latest: latestVersion,
        behind: true,
        upgrade_command: UPGRADE_COMMAND,
      };
    }
  } catch {
    // ignore version check failures
  }

  return null;
}

/**
 * Calculate how many minor versions behind the current version is.
 * Returns 0 if current >= latest or if versions can't be parsed.
 */
export function getVersionsBehind(current: string, latest: string): number {
  try {
    const currentParts = current.split(".").map(Number);
    const latestParts = latest.split(".").map(Number);

    // If major version differs, treat as very behind (10+)
    if ((latestParts[0] ?? 0) > (currentParts[0] ?? 0)) {
      return 10 + ((latestParts[1] ?? 0) - (currentParts[1] ?? 0));
    }

    // If same major, calculate minor + patch delta
    const minorDiff = (latestParts[1] ?? 0) - (currentParts[1] ?? 0);
    const patchDiff = (latestParts[2] ?? 0) - (currentParts[2] ?? 0);

    if (minorDiff > 0) {
      return minorDiff;
    } else if (minorDiff === 0 && patchDiff > 0) {
      return 1; // At least 1 patch behind
    }

    return 0;
  } catch {
    return 0;
  }
}

/**
 * Generate a version warning message for the warnings array.
 * Returns null if not behind.
 */
export function getVersionWarning(notice: VersionNotice | null): string | null {
  if (!notice?.behind) return null;

  const versionsBehind = getVersionsBehind(notice.current, notice.latest);

  if (versionsBehind >= 3) {
    return `[VERSION_CRITICAL] MCP server is ${versionsBehind} versions behind (${notice.current} → ${notice.latest}). Update immediately: ${notice.upgrade_command}`;
  } else if (versionsBehind >= 1) {
    return `[VERSION_NOTICE] MCP server update available (${notice.current} → ${notice.latest}). Run: ${notice.upgrade_command}`;
  }

  return null;
}

/**
 * Get instructions for how often to mention version updates based on how far behind.
 * This gets prepended to context for the AI to follow.
 */
export function getVersionInstructions(notice: VersionNotice | null): string | null {
  if (!notice?.behind) return null;

  const versionsBehind = getVersionsBehind(notice.current, notice.latest);

  if (versionsBehind >= 5) {
    return `⚠️ CRITICAL: User's MCP is ${versionsBehind} versions behind. Mention update in EVERY response until updated.`;
  } else if (versionsBehind >= 3) {
    return `⚠️ User's MCP is ${versionsBehind} versions behind. Remind about update every 2-3 messages.`;
  } else if (versionsBehind >= 1) {
    return `Note: MCP update available (${notice.current}→${notice.latest}). Mention once at session start.`;
  }

  return null;
}
