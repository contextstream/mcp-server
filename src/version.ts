import { createRequire } from "module";
import { existsSync, readFileSync, writeFileSync, mkdirSync } from "fs";
import { homedir, platform } from "os";
import { join } from "path";
import { spawn } from "child_process";

const NPM_LATEST_URL = "https://registry.npmjs.org/@contextstream/mcp-server/latest";

// Auto-update is enabled by default, can be disabled via env var
const AUTO_UPDATE_ENABLED = process.env.CONTEXTSTREAM_AUTO_UPDATE !== "false";

// Multi-platform update commands
export const UPDATE_COMMANDS = {
  npm: "npm install -g @contextstream/mcp-server@latest",
  macLinux: "curl -fsSL https://contextstream.io/scripts/setup.sh | bash",
  windows: "irm https://contextstream.io/scripts/setup.ps1 | iex",
} as const;

// Legacy single command for backwards compatibility
const UPGRADE_COMMAND = UPDATE_COMMANDS.npm;

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

const CACHE_TTL_MS = 12 * 60 * 60 * 1000; // 12 hours
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

/**
 * Invalidate the version cache if a known newer version exists.
 * Called when rules_notice indicates a newer version than the cache.
 * This ensures we don't serve stale "you're up to date" info.
 */
export function invalidateCacheIfBehind(knownNewerVersion: string): boolean {
  const cached = readCache();
  if (!cached) return false;

  // If the known version is newer than cached, invalidate
  if (compareVersions(knownNewerVersion, cached.latestVersion) > 0) {
    try {
      const cacheFile = getCacheFilePath();
      if (existsSync(cacheFile)) {
        require("fs").unlinkSync(cacheFile);
      }
      return true;
    } catch {
      return false;
    }
  }
  return false;
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

/**
 * Get full update instructions with all platform methods.
 * Used when surfacing update info to the AI for users.
 */
export function getFullUpdateInstructions(): string {
  return `**Keep Updated**

To get the latest features and fixes, update periodically:

**macOS/Linux:**
\`\`\`bash
curl -fsSL https://contextstream.io/scripts/setup.sh | bash
\`\`\`

**Windows (PowerShell):**
\`\`\`powershell
irm https://contextstream.io/scripts/setup.ps1 | iex
\`\`\`

**npm (requires Node.js 18+):**
\`\`\`bash
npm install -g @contextstream/mcp-server@latest
\`\`\`

After updating, restart your AI tool to use the new version.`;
}

/**
 * Get a concise version notice for hooks/context injection.
 * Includes update instructions without being overly verbose.
 */
export function getVersionNoticeForHook(notice: VersionNotice | null): string | null {
  if (!notice?.behind) return null;

  const versionsBehind = getVersionsBehind(notice.current, notice.latest);
  const severity = versionsBehind >= 5 ? "CRITICAL" : versionsBehind >= 3 ? "IMPORTANT" : "INFO";

  let frequency = "";
  let whatTheyAreMissing = "";

  if (versionsBehind >= 5) {
    frequency = "Mention this update in EVERY response until the user updates.";
    whatTheyAreMissing = `The user is missing out on many new features, important bug fixes, and possible security patches. Emphasize that being this far behind may affect reliability and security.`;
  } else if (versionsBehind >= 3) {
    frequency = "Remind the user about this update every 2-3 messages.";
    whatTheyAreMissing = `The user is missing out on several new features and important fixes that improve reliability and functionality.`;
  } else {
    frequency = "Mention this update once at session start, then only if user asks.";
    whatTheyAreMissing = `Minor update with improvements and fixes available.`;
  }

  return `[VERSION_${severity}] MCP server is ${versionsBehind > 0 ? versionsBehind + " version(s)" : "a version"} behind (${notice.current} → ${notice.latest}).

${whatTheyAreMissing}

${frequency}

When mentioning the update, provide these commands (user can choose their preferred method):
- macOS/Linux: curl -fsSL https://contextstream.io/scripts/setup.sh | bash
- Windows: irm https://contextstream.io/scripts/setup.ps1 | iex
- npm: npm install -g @contextstream/mcp-server@latest

Be helpful but not annoying - frame it positively as access to new capabilities rather than criticism.`;
}

/**
 * Check if auto-update is enabled (default: true)
 */
export function isAutoUpdateEnabled(): boolean {
  // Check env var first
  if (process.env.CONTEXTSTREAM_AUTO_UPDATE === "false") {
    return false;
  }

  // Check config file for user preference
  try {
    const configPath = join(homedir(), ".contextstream", "config.json");
    if (existsSync(configPath)) {
      const config = JSON.parse(readFileSync(configPath, "utf-8")) as { auto_update?: boolean };
      if (config.auto_update === false) {
        return false;
      }
    }
  } catch {
    // Ignore config read errors
  }

  return true;
}

/**
 * Set auto-update preference in config file
 */
export function setAutoUpdatePreference(enabled: boolean): void {
  try {
    const configDir = join(homedir(), ".contextstream");
    const configPath = join(configDir, "config.json");

    let config: Record<string, unknown> = {};
    if (existsSync(configPath)) {
      config = JSON.parse(readFileSync(configPath, "utf-8")) as Record<string, unknown>;
    } else {
      if (!existsSync(configDir)) {
        mkdirSync(configDir, { recursive: true });
      }
    }

    config.auto_update = enabled;
    writeFileSync(configPath, JSON.stringify(config, null, 2));
  } catch {
    // Ignore config write errors
  }
}

export interface AutoUpdateResult {
  attempted: boolean;
  success: boolean;
  previousVersion: string;
  newVersion: string | null;
  error?: string;
}

/**
 * Attempt to auto-update to the latest version.
 * Returns immediately if already up-to-date or auto-update is disabled.
 * Runs update in background and returns result.
 */
export async function attemptAutoUpdate(): Promise<AutoUpdateResult> {
  const currentVersion = VERSION;

  // Check if auto-update is enabled
  if (!isAutoUpdateEnabled()) {
    return {
      attempted: false,
      success: false,
      previousVersion: currentVersion,
      newVersion: null,
      error: "Auto-update disabled",
    };
  }

  // Check if update is needed
  const notice = await getUpdateNotice();
  if (!notice?.behind) {
    return {
      attempted: false,
      success: true,
      previousVersion: currentVersion,
      newVersion: null,
    };
  }

  // Determine best update method based on platform and how we're installed
  const updateMethod = detectUpdateMethod();

  try {
    await runUpdate(updateMethod);

    // Mark that we updated (for the restart message)
    writeUpdateMarker(currentVersion, notice.latest);

    return {
      attempted: true,
      success: true,
      previousVersion: currentVersion,
      newVersion: notice.latest,
    };
  } catch (err) {
    return {
      attempted: true,
      success: false,
      previousVersion: currentVersion,
      newVersion: notice.latest,
      error: err instanceof Error ? err.message : "Update failed",
    };
  }
}

type UpdateMethod = "npm" | "curl" | "powershell";

function detectUpdateMethod(): UpdateMethod {
  // Check if we're running from npm global install
  const execPath = process.argv[1] || "";
  if (execPath.includes("node_modules") || execPath.includes("npm")) {
    return "npm";
  }

  // Use platform-appropriate installer
  const os = platform();
  if (os === "win32") {
    return "powershell";
  }

  return "curl";
}

async function runUpdate(method: UpdateMethod): Promise<void> {
  return new Promise((resolve, reject) => {
    let command: string;
    let args: string[];
    let shell: boolean;

    switch (method) {
      case "npm":
        command = "npm";
        args = ["install", "-g", "@contextstream/mcp-server@latest"];
        shell = false;
        break;
      case "curl":
        command = "bash";
        args = ["-c", "curl -fsSL https://contextstream.io/scripts/setup.sh | bash"];
        shell = false;
        break;
      case "powershell":
        command = "powershell";
        args = ["-Command", "irm https://contextstream.io/scripts/setup.ps1 | iex"];
        shell = false;
        break;
    }

    const proc = spawn(command, args, {
      shell,
      stdio: "ignore",
      detached: true,
    });

    proc.on("error", (err) => {
      reject(err);
    });

    proc.on("close", (code) => {
      if (code === 0) {
        resolve();
      } else {
        reject(new Error(`Update process exited with code ${code}`));
      }
    });

    // Don't wait for detached process
    proc.unref();

    // Give it a moment to start, then consider it successful
    setTimeout(() => resolve(), 1000);
  });
}

function writeUpdateMarker(previousVersion: string, newVersion: string): void {
  try {
    const markerPath = join(homedir(), ".contextstream", "update-pending.json");
    const configDir = join(homedir(), ".contextstream");
    if (!existsSync(configDir)) {
      mkdirSync(configDir, { recursive: true });
    }
    writeFileSync(markerPath, JSON.stringify({
      previousVersion,
      newVersion,
      updatedAt: new Date().toISOString(),
    }));
  } catch {
    // Ignore
  }
}

/**
 * Check if an update was recently performed (for showing restart message)
 */
export function checkUpdateMarker(): { previousVersion: string; newVersion: string } | null {
  try {
    const markerPath = join(homedir(), ".contextstream", "update-pending.json");
    if (!existsSync(markerPath)) return null;

    const marker = JSON.parse(readFileSync(markerPath, "utf-8")) as {
      previousVersion: string;
      newVersion: string;
      updatedAt: string;
    };

    // Only show if updated in last hour
    const updatedAt = new Date(marker.updatedAt);
    const hourAgo = new Date(Date.now() - 60 * 60 * 1000);
    if (updatedAt < hourAgo) {
      // Clean up old marker
      try { require("fs").unlinkSync(markerPath); } catch { /* ignore */ }
      return null;
    }

    return { previousVersion: marker.previousVersion, newVersion: marker.newVersion };
  } catch {
    return null;
  }
}

/**
 * Clear the update marker after user has been notified
 */
export function clearUpdateMarker(): void {
  try {
    const markerPath = join(homedir(), ".contextstream", "update-pending.json");
    if (existsSync(markerPath)) {
      require("fs").unlinkSync(markerPath);
    }
  } catch {
    // Ignore
  }
}
