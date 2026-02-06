import * as fs from "node:fs/promises";
import * as path from "node:path";
import { homedir } from "node:os";
import { stdin, stdout } from "node:process";
import { createInterface } from "node:readline/promises";

import { ContextStreamClient } from "./client.js";
import type { Config } from "./config.js";
import { HttpError } from "./http.js";
import { generateRuleContent } from "./rules-templates.js";
import { VERSION, getUpdateNotice, setAutoUpdatePreference, isAutoUpdateEnabled } from "./version.js";
import {
  credentialsFilePath,
  normalizeApiUrl,
  readSavedCredentials,
  writeSavedCredentials,
} from "./credentials.js";
import {
  installClaudeCodeHooks,
  installEditorHooks,
  generateHooksDocumentation,
  type SupportedEditor,
} from "./hooks-config.js";
import { readAllFilesInBatches, countIndexableFiles } from "./files.js";
import { readLocalConfig, writeLocalConfig } from "./workspace-config.js";

type RuleMode = "dynamic" | "minimal" | "full" | "bootstrap";
type InstallScope = "global" | "project" | "both";
type McpScope = InstallScope | "skip";

type EditorKey = "codex" | "claude" | "cursor" | "cline" | "kilo" | "roo" | "aider" | "antigravity";

const EDITOR_LABELS: Record<EditorKey, string> = {
  codex: "Codex CLI",
  claude: "Claude Code",
  cursor: "Cursor / VS Code",
  cline: "Cline",
  kilo: "Kilo Code",
  roo: "Roo Code",
  aider: "Aider",
  antigravity: "Antigravity (Google)",
};

function supportsProjectMcpConfig(editor: EditorKey): boolean {
  return editor === "cursor" || editor === "claude" || editor === "kilo" || editor === "roo" || editor === "antigravity";
}

function normalizeInput(value: string): string {
  return value.trim();
}

function maskApiKey(apiKey: string): string {
  const trimmed = apiKey.trim();
  if (trimmed.length <= 8) return "********";
  return `${trimmed.slice(0, 4)}…${trimmed.slice(-4)}`;
}

function parseNumberList(input: string, max: number): number[] {
  const cleaned = input.trim().toLowerCase();
  if (!cleaned) return [];
  if (cleaned === "all" || cleaned === "*") {
    return Array.from({ length: max }, (_, i) => i + 1);
  }
  const parts = cleaned.split(/[, ]+/).filter(Boolean);
  const out = new Set<number>();
  for (const part of parts) {
    const n = Number.parseInt(part, 10);
    if (Number.isFinite(n) && n >= 1 && n <= max) out.add(n);
  }
  return [...out].sort((a, b) => a - b);
}

async function fileExists(filePath: string): Promise<boolean> {
  try {
    await fs.stat(filePath);
    return true;
  } catch {
    return false;
  }
}

const CONTEXTSTREAM_START_MARKER = "<!-- BEGIN ContextStream -->";
const CONTEXTSTREAM_END_MARKER = "<!-- END ContextStream -->";
const LEGACY_CONTEXTSTREAM_HINTS = [
  "contextstream integration",
  "contextstream v0.4",
  "contextstream v0.3",
  "contextstream (standard)",
  "contextstream (consolidated",
  "contextstream mcp",
  "contextstream tools",
];
const LEGACY_CONTEXTSTREAM_ALLOWED_HEADINGS = [
  "contextstream",
  "tl;dr",
  "required every message",
  "quick reference",
  "tool catalog",
  "consolidated domain tools",
  "standalone tools",
  "domain tools",
  "why context_smart",
  "recommended token budgets",
  "rules update notices",
  "preferences & lessons",
  "index & graph preflight",
  "search & code intelligence",
  "distillation",
  "when to capture",
  "behavior rules",
  "plans & tasks",
  "complete action reference",
];
const CONTEXTSTREAM_PREAMBLE_PATTERNS: RegExp[] = [
  /^#\s+workspace:/i,
  /^#\s+project:/i,
  /^#\s+workspace id:/i,
  /^#\s+codex cli instructions$/i,
  /^#\s+claude code instructions$/i,
  /^#\s+cursor rules$/i,
  /^#\s+cline rules$/i,
  /^#\s+kilo code rules$/i,
  /^#\s+roo code rules$/i,
  /^#\s+aider configuration$/i,
  /^#\s+antigravity agent rules$/i,
];

function wrapWithMarkers(content: string): string {
  return `${CONTEXTSTREAM_START_MARKER}\n${content.trim()}\n${CONTEXTSTREAM_END_MARKER}`;
}

function isLegacyContextStreamRules(content: string): boolean {
  const lower = content.toLowerCase();
  if (!lower.includes("contextstream")) return false;
  if (!LEGACY_CONTEXTSTREAM_HINTS.some((hint) => lower.includes(hint))) return false;

  const headingRegex = /^#{1,6}\s+(.+)$/gm;
  let hasHeading = false;
  let match: RegExpExecArray | null;
  while ((match = headingRegex.exec(content)) !== null) {
    hasHeading = true;
    const heading = match[1].trim().toLowerCase();
    const allowed = LEGACY_CONTEXTSTREAM_ALLOWED_HEADINGS.some((prefix) =>
      heading.startsWith(prefix)
    );
    if (!allowed) return false;
  }

  return hasHeading;
}

function isContextStreamPreamble(line: string): boolean {
  const trimmed = line.trim();
  if (!trimmed) return false;
  return CONTEXTSTREAM_PREAMBLE_PATTERNS.some((pattern) => pattern.test(trimmed));
}

function findContextStreamHeading(lines: string[]): { index: number; level: number } | null {
  const headingRegex = /^(#{1,6})\s+(.+)$/;
  for (let i = 0; i < lines.length; i += 1) {
    const match = headingRegex.exec(lines[i]);
    if (!match) continue;
    if (match[2].toLowerCase().includes("contextstream")) {
      return { index: i, level: match[1].length };
    }
  }
  return null;
}

function findSectionEnd(lines: string[], startLine: number, level: number): number {
  const headingRegex = /^(#{1,6})\s+(.+)$/;
  for (let i = startLine + 1; i < lines.length; i += 1) {
    const match = headingRegex.exec(lines[i]);
    if (!match) continue;
    if (match[1].length <= level) return i;
  }
  return lines.length;
}

function extractContextStreamBlock(content: string): string {
  const lines = content.split(/\r?\n/);
  const heading = findContextStreamHeading(lines);
  if (!heading) return content.trim();
  const endLine = findSectionEnd(lines, heading.index, heading.level);
  return lines.slice(heading.index, endLine).join("\n").trim();
}

function findLegacyContextStreamSection(
  content: string
): { startLine: number; endLine: number; contextLine: number } | null {
  const lines = content.split(/\r?\n/);
  const heading = findContextStreamHeading(lines);
  if (!heading) return null;

  let startLine = heading.index;
  for (let i = heading.index - 1; i >= 0; i -= 1) {
    const line = lines[i];
    if (!line.trim()) {
      startLine = i;
      continue;
    }
    if (isContextStreamPreamble(line)) {
      startLine = i;
      continue;
    }
    break;
  }

  const endLine = findSectionEnd(lines, heading.index, heading.level);
  return { startLine, endLine, contextLine: heading.index };
}

function blockHasPreamble(block: string): boolean {
  const lines = block.split(/\r?\n/);
  for (const line of lines) {
    const trimmed = line.trim();
    if (!trimmed) continue;
    if (isContextStreamPreamble(trimmed)) return true;
    if (/^#{1,6}\s+/.test(trimmed)) return false;
  }
  return false;
}

function replaceContextStreamBlock(
  existing: string,
  content: string
): { content: string; status: "updated" | "appended" } {
  const fullWrapped = wrapWithMarkers(content);
  const blockWrapped = wrapWithMarkers(extractContextStreamBlock(content));

  const startIdx = existing.indexOf(CONTEXTSTREAM_START_MARKER);
  const endIdx = existing.indexOf(CONTEXTSTREAM_END_MARKER);

  if (startIdx !== -1 && endIdx !== -1 && endIdx > startIdx) {
    const existingBlock = existing.slice(startIdx + CONTEXTSTREAM_START_MARKER.length, endIdx);
    const replacement = blockHasPreamble(existingBlock) ? fullWrapped : blockWrapped;
    const before = existing.substring(0, startIdx).trimEnd();
    const after = existing.substring(endIdx + CONTEXTSTREAM_END_MARKER.length).trimStart();
    const merged = [before, replacement, after].filter((part) => part.length > 0).join("\n\n");
    return { content: merged.trim() + "\n", status: "updated" };
  }

  const legacy = findLegacyContextStreamSection(existing);
  if (legacy) {
    const lines = existing.split(/\r?\n/);
    const before = lines.slice(0, legacy.startLine).join("\n").trimEnd();
    const after = lines.slice(legacy.endLine).join("\n").trimStart();
    const replacement = legacy.startLine < legacy.contextLine ? fullWrapped : blockWrapped;
    const merged = [before, replacement, after].filter((part) => part.length > 0).join("\n\n");
    return { content: merged.trim() + "\n", status: "updated" };
  }

  if (isLegacyContextStreamRules(existing)) {
    return { content: fullWrapped + "\n", status: "updated" };
  }

  const appended = existing.trimEnd() + "\n\n" + blockWrapped + "\n";
  return { content: appended, status: "appended" };
}

async function upsertTextFile(
  filePath: string,
  content: string,
  _marker: string
): Promise<"created" | "appended" | "updated"> {
  await fs.mkdir(path.dirname(filePath), { recursive: true });
  const exists = await fileExists(filePath);
  const wrappedContent = wrapWithMarkers(content);

  if (!exists) {
    await fs.writeFile(filePath, wrappedContent + "\n", "utf8");
    return "created";
  }

  const existing = await fs.readFile(filePath, "utf8").catch(() => "");
  if (!existing.trim()) {
    await fs.writeFile(filePath, wrappedContent + "\n", "utf8");
    return "updated";
  }

  const replaced = replaceContextStreamBlock(existing, content);
  await fs.writeFile(filePath, replaced.content, "utf8");
  return replaced.status;
}

function globalRulesPathForEditor(editor: EditorKey): string | null {
  const home = homedir();

  switch (editor) {
    case "codex":
      return path.join(home, ".codex", "AGENTS.md");
    case "claude":
      return path.join(home, ".claude", "CLAUDE.md");
    case "cline":
      return path.join(home, "Documents", "Cline", "Rules", "contextstream.md");
    case "kilo":
      return path.join(home, ".kilocode", "rules", "contextstream.md");
    case "roo":
      return path.join(home, ".roo", "rules", "contextstream.md");
    case "aider":
      return path.join(home, ".aider.conf.yml");
    case "antigravity":
      return path.join(home, ".gemini", "GEMINI.md");
    case "cursor":
      // Cursor global rules are configured via the app UI; project rules are supported via `.cursorrules`.
      return null;
    default:
      return null;
  }
}

async function anyPathExists(paths: string[]): Promise<boolean> {
  for (const candidate of paths) {
    if (await fileExists(candidate)) return true;
  }
  return false;
}

async function isCodexInstalled(): Promise<boolean> {
  const home = homedir();
  const envHome = process.env.CODEX_HOME;
  const candidates = [
    envHome,
    path.join(home, ".codex"),
    path.join(home, ".codex", "config.toml"),
    path.join(home, ".config", "codex"),
  ].filter((candidate): candidate is string => Boolean(candidate));
  return anyPathExists(candidates);
}

async function isClaudeInstalled(): Promise<boolean> {
  const home = homedir();
  const candidates = [path.join(home, ".claude"), path.join(home, ".config", "claude")];
  const desktopConfig = claudeDesktopConfigPath();
  if (desktopConfig) candidates.push(desktopConfig);

  if (process.platform === "darwin") {
    candidates.push(path.join(home, "Library", "Application Support", "Claude"));
  } else if (process.platform === "win32") {
    const appData = process.env.APPDATA;
    if (appData) candidates.push(path.join(appData, "Claude"));
  }

  return anyPathExists(candidates);
}

async function isClineInstalled(): Promise<boolean> {
  const home = homedir();
  const candidates = [
    path.join(home, "Documents", "Cline"),
    path.join(home, ".cline"),
    path.join(home, ".config", "cline"),
  ];
  return anyPathExists(candidates);
}

async function isKiloInstalled(): Promise<boolean> {
  const home = homedir();
  const candidates = [path.join(home, ".kilocode"), path.join(home, ".config", "kilocode")];
  return anyPathExists(candidates);
}

async function isRooInstalled(): Promise<boolean> {
  const home = homedir();
  const candidates = [path.join(home, ".roo"), path.join(home, ".config", "roo")];
  return anyPathExists(candidates);
}

async function isAiderInstalled(): Promise<boolean> {
  const home = homedir();
  const candidates = [path.join(home, ".aider.conf.yml"), path.join(home, ".config", "aider")];
  return anyPathExists(candidates);
}

// Best-effort detection to avoid creating editor configs when the editor isn't installed.
async function isCursorInstalled(): Promise<boolean> {
  const home = homedir();
  const candidates: string[] = [path.join(home, ".cursor")];

  if (process.platform === "darwin") {
    candidates.push("/Applications/Cursor.app");
    candidates.push(path.join(home, "Applications", "Cursor.app"));
    candidates.push(path.join(home, "Library", "Application Support", "Cursor"));
  } else if (process.platform === "win32") {
    const localApp = process.env.LOCALAPPDATA;
    const programFiles = process.env.ProgramFiles;
    const programFilesX86 = process.env["ProgramFiles(x86)"];
    if (localApp) candidates.push(path.join(localApp, "Programs", "Cursor", "Cursor.exe"));
    if (localApp) candidates.push(path.join(localApp, "Cursor", "Cursor.exe"));
    if (programFiles) candidates.push(path.join(programFiles, "Cursor", "Cursor.exe"));
    if (programFilesX86) candidates.push(path.join(programFilesX86, "Cursor", "Cursor.exe"));
  } else {
    candidates.push("/usr/bin/cursor");
    candidates.push("/usr/local/bin/cursor");
    candidates.push("/opt/Cursor");
    candidates.push("/opt/cursor");
  }

  return anyPathExists(candidates);
}

async function isAntigravityInstalled(): Promise<boolean> {
  const home = homedir();
  const candidates: string[] = [path.join(home, ".gemini")];

  if (process.platform === "darwin") {
    candidates.push("/Applications/Antigravity.app");
    candidates.push(path.join(home, "Applications", "Antigravity.app"));
    candidates.push(path.join(home, "Library", "Application Support", "Antigravity"));
  } else if (process.platform === "win32") {
    const localApp = process.env.LOCALAPPDATA;
    const programFiles = process.env.ProgramFiles;
    const programFilesX86 = process.env["ProgramFiles(x86)"];
    if (localApp) candidates.push(path.join(localApp, "Programs", "Antigravity", "Antigravity.exe"));
    if (localApp) candidates.push(path.join(localApp, "Antigravity", "Antigravity.exe"));
    if (programFiles) candidates.push(path.join(programFiles, "Antigravity", "Antigravity.exe"));
    if (programFilesX86) candidates.push(path.join(programFilesX86, "Antigravity", "Antigravity.exe"));
  } else {
    candidates.push("/usr/bin/antigravity");
    candidates.push("/usr/local/bin/antigravity");
    candidates.push("/opt/Antigravity");
    candidates.push("/opt/antigravity");
  }

  return anyPathExists(candidates);
}

async function isEditorInstalled(editor: EditorKey): Promise<boolean> {
  switch (editor) {
    case "codex":
      return isCodexInstalled();
    case "claude":
      return isClaudeInstalled();
    case "cursor":
      return isCursorInstalled();
    case "cline":
      return isClineInstalled();
    case "kilo":
      return isKiloInstalled();
    case "roo":
      return isRooInstalled();
    case "aider":
      return isAiderInstalled();
    case "antigravity":
      return isAntigravityInstalled();
    default:
      return false;
  }
}

type McpServerJson = {
  command: string;
  args: string[];
  env: Record<string, string>;
};

const IS_WINDOWS = process.platform === "win32";

type SetupEnvParams = {
  apiUrl: string;
  apiKey: string;
  contextPackEnabled?: boolean;
};

function escapeTomlString(value: string): string {
  return value.replace(/\\/g, "\\\\").replace(/"/g, '\\"');
}

function formatTomlEnvLines(env: Record<string, string>): string {
  return Object.entries(env)
    .map(([key, value]) => `${key} = "${escapeTomlString(value)}"`)
    .join("\n");
}

function buildSetupEnv(params: SetupEnvParams): Record<string, string> {
  const contextPack = params.contextPackEnabled === false ? "false" : "true";

  return {
    CONTEXTSTREAM_API_URL: params.apiUrl,
    CONTEXTSTREAM_API_KEY: params.apiKey,
    CONTEXTSTREAM_CONTEXT_PACK: contextPack,
  };
}

function buildContextStreamMcpServer(params: SetupEnvParams): McpServerJson {
  const env = buildSetupEnv(params);

  // Windows requires cmd /c wrapper to execute npx
  if (IS_WINDOWS) {
    return {
      command: "cmd",
      args: ["/c", "npx", "--prefer-online", "-y", "@contextstream/mcp-server@latest"],
      env,
    };
  }
  return {
    command: "npx",
    args: ["--prefer-online", "-y", "@contextstream/mcp-server@latest"],
    env,
  };
}

type VsCodeServerJson = {
  type: "stdio";
  command: string;
  args: string[];
  env: Record<string, string>;
};

function buildContextStreamVsCodeServer(params: SetupEnvParams): VsCodeServerJson {
  const env = buildSetupEnv(params);

  // Windows requires cmd /c wrapper to execute npx
  if (IS_WINDOWS) {
    return {
      type: "stdio",
      command: "cmd",
      args: ["/c", "npx", "--prefer-online", "-y", "@contextstream/mcp-server@latest"],
      env,
    };
  }
  return {
    type: "stdio",
    command: "npx",
    args: ["--prefer-online", "-y", "@contextstream/mcp-server@latest"],
    env,
  };
}

function stripJsonComments(input: string): string {
  return (
    input
      // Remove /* */ comments
      .replace(/\/\*[\s\S]*?\*\//g, "")
      // Remove // comments
      .replace(/(^|[^:])\/\/.*$/gm, "$1")
  );
}

function tryParseJsonLike(raw: string): { ok: true; value: any } | { ok: false; error: string } {
  const trimmed = raw.replace(/^\uFEFF/, "").trim();
  if (!trimmed) return { ok: true, value: {} };

  try {
    return { ok: true, value: JSON.parse(trimmed) };
  } catch {
    // Retry with basic JSONC support.
    try {
      const noComments = stripJsonComments(trimmed);
      const noTrailingCommas = noComments.replace(/,(\s*[}\]])/g, "$1");
      return { ok: true, value: JSON.parse(noTrailingCommas) };
    } catch (err: any) {
      return { ok: false, error: err?.message || "Invalid JSON" };
    }
  }
}

async function upsertJsonMcpConfig(
  filePath: string,
  server: McpServerJson
): Promise<"created" | "updated" | "skipped"> {
  await fs.mkdir(path.dirname(filePath), { recursive: true });
  const exists = await fileExists(filePath);

  let root: any = {};
  if (exists) {
    const raw = await fs.readFile(filePath, "utf8").catch(() => "");
    const parsed = tryParseJsonLike(raw);
    if (!parsed.ok) throw new Error(`Invalid JSON in ${filePath}: ${parsed.error}`);
    root = parsed.value;
  }

  if (!root || typeof root !== "object" || Array.isArray(root)) root = {};
  if (!root.mcpServers || typeof root.mcpServers !== "object" || Array.isArray(root.mcpServers))
    root.mcpServers = {};

  const before = JSON.stringify(root.mcpServers.contextstream ?? null);
  root.mcpServers.contextstream = server;
  const after = JSON.stringify(root.mcpServers.contextstream ?? null);

  await fs.writeFile(filePath, JSON.stringify(root, null, 2) + "\n", "utf8");
  if (!exists) return "created";
  return before === after ? "skipped" : "updated";
}

async function upsertJsonVsCodeMcpConfig(
  filePath: string,
  server: VsCodeServerJson
): Promise<"created" | "updated" | "skipped"> {
  await fs.mkdir(path.dirname(filePath), { recursive: true });
  const exists = await fileExists(filePath);

  let root: any = {};
  if (exists) {
    const raw = await fs.readFile(filePath, "utf8").catch(() => "");
    const parsed = tryParseJsonLike(raw);
    if (!parsed.ok) throw new Error(`Invalid JSON in ${filePath}: ${parsed.error}`);
    root = parsed.value;
  }

  if (!root || typeof root !== "object" || Array.isArray(root)) root = {};
  if (!root.servers || typeof root.servers !== "object" || Array.isArray(root.servers))
    root.servers = {};

  const before = JSON.stringify(root.servers.contextstream ?? null);
  root.servers.contextstream = server;
  const after = JSON.stringify(root.servers.contextstream ?? null);

  await fs.writeFile(filePath, JSON.stringify(root, null, 2) + "\n", "utf8");
  if (!exists) return "created";
  return before === after ? "skipped" : "updated";
}

function claudeDesktopConfigPath(): string | null {
  const home = homedir();
  if (process.platform === "darwin") {
    return path.join(
      home,
      "Library",
      "Application Support",
      "Claude",
      "claude_desktop_config.json"
    );
  }
  if (process.platform === "win32") {
    const appData = process.env.APPDATA || path.join(home, "AppData", "Roaming");
    return path.join(appData, "Claude", "claude_desktop_config.json");
  }
  if (process.platform === "linux") {
    return path.join(home, ".config", "Claude", "claude_desktop_config.json");
  }
  return null;
}

async function upsertCodexTomlConfig(
  filePath: string,
  params: SetupEnvParams
): Promise<"created" | "updated" | "skipped"> {
  await fs.mkdir(path.dirname(filePath), { recursive: true });
  const exists = await fileExists(filePath);
  const existing = exists ? await fs.readFile(filePath, "utf8").catch(() => "") : "";
  const env = buildSetupEnv(params);
  const envLines = formatTomlEnvLines(env);

  const marker = "[mcp_servers.contextstream]";
  const envMarker = "[mcp_servers.contextstream.env]";

  // Windows requires cmd /c wrapper to execute npx
  const commandLine = IS_WINDOWS
    ? `command = "cmd"\nargs = ["/c", "npx", "--prefer-online", "-y", "@contextstream/mcp-server@latest"]\n`
    : `command = "npx"\nargs = ["--prefer-online", "-y", "@contextstream/mcp-server@latest"]\n`;
  const block =
    `\n\n# ContextStream MCP server\n` +
    `[mcp_servers.contextstream]\n` +
    commandLine +
    `\n[mcp_servers.contextstream.env]\n` +
    envLines +
    "\n";

  if (!exists) {
    await fs.writeFile(filePath, block.trimStart(), "utf8");
    return "created";
  }

  if (!existing.includes(marker)) {
    await fs.writeFile(filePath, existing.trimEnd() + block, "utf8");
    return "updated";
  }

  if (!existing.includes(envMarker)) {
    await fs.writeFile(
      filePath,
      existing.trimEnd() +
      "\n\n" +
      envMarker +
      "\n" +
      envLines +
      "\n",
      "utf8"
    );
    return "updated";
  }

  const lines = existing.split(/\r?\n/);
  const out: string[] = [];
  let inEnv = false;
  const seen = new Set<string>();
  const managedKeys = new Set(Object.keys(env));

  for (const line of lines) {
    const trimmed = line.trim();
    if (trimmed.startsWith("[") && trimmed.endsWith("]")) {
      if (inEnv && trimmed !== envMarker) {
        for (const [key, value] of Object.entries(env)) {
          if (!seen.has(key)) {
            out.push(`${key} = "${escapeTomlString(value)}"`);
          }
        }
        inEnv = false;
      }
      if (trimmed === envMarker) {
        inEnv = true;
        seen.clear();
      }
      out.push(line);
      continue;
    }

    if (inEnv) {
      const match = line.match(/^\s*([A-Za-z0-9_]+)\s*=/);
      if (match && managedKeys.has(match[1])) {
        const key = match[1];
        out.push(`${key} = "${escapeTomlString(env[key])}"`);
        seen.add(key);
        continue;
      }
    }

    out.push(line);
  }

  if (inEnv) {
    for (const [key, value] of Object.entries(env)) {
      if (!seen.has(key)) {
        out.push(`${key} = "${escapeTomlString(value)}"`);
      }
    }
  }

  const updated = out.join("\n");
  if (updated === existing) return "skipped";
  await fs.writeFile(filePath, updated, "utf8");
  return "updated";
}

async function discoverProjectsUnderFolder(parentFolder: string): Promise<string[]> {
  const entries = await fs.readdir(parentFolder, { withFileTypes: true });
  const candidates = entries
    .filter((e) => e.isDirectory() && !e.name.startsWith("."))
    .map((e) => path.join(parentFolder, e.name));

  const projects: string[] = [];
  for (const dir of candidates) {
    const hasGit = await fileExists(path.join(dir, ".git"));
    const hasPkg = await fileExists(path.join(dir, "package.json"));
    const hasCargo = await fileExists(path.join(dir, "Cargo.toml"));
    const hasPyProject = await fileExists(path.join(dir, "pyproject.toml"));
    if (hasGit || hasPkg || hasCargo || hasPyProject) projects.push(dir);
  }

  return projects;
}

function buildClientConfig(params: { apiUrl: string; apiKey?: string; jwt?: string }): Config {
  return {
    apiUrl: params.apiUrl,
    apiKey: params.apiKey,
    jwt: params.jwt,
    defaultWorkspaceId: undefined,
    defaultProjectId: undefined,
    userAgent: `contextstream-mcp/setup/${VERSION}`,
    contextPackEnabled: true,
    showTiming: false,
  };
}

// ANSI color codes for progress indicator
const colors = {
  reset: "\x1b[0m",
  bright: "\x1b[1m",
  dim: "\x1b[2m",
  cyan: "\x1b[36m",
  green: "\x1b[32m",
  yellow: "\x1b[33m",
  blue: "\x1b[34m",
  magenta: "\x1b[35m",
};

// Animated spinner frames
const spinnerFrames = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

function formatBytes(bytes: number): string {
  if (bytes < 1024) return `${bytes} B`;
  if (bytes < 1024 * 1024) return `${(bytes / 1024).toFixed(1)} KB`;
  return `${(bytes / (1024 * 1024)).toFixed(1)} MB`;
}

function createProgressBar(progress: number, width: number = 30): string {
  const filled = Math.round(progress * width);
  const empty = width - filled;
  const filledBar = "█".repeat(filled);
  const emptyBar = "░".repeat(empty);
  return `${colors.cyan}${filledBar}${colors.dim}${emptyBar}${colors.reset}`;
}

async function indexProjectWithProgress(
  client: ContextStreamClient,
  projectPath: string,
  workspaceId?: string
): Promise<void> {
  const projectName = path.basename(projectPath);
  console.log(`\n${colors.bright}Indexing: ${projectName}${colors.reset}`);
  console.log(`${colors.dim}${projectPath}${colors.reset}\n`);

  // Get or create project
  let projectId: string | undefined;
  try {
    // First check local config for existing project_id
    const localConfig = readLocalConfig(projectPath);
    if (localConfig?.project_id) {
      projectId = localConfig.project_id;
    } else {
      // Find or create project by name
      const projectList = (await client.listProjects({ workspace_id: workspaceId })) as any;
      const items = projectList?.items ?? projectList?.data?.items ?? [];
      const projectNameLower = projectName.toLowerCase();

      // Look for exact name match first
      let existing = items.find((p: any) => p.name?.toLowerCase() === projectNameLower);

      // If no exact match, look for partial match
      if (!existing) {
        existing = items.find(
          (p: any) =>
            p.name?.toLowerCase().includes(projectNameLower) ||
            projectNameLower.includes(p.name?.toLowerCase() ?? "")
        );
      }

      if (existing?.id) {
        projectId = existing.id;
      } else {
        // Create new project
        const created = (await client.createProject({
          name: projectName,
          description: `Indexed from ${projectPath}`,
          workspace_id: workspaceId,
        })) as any;
        projectId = created?.id ?? created?.data?.id;
      }

      // Save project ID to local config
      if (projectId && localConfig) {
        writeLocalConfig(projectPath, {
          ...localConfig,
          project_id: projectId,
          project_name: projectName,
        });
      }
    }

    if (!projectId) {
      console.log(`${colors.yellow}! Could not resolve project ID for ${projectName}${colors.reset}`);
      return;
    }
  } catch (err: any) {
    const msg = err instanceof Error ? err.message : String(err);
    console.log(`${colors.yellow}! Failed to get/create project: ${msg}${colors.reset}`);
    return;
  }

  // Count files first (quick check with high limit)
  let totalFiles = 0;
  try {
    const countResult = await countIndexableFiles(projectPath, { maxFiles: 50000 });
    totalFiles = countResult.count;
    if (countResult.stopped) {
      console.log(`${colors.dim}Found 50,000+ indexable files${colors.reset}`);
    } else if (totalFiles === 0) {
      console.log(`${colors.dim}No indexable files found${colors.reset}`);
      return;
    } else {
      console.log(`${colors.dim}Found ${totalFiles.toLocaleString()} indexable files${colors.reset}`);
    }
  } catch {
    console.log(`${colors.dim}Scanning files...${colors.reset}`);
  }

  // Progress tracking
  let filesIndexed = 0;
  let bytesIndexed = 0;
  let batchCount = 0;
  let spinnerIdx = 0;
  const startTime = Date.now();

  // Progress update function
  const updateProgress = () => {
    const elapsed = (Date.now() - startTime) / 1000;
    const filesPerSec = elapsed > 0 ? filesIndexed / elapsed : 0;
    const progress = totalFiles > 0 ? filesIndexed / totalFiles : 0;
    const spinner = spinnerFrames[spinnerIdx % spinnerFrames.length];
    spinnerIdx++;

    const progressBar = createProgressBar(progress);
    const percentage = (progress * 100).toFixed(1);
    const speed = filesPerSec.toFixed(1);
    const size = formatBytes(bytesIndexed);

    // Clear line and write progress
    process.stdout.write(`\r${colors.cyan}${spinner}${colors.reset} ${progressBar} ${colors.bright}${percentage}%${colors.reset} | ${colors.green}${filesIndexed.toLocaleString()}${colors.reset}/${totalFiles.toLocaleString()} files | ${colors.magenta}${size}${colors.reset} | ${colors.blue}${speed} files/s${colors.reset}   `);
  };

  // Start progress animation
  const progressInterval = setInterval(updateProgress, 80);

  // Retry helper for transient failures
  const ingestWithRetry = async (batch: any[], maxRetries = 3): Promise<number> => {
    for (let attempt = 1; attempt <= maxRetries; attempt++) {
      try {
        const result = (await client.ingestFiles(projectId, batch)) as { data?: { files_indexed?: number } };
        return result.data?.files_indexed ?? batch.length;
      } catch (err: any) {
        const isTimeout = err.message?.includes("Timeout") || err.message?.includes("timeout");
        if (attempt < maxRetries && isTimeout) {
          // Wait before retry (exponential backoff)
          await new Promise((r) => setTimeout(r, 1000 * attempt));
          continue;
        }
        throw err;
      }
    }
    return batch.length;
  };

  let failedBatches = 0;

  try {
    // Index files in smaller batches (25) for better reliability
    for await (const batch of readAllFilesInBatches(projectPath, { batchSize: 25 })) {
      try {
        const indexed = await ingestWithRetry(batch);
        filesIndexed += indexed;
        bytesIndexed += batch.reduce((sum, f) => sum + f.content.length, 0);
        batchCount++;
      } catch {
        // Continue on failure - don't stop the whole process
        failedBatches++;
        filesIndexed += batch.length; // Count as processed even if failed
      }

      // Update total if we didn't get accurate count
      if (filesIndexed > totalFiles) {
        totalFiles = filesIndexed + 100; // Estimate more to come
      }
    }

    // Final progress update
    clearInterval(progressInterval);
    const elapsed = ((Date.now() - startTime) / 1000).toFixed(1);
    const finalSize = formatBytes(bytesIndexed);

    // Clear line and print completion
    process.stdout.write("\r" + " ".repeat(120) + "\r");
    if (failedBatches > 0) {
      console.log(`${colors.yellow}✓${colors.reset} Indexed ${colors.bright}${filesIndexed.toLocaleString()}${colors.reset} files (${finalSize}) in ${elapsed}s (${failedBatches} batches had errors)`);
    } else {
      console.log(`${colors.green}✓${colors.reset} Indexed ${colors.bright}${filesIndexed.toLocaleString()}${colors.reset} files (${finalSize}) in ${elapsed}s`);
    }
  } catch (err: any) {
    clearInterval(progressInterval);
    process.stdout.write("\r" + " ".repeat(120) + "\r");
    const msg = err instanceof Error ? err.message : String(err);
    console.log(`${colors.yellow}! Indexing failed: ${msg}${colors.reset}`);
  }
}

export async function runSetupWizard(args: string[]): Promise<void> {
  const dryRun = args.includes("--dry-run");
  const rl = createInterface({ input: stdin, output: stdout });

  const writeActions: Array<{
    kind: "rules" | "workspace-config" | "mcp-config" | "hooks";
    target: string;
    status: string;
  }> = [];
  let overwriteAllRules: boolean | null = null;
  let skipAllRules = false;

  const confirmOverwriteRules = async (filePath: string): Promise<boolean> => {
    if (dryRun) return true;
    if (skipAllRules) return false;
    if (overwriteAllRules) return true;
    const exists = await fileExists(filePath);
    if (!exists) return true;

    const answer = normalizeInput(
      await rl.question(
        `Rules file already exists at ${filePath}. Replace ContextStream block? [y/N/a/s]: `
      )
    ).toLowerCase();
    if (answer === "a" || answer === "all") {
      overwriteAllRules = true;
      return true;
    }
    if (answer === "s" || answer === "skip-all" || answer === "none") {
      skipAllRules = true;
      return false;
    }
    return answer === "y" || answer === "yes";
  };

  try {
    console.log(`ContextStream Setup Wizard (v${VERSION})`);
    console.log("This configures ContextStream MCP + rules for your AI editor(s).");
    if (dryRun) console.log("DRY RUN: no files will be written.\n");
    else console.log("");

    // Check for newer version and warn if running outdated cached version
    const versionNotice = await getUpdateNotice();
    if (versionNotice?.behind) {
      console.log("━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━");
      console.log(`⚠️  You're running an outdated version (v${versionNotice.current})`);
      console.log(`   Latest version is v${versionNotice.latest}`);
      console.log("");
      console.log("   To use the latest version, exit and run:");
      console.log("   npx --prefer-online -y @contextstream/mcp-server@latest setup");
      console.log("━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━");
      console.log("");
      const continueAnyway = normalizeInput(
        await rl.question("Continue with current version anyway? [y/N]: ")
      ).toLowerCase();
      if (continueAnyway !== "y" && continueAnyway !== "yes") {
        console.log("\nExiting. Run the command above to use the latest version.");
        rl.close();
        return;
      }
      console.log("");
    }

    const savedCreds = await readSavedCredentials();
    const apiUrlDefault = normalizeApiUrl(
      process.env.CONTEXTSTREAM_API_URL || savedCreds?.api_url || "https://api.contextstream.io"
    );
    const apiUrl = normalizeApiUrl(
      normalizeInput(await rl.question(`ContextStream API URL [${apiUrlDefault}]: `)) ||
      apiUrlDefault
    );

    let apiKey = normalizeInput(process.env.CONTEXTSTREAM_API_KEY || "");
    let apiKeySource: "env" | "saved" | "paste" | "browser" | "unknown" = apiKey
      ? "env"
      : "unknown";

    if (apiKey) {
      const confirm = normalizeInput(
        await rl.question(
          `Use CONTEXTSTREAM_API_KEY from environment (${maskApiKey(apiKey)})? [Y/n]: `
        )
      );
      if (confirm.toLowerCase() === "n" || confirm.toLowerCase() === "no") {
        apiKey = "";
        apiKeySource = "unknown";
      }
    }

    if (!apiKey && savedCreds?.api_key && normalizeApiUrl(savedCreds.api_url) === apiUrl) {
      const confirm = normalizeInput(
        await rl.question(
          `Use saved API key from ${credentialsFilePath()} (${maskApiKey(savedCreds.api_key)})? [Y/n]: `
        )
      );
      if (!(confirm.toLowerCase() === "n" || confirm.toLowerCase() === "no")) {
        apiKey = savedCreds.api_key;
        apiKeySource = "saved";
      }
    }

    if (!apiKey) {
      console.log("\nAuthentication:");
      console.log("  1) Browser login (recommended)");
      console.log("  2) Paste an API key");
      const authChoice = normalizeInput(await rl.question("Choose [1/2] (default 1): ")) || "1";

      if (authChoice === "2") {
        console.log("\nYou need a ContextStream API key to continue.");
        console.log(
          "Create one here (then paste it): https://app.contextstream.io/settings/api-keys\n"
        );
        apiKey = normalizeInput(await rl.question("CONTEXTSTREAM_API_KEY: "));
        apiKeySource = "paste";
      } else {
        const anonClient = new ContextStreamClient(buildClientConfig({ apiUrl }));
        let device: any;
        try {
          device = await anonClient.startDeviceLogin();
        } catch (err: any) {
          const message =
            err instanceof HttpError
              ? `${err.status} ${err.code}: ${err.message}`
              : err?.message || String(err);
          throw new Error(
            `Browser login is not available on this API. Please use an API key instead. (${message})`
          );
        }

        const verificationUrl =
          typeof device?.verification_uri_complete === "string"
            ? device.verification_uri_complete
            : typeof device?.verification_uri === "string" && typeof device?.user_code === "string"
              ? `${device.verification_uri}?user_code=${device.user_code}`
              : undefined;

        if (
          !verificationUrl ||
          typeof device?.device_code !== "string" ||
          typeof device?.expires_in !== "number"
        ) {
          throw new Error("Browser login returned an unexpected response.");
        }

        console.log("\nOpen this URL to sign in and approve the setup wizard:");
        console.log(verificationUrl);
        if (typeof device?.user_code === "string") {
          console.log(`\nCode: ${device.user_code}`);
        }
        console.log("\nWaiting for approval...");

        const startedAt = Date.now();
        const expiresMs = device.expires_in * 1000;
        const deviceCode = device.device_code as string;
        let accessToken: string | undefined;

        while (Date.now() - startedAt < expiresMs) {
          let poll: any;
          try {
            poll = await anonClient.pollDeviceLogin({ device_code: deviceCode });
          } catch (err: any) {
            const message =
              err instanceof HttpError
                ? `${err.status} ${err.code}: ${err.message}`
                : err?.message || String(err);
            throw new Error(`Browser login failed while polling. (${message})`);
          }

          if (poll && poll.status === "authorized" && typeof poll.access_token === "string") {
            accessToken = poll.access_token;
            break;
          }

          if (poll && poll.status === "pending") {
            const intervalSeconds = typeof poll.interval === "number" ? poll.interval : 5;
            const waitMs = Math.max(1, intervalSeconds) * 1000;
            await new Promise((resolve) => setTimeout(resolve, waitMs));
            continue;
          }

          // Unknown response; wait briefly and retry until expiry.
          await new Promise((resolve) => setTimeout(resolve, 1000));
        }

        if (!accessToken) {
          throw new Error(
            "Browser login expired or was not approved in time. Please run setup again."
          );
        }

        const jwtClient = new ContextStreamClient(buildClientConfig({ apiUrl, jwt: accessToken }));
        const keyName = `setup-wizard-${Date.now()}`;
        let createdKey: any;
        try {
          createdKey = await jwtClient.createApiKey({ name: keyName });
        } catch (err: any) {
          const message =
            err instanceof HttpError
              ? `${err.status} ${err.code}: ${err.message}`
              : err?.message || String(err);
          throw new Error(`Login succeeded but API key creation failed. (${message})`);
        }

        if (typeof createdKey?.secret_key !== "string" || !createdKey.secret_key.trim()) {
          throw new Error("API key creation returned an unexpected response.");
        }

        apiKey = createdKey.secret_key.trim();
        apiKeySource = "browser";
        console.log("\nCreated API key\n");
      }
    }

    const client = new ContextStreamClient(buildClientConfig({ apiUrl, apiKey }));

    // Validate auth
    let me: any;
    try {
      me = await client.me();
    } catch (err: any) {
      const message =
        err instanceof HttpError
          ? `${err.status} ${err.code}: ${err.message}`
          : err?.message || String(err);
      throw new Error(`Authentication failed. Check your API key. (${message})`);
    }

    const email =
      typeof me?.data?.email === "string"
        ? me.data.email
        : typeof me?.email === "string"
          ? me.email
          : undefined;

    console.log("Authenticated\n");

    // Persist for future setup runs so users don't have to log in again.
    // (The MCP config files we write also include the API key, but setup itself should be able to reuse it.)
    if (!dryRun && (apiKeySource === "browser" || apiKeySource === "paste")) {
      try {
        await writeSavedCredentials({ apiUrl, apiKey, email });
        console.log("Saved API key for future runs\n");
      } catch (err: any) {
        const msg = err instanceof Error ? err.message : String(err);
        console.log(
          `Warning: failed to save API key for future runs (${credentialsFilePath()}): ${msg}\n`
        );
      }
    }

    // Workspace selection
    let workspaceId: string | undefined;
    let workspaceName: string | undefined;

    console.log("Workspace setup:");
    console.log("  1) Create a new workspace");
    console.log("  2) Select an existing workspace");
    console.log("  3) Skip (rules only, no workspace mapping)");
    const wsChoice = normalizeInput(await rl.question("Choose [1/2/3] (default 2): ")) || "2";

    if (wsChoice === "1") {
      const name = normalizeInput(await rl.question("Workspace name: "));
      if (!name) throw new Error("Workspace name is required.");
      const description = normalizeInput(await rl.question("Workspace description (optional): "));
      let visibility = "private";
      while (true) {
        const raw =
          normalizeInput(await rl.question("Visibility [private/team/org] (default private): ")) ||
          "private";
        const normalized = raw.trim().toLowerCase() === "public" ? "org" : raw.trim().toLowerCase();
        if (normalized === "private" || normalized === "team" || normalized === "org") {
          visibility = normalized;
          break;
        }
        console.log("Invalid visibility. Choose: private, team, org.");
      }

      if (!dryRun) {
        const created = (await client.createWorkspace({
          name,
          description: description || undefined,
          visibility,
        })) as any;
        workspaceId = typeof created?.id === "string" ? created.id : undefined;
        workspaceName = typeof created?.name === "string" ? created.name : name;
      } else {
        workspaceId = "dry-run";
        workspaceName = name;
      }

      console.log(`Workspace: ${workspaceName}${workspaceId ? ` (${workspaceId})` : ""}\n`);
    } else if (wsChoice === "2") {
      const list = (await client.listWorkspaces({ page_size: 50 })) as any;
      const items: Array<{ id?: string; name?: string; description?: string }> = Array.isArray(
        list?.items
      )
        ? list.items
        : Array.isArray(list?.data?.items)
          ? list.data.items
          : [];

      if (items.length === 0) {
        console.log("No workspaces found. Creating a new one is recommended.\n");
      } else {
        items.slice(0, 20).forEach((w, i) => {
          console.log(`  ${i + 1}) ${w.name || "Untitled"}${w.id ? ` (${w.id})` : ""}`);
        });
        const idxRaw = normalizeInput(
          await rl.question("Select workspace number (or blank to skip): ")
        );
        if (idxRaw) {
          const idx = Number.parseInt(idxRaw, 10);
          const selected = Number.isFinite(idx) ? items[idx - 1] : undefined;
          if (selected?.id) {
            workspaceId = selected.id;
            workspaceName = selected.name;
          }
        }
      }
    }

    // Editors without hooks need full rules; others get bootstrap (hooks deliver the rest)
    const NO_HOOKS_EDITORS: EditorKey[] = ["codex", "aider", "antigravity"];
    const getModeForEditor = (editor: EditorKey): RuleMode =>
      NO_HOOKS_EDITORS.includes(editor) ? "full" : "bootstrap";

    const detectedPlanName = await client.getPlanName();
    const detectedGraphTier = await client.getGraphTier();
    const graphTierLabel =
      detectedGraphTier === "full"
        ? "full graph"
        : detectedGraphTier === "lite"
          ? "graph-lite"
          : "none";
    const planLabel = detectedPlanName ?? "unknown";
    console.log(`\nDetected plan: ${planLabel} (graph: ${graphTierLabel})`);

    // Context Pack: auto-enabled for Pro+ plans
    const contextPackEnabled = !!detectedPlanName && ["pro", "team", "enterprise"].some(p => detectedPlanName.toLowerCase().includes(p));

    // Auto-Update: enabled by default, ask user if they want to disable
    console.log("\nAuto-Update:");
    console.log("  When enabled, ContextStream will automatically update to the latest version");
    console.log("  on new sessions (checks daily). You can disable this if you prefer manual updates.");
    const currentAutoUpdate = isAutoUpdateEnabled();
    const autoUpdateChoice = normalizeInput(
      await rl.question(`Enable auto-update? [${currentAutoUpdate ? "Y/n" : "y/N"}]: `)
    ).toLowerCase();
    const autoUpdateEnabled = autoUpdateChoice === ""
      ? currentAutoUpdate  // Keep current setting if blank
      : autoUpdateChoice === "y" || autoUpdateChoice === "yes";

    // Save the preference
    setAutoUpdatePreference(autoUpdateEnabled);
    if (autoUpdateEnabled) {
      console.log("  ✓ Auto-update enabled (disable anytime with CONTEXTSTREAM_AUTO_UPDATE=false)");
    } else {
      console.log("  ✗ Auto-update disabled");
    }

    const editors: EditorKey[] = [
      "codex",
      "claude",
      "cursor",
      "cline",
      "kilo",
      "roo",
      "aider",
      "antigravity",
    ];
    console.log('\nSelect editors to configure (comma-separated numbers, or "all"):');
    editors.forEach((e, i) => console.log(`  ${i + 1}) ${EDITOR_LABELS[e]}`));
    const selectedRaw = normalizeInput(await rl.question("Editors [all]: ")) || "all";
    const selectedNums = parseNumberList(selectedRaw, editors.length);
    const selectedEditors = selectedNums.length ? selectedNums.map((n) => editors[n - 1]) : editors;

    const editorDetected = new Map<EditorKey, boolean>();
    for (const editor of selectedEditors) {
      editorDetected.set(editor, await isEditorInstalled(editor));
    }
    // If the wizard is running in Codex CLI, favor configuring Codex even if not detected.
    if (process.env.CODEX_CLI || process.env.CODEX_HOME) {
      editorDetected.set("codex", true);
    }
    const undetectedEditors = selectedEditors.filter((editor) => !editorDetected.get(editor));
    let allowUndetectedEditors = false;
    if (undetectedEditors.length) {
      console.log("\nEditors not detected on this system:");
      undetectedEditors.forEach((editor) => console.log(`- ${EDITOR_LABELS[editor]}`));
      console.log('If your editor is installed but not detected, choose "yes" to force config.');
      const confirm = normalizeInput(
        await rl.question("Configure these anyway? [y/N]: ")
      ).toLowerCase();
      allowUndetectedEditors = confirm === "y" || confirm === "yes";
    }

    const configuredEditors = allowUndetectedEditors
      ? selectedEditors
      : selectedEditors.filter((editor) => editorDetected.get(editor));
    const skippedEditors = selectedEditors.filter((editor) => !configuredEditors.includes(editor));
    if (skippedEditors.length) {
      console.log("\nSkipping editor setup:");
      skippedEditors.forEach((editor) => console.log(`- ${EDITOR_LABELS[editor]}`));
    }
    if (configuredEditors.length) {
      console.log("\nConfiguring editors:");
      configuredEditors.forEach((editor) => console.log(`- ${EDITOR_LABELS[editor]}`));
    }

    const hasCodex = configuredEditors.includes("codex");
    const hasProjectMcpEditors = configuredEditors.some((e) => supportsProjectMcpConfig(e));

    console.log("\nInstall rules as:");
    console.log("  1) Global");
    console.log("  2) Project");
    console.log("  3) Both");
    const scopeChoice = normalizeInput(await rl.question("Choose [1/2/3] (default 2): ")) || "2";
    const scope: InstallScope =
      scopeChoice === "1" ? "global" : scopeChoice === "2" ? "project" : "both";

    console.log("\nInstall MCP server config as:");
    if (hasCodex && !hasProjectMcpEditors) {
      console.log("  1) Global (Codex CLI supports global config only)");
      console.log("  2) Skip (rules only)");
    } else {
      console.log("  1) Global");
      console.log("  2) Project");
      console.log("  3) Both");
      console.log("  4) Skip (rules only)");
      if (hasCodex) {
        console.log(
          "  Note: Codex CLI does not support per-project MCP config; it will be configured globally if selected."
        );
      }
    }

    const mcpChoiceDefault = hasCodex && !hasProjectMcpEditors ? "1" : "2";
    const mcpChoice =
      normalizeInput(
        await rl.question(
          `Choose [${hasCodex && !hasProjectMcpEditors ? "1/2" : "1/2/3/4"}] (default ${mcpChoiceDefault}): `
        )
      ) || mcpChoiceDefault;
    const mcpScope: McpScope =
      mcpChoice === "2" && hasCodex && !hasProjectMcpEditors
        ? "skip"
        : mcpChoice === "4"
          ? "skip"
          : mcpChoice === "1"
            ? "global"
            : mcpChoice === "2"
              ? "project"
              : "both";

    // Build MCP server configs with selected toolset
    // v0.4.x: consolidated (~11 tools) is default, router (~2 tools) uses PROGRESSIVE_MODE
    const mcpServer = buildContextStreamMcpServer({ apiUrl, apiKey, contextPackEnabled });
    const mcpServerClaude = buildContextStreamMcpServer({ apiUrl, apiKey, contextPackEnabled });
    const vsCodeServer = buildContextStreamVsCodeServer({ apiUrl, apiKey, contextPackEnabled });

    // Global MCP config
    const needsGlobalMcpConfig =
      mcpScope === "global" || mcpScope === "both" || (mcpScope === "project" && hasCodex);
    if (needsGlobalMcpConfig) {
      console.log("\nInstalling global MCP config...");
      for (const editor of configuredEditors) {
        // If user selected Project-only, only Codex gets a global config (it has no per-project option).
        if (mcpScope === "project" && editor !== "codex") continue;
        try {
          if (editor === "codex") {
            const filePath = path.join(homedir(), ".codex", "config.toml");
            if (dryRun) {
              writeActions.push({ kind: "mcp-config", target: filePath, status: "dry-run" });
              console.log(`- ${EDITOR_LABELS[editor]}: would update ${filePath}`);
              continue;
            }
            const status = await upsertCodexTomlConfig(filePath, {
              apiUrl,
              apiKey,
              contextPackEnabled,
            });
            writeActions.push({ kind: "mcp-config", target: filePath, status });
            console.log(`- ${EDITOR_LABELS[editor]}: ${status} ${filePath}`);
            continue;
          }

          if (editor === "claude") {
            const desktopPath = claudeDesktopConfigPath();
            if (desktopPath) {
              const useDesktop =
                normalizeInput(
                  await rl.question("Also configure Claude Desktop (GUI app)? [y/N]: ")
                ).toLowerCase() === "y";
              if (useDesktop) {
                if (dryRun) {
                  writeActions.push({ kind: "mcp-config", target: desktopPath, status: "dry-run" });
                  console.log(`- Claude Desktop: would update ${desktopPath}`);
                } else {
                  const status = await upsertJsonMcpConfig(desktopPath, mcpServerClaude);
                  writeActions.push({ kind: "mcp-config", target: desktopPath, status });
                  console.log(`- Claude Desktop: ${status} ${desktopPath}`);
                }
              }
            }

            console.log(
              "- Claude Code: global MCP config is best done via `claude mcp add --transport stdio ...` (see docs)."
            );
            const packHint =
              contextPackEnabled === false
                ? " --env CONTEXTSTREAM_CONTEXT_PACK=false"
                : " --env CONTEXTSTREAM_CONTEXT_PACK=true";
            console.log(
              `  macOS/Linux: claude mcp add --transport stdio contextstream --scope user --env CONTEXTSTREAM_API_URL=... --env CONTEXTSTREAM_API_KEY=...${packHint} -- npx --prefer-online -y @contextstream/mcp-server@latest`
            );
            console.log(
              "  Windows (native): use `cmd /c npx --prefer-online -y @contextstream/mcp-server@latest` after `--` if `npx` is not found."
            );
            continue;
          }

          if (editor === "cursor") {
            const filePath = path.join(homedir(), ".cursor", "mcp.json");
            if (dryRun) {
              writeActions.push({ kind: "mcp-config", target: filePath, status: "dry-run" });
              console.log(`- ${EDITOR_LABELS[editor]}: would update ${filePath}`);
              continue;
            }
            const status = await upsertJsonMcpConfig(filePath, mcpServer);
            writeActions.push({ kind: "mcp-config", target: filePath, status });
            console.log(`- ${EDITOR_LABELS[editor]}: ${status} ${filePath}`);
            continue;
          }
          if (editor === "cline") {
            console.log(
              `- ${EDITOR_LABELS[editor]}: MCP config is managed via the extension UI (skipping global).`
            );
            continue;
          }
          if (editor === "kilo" || editor === "roo") {
            console.log(
              `- ${EDITOR_LABELS[editor]}: project MCP config supported via file; global is managed via the app UI.`
            );
            continue;
          }
          if (editor === "aider") {
            console.log(`- ${EDITOR_LABELS[editor]}: no MCP config file to write (rules only).`);
            continue;
          }
        } catch (err: any) {
          const message = err instanceof Error ? err.message : String(err);
          console.log(`- ${EDITOR_LABELS[editor]}: failed to write MCP config: ${message}`);
        }
      }
    }

    // Editor hooks (optional but highly recommended)
    // Map editors to their hook support
    const HOOKS_SUPPORTED_EDITORS: Record<EditorKey, SupportedEditor | null> = {
      claude: "claude",
      cursor: "cursor",
      cline: "cline",
      roo: "roo",
      kilo: "kilo",
      codex: null, // No hooks API
      aider: null, // No hooks API
      antigravity: null, // No hooks API
    };

    const hookEligibleEditors = configuredEditors.filter(
      (e) => HOOKS_SUPPORTED_EDITORS[e] !== null
    );

    // Install hooks by default for eligible editors
    if (hookEligibleEditors.length > 0) {
      console.log("\nInstalling editor hooks...");
      for (const editor of hookEligibleEditors) {
        const hookEditor = HOOKS_SUPPORTED_EDITORS[editor];
        if (!hookEditor) continue;

        try {
          if (dryRun) {
            console.log(`- ${EDITOR_LABELS[editor]}: would install hooks`);
            continue;
          }

          const result = await installEditorHooks({
            editor: hookEditor,
            scope: "global",
          });

          for (const script of result.installed) {
            writeActions.push({ kind: "hooks", target: script, status: "created" });
            console.log(`- ${EDITOR_LABELS[editor]}: installed ${path.basename(script)}`);
          }
        } catch (err: any) {
          const message = err instanceof Error ? err.message : String(err);
          console.log(`- ${EDITOR_LABELS[editor]}: failed to install hooks: ${message}`);
        }
      }
      console.log("  Disable hooks anytime with CONTEXTSTREAM_HOOK_ENABLED=false");
    }

    // Code Privacy message
    console.log("\nCode Privacy:");
    console.log("  ✓ Encrypted in transit (TLS 1.3) and at rest (AES-256)");
    console.log("  ✓ Isolated per workspace — no cross-tenant access");
    console.log("  ✓ Customize exclusions: .contextstream/ignore (gitignore syntax)");

    // Global rules
    if (scope === "global" || scope === "both") {
      console.log("\nInstalling global rules...");
      for (const editor of configuredEditors) {
        const filePath = globalRulesPathForEditor(editor);
        if (!filePath) {
          console.log(
            `- ${EDITOR_LABELS[editor]}: global rules need manual setup (project rules supported).`
          );
          continue;
        }

        const rule = generateRuleContent(editor, {
          workspaceName,
          workspaceId: workspaceId && workspaceId !== "dry-run" ? workspaceId : undefined,
          mode: getModeForEditor(editor),
        });
        if (!rule) continue;

        if (dryRun) {
          writeActions.push({ kind: "rules", target: filePath, status: "dry-run" });
          console.log(`- ${EDITOR_LABELS[editor]}: would write ${filePath}`);
          continue;
        }

        const allowOverwrite = await confirmOverwriteRules(filePath);
        if (!allowOverwrite) {
          writeActions.push({ kind: "rules", target: filePath, status: "skipped" });
          console.log(`- ${EDITOR_LABELS[editor]}: skipped ${filePath}`);
          continue;
        }

        const status = await upsertTextFile(filePath, rule.content, "ContextStream");
        writeActions.push({ kind: "rules", target: filePath, status });
        console.log(`- ${EDITOR_LABELS[editor]}: ${status} ${filePath}`);
      }
    }

    // Project rules + workspace mapping
    const projectPaths = new Set<string>();
    const needsProjects =
      scope === "project" ||
      scope === "both" ||
      ((mcpScope === "project" || mcpScope === "both") && hasProjectMcpEditors);

    if (needsProjects) {
      console.log("\nProject setup...");

      const addCwd = normalizeInput(
        await rl.question(`Add current folder as a project? [Y/n] (${process.cwd()}): `)
      );
      if (addCwd.toLowerCase() !== "n" && addCwd.toLowerCase() !== "no") {
        projectPaths.add(path.resolve(process.cwd()));
      }

      while (true) {
        console.log("\n  1) Add another project path");
        console.log("  2) Add all projects under a folder");
        console.log("  3) Continue");
        const choice = normalizeInput(await rl.question("Choose [1/2/3] (default 3): ")) || "3";
        if (choice === "3") break;

        if (choice === "1") {
          const p = normalizeInput(await rl.question("Project folder path: "));
          if (p) projectPaths.add(path.resolve(p));
          continue;
        }

        if (choice === "2") {
          const parent = normalizeInput(await rl.question("Parent folder path: "));
          if (!parent) continue;
          const parentAbs = path.resolve(parent);
          const projects = await discoverProjectsUnderFolder(parentAbs);
          if (projects.length === 0) {
            console.log(
              `No projects detected under ${parentAbs} (looked for .git/package.json/Cargo.toml/pyproject.toml).`
            );
            continue;
          }
          console.log(`Found ${projects.length} project(s):`);
          projects.slice(0, 25).forEach((p) => console.log(`- ${p}`));
          if (projects.length > 25) console.log(`…and ${projects.length - 25} more`);

          const confirm = normalizeInput(await rl.question("Add these projects? [Y/n]: "));
          if (confirm.toLowerCase() === "n" || confirm.toLowerCase() === "no") continue;
          projects.forEach((p) => projectPaths.add(p));
        }
      }
    }

    const projects = [...projectPaths];
    if (projects.length && needsProjects) {
      console.log(`\nApplying to ${projects.length} project(s)...`);
    }

    const createParentMapping =
      !!workspaceId &&
      workspaceId !== "dry-run" &&
      projects.length > 1 &&
      normalizeInput(
        await rl.question("Also create a parent folder mapping for auto-detection? [y/N]: ")
      ).toLowerCase() === "y";

    for (const projectPath of projects) {
      // Workspace association per project (writes .contextstream/config.json)
      if (workspaceId && workspaceId !== "dry-run" && workspaceName && !dryRun) {
        try {
          await client.associateWorkspace({
            folder_path: projectPath,
            workspace_id: workspaceId,
            workspace_name: workspaceName,
            create_parent_mapping: createParentMapping,
            // Include version and config info for desktop app compatibility
            version: VERSION,
            configured_editors: configuredEditors,
            context_pack: contextPackEnabled,
            api_url: apiUrl,
          });
          writeActions.push({
            kind: "workspace-config",
            target: path.join(projectPath, ".contextstream", "config.json"),
            status: "created",
          });
          console.log(`- Linked workspace in ${projectPath}`);
        } catch (err: any) {
          const message = err instanceof Error ? err.message : String(err);
          console.log(`- Failed to link workspace in ${projectPath}: ${message}`);
        }
      } else if (workspaceId && workspaceId !== "dry-run" && workspaceName && dryRun) {
        writeActions.push({
          kind: "workspace-config",
          target: path.join(projectPath, ".contextstream", "config.json"),
          status: "dry-run",
        });
      }

      // Project MCP configs per editor
      if (mcpScope === "project" || mcpScope === "both") {
        for (const editor of configuredEditors) {
          try {
            if (editor === "cursor") {
              const cursorPath = path.join(projectPath, ".cursor", "mcp.json");
              const vscodePath = path.join(projectPath, ".vscode", "mcp.json");
              if (dryRun) {
                writeActions.push({ kind: "mcp-config", target: cursorPath, status: "dry-run" });
                writeActions.push({ kind: "mcp-config", target: vscodePath, status: "dry-run" });
              } else {
                const status1 = await upsertJsonMcpConfig(cursorPath, mcpServer);
                const status2 = await upsertJsonVsCodeMcpConfig(vscodePath, vsCodeServer);
                writeActions.push({ kind: "mcp-config", target: cursorPath, status: status1 });
                writeActions.push({ kind: "mcp-config", target: vscodePath, status: status2 });
              }
              continue;
            }

            if (editor === "claude") {
              const mcpPath = path.join(projectPath, ".mcp.json");
              if (dryRun) {
                writeActions.push({ kind: "mcp-config", target: mcpPath, status: "dry-run" });
              } else {
                const status = await upsertJsonMcpConfig(mcpPath, mcpServerClaude);
                writeActions.push({ kind: "mcp-config", target: mcpPath, status });
              }
              continue;
            }

            if (editor === "kilo") {
              const kiloPath = path.join(projectPath, ".kilocode", "mcp.json");
              if (dryRun) {
                writeActions.push({ kind: "mcp-config", target: kiloPath, status: "dry-run" });
              } else {
                const status = await upsertJsonMcpConfig(kiloPath, mcpServer);
                writeActions.push({ kind: "mcp-config", target: kiloPath, status });
              }
              continue;
            }

            if (editor === "roo") {
              const rooPath = path.join(projectPath, ".roo", "mcp.json");
              if (dryRun) {
                writeActions.push({ kind: "mcp-config", target: rooPath, status: "dry-run" });
              } else {
                const status = await upsertJsonMcpConfig(rooPath, mcpServer);
                writeActions.push({ kind: "mcp-config", target: rooPath, status });
              }
              continue;
            }
          } catch (err: any) {
            const message = err instanceof Error ? err.message : String(err);
            console.log(
              `- Failed to write MCP config for ${EDITOR_LABELS[editor]} in ${projectPath}: ${message}`
            );
          }
        }
      }

      // Project rules per editor
      for (const editor of selectedEditors) {
        if (scope !== "project" && scope !== "both") continue;
        if (!configuredEditors.includes(editor)) continue;
        const rule = generateRuleContent(editor, {
          workspaceName,
          workspaceId: workspaceId && workspaceId !== "dry-run" ? workspaceId : undefined,
          projectName: path.basename(projectPath),
          mode: getModeForEditor(editor),
        });
        if (!rule) continue;

        const filePath = path.join(projectPath, rule.filename);
        if (dryRun) {
          writeActions.push({ kind: "rules", target: filePath, status: "dry-run" });
          continue;
        }
        try {
          const allowOverwrite = await confirmOverwriteRules(filePath);
          if (!allowOverwrite) {
            writeActions.push({ kind: "rules", target: filePath, status: "skipped" });
            continue;
          }
          const status = await upsertTextFile(filePath, rule.content, "ContextStream");
          writeActions.push({ kind: "rules", target: filePath, status });
        } catch (err: any) {
          const message = err instanceof Error ? err.message : String(err);
          writeActions.push({ kind: "rules", target: filePath, status: `error: ${message}` });
        }
      }
    }

    // Project indexing (optional but recommended for full-featured context)
    // Also check for existing projects that need indexing (update scenario)
    let projectsToIndex: string[] = [...projects];

    if (projects.length === 0 && !dryRun) {
      // Check if CWD has an existing project that needs indexing
      const cwdConfig = readLocalConfig(process.cwd());
      if (cwdConfig?.project_id) {
        try {
          const indexStatus = (await client.projectIndexStatus(cwdConfig.project_id)) as any;
          const filesIndexed = indexStatus?.data?.files_indexed ?? indexStatus?.files_indexed ?? 0;
          const isStale = indexStatus?.data?.is_stale ?? indexStatus?.is_stale ?? false;

          if (filesIndexed === 0 || isStale) {
            console.log("\n" + "─".repeat(60));
            console.log("PROJECT INDEXING");
            console.log("─".repeat(60));
            if (filesIndexed === 0) {
              console.log("Indexing enables semantic code search and AI-powered graph knowledge for rich AI context.");
            } else {
              console.log("Your project index is stale and could use a refresh.");
            }
            console.log("Powered by our blazing-fast Rust engine, indexing typically takes under a minute,");
            console.log("though larger projects may take a bit longer.\n");
            console.log("Your code is private and securely stored.\n");

            const indexChoice = normalizeInput(
              await rl.question("Perform indexing now? [Y/n]: ")
            ).toLowerCase();

            const indexingEnabled = indexChoice !== "n" && indexChoice !== "no";

            // Save indexing preference to config
            writeLocalConfig(process.cwd(), {
              ...cwdConfig,
              indexing_enabled: indexingEnabled,
              updated_at: new Date().toISOString(),
            });

            if (indexingEnabled) {
              await indexProjectWithProgress(client, process.cwd(), cwdConfig.workspace_id);
            } else {
              console.log("\nSkipping indexing. You can index later with: contextstream-mcp index <path>");
            }
          }
        } catch {
          // Ignore errors checking index status
        }
      }
    } else if (projects.length > 0 && !dryRun) {
      console.log("\n" + "─".repeat(60));
      console.log("PROJECT INDEXING");
      console.log("─".repeat(60));
      console.log("Indexing enables semantic code search and AI-powered graph knowledge for rich AI context.");
      console.log("Powered by our blazing-fast Rust engine, indexing typically takes under a minute,");
      console.log("though larger projects may take a bit longer.\n");
      console.log("Your code is private and securely stored.\n");

      const indexChoice = normalizeInput(
        await rl.question("Perform indexing for full-featured context? [Y/n]: ")
      ).toLowerCase();

      const indexingEnabled = indexChoice !== "n" && indexChoice !== "no";

      // Save indexing preference to each project config
      for (const projectPath of projects) {
        const existingConfig = readLocalConfig(projectPath);
        if (existingConfig) {
          writeLocalConfig(projectPath, {
            ...existingConfig,
            indexing_enabled: indexingEnabled,
            updated_at: new Date().toISOString(),
          });
        }
      }

      if (indexingEnabled) {
        for (const projectPath of projects) {
          await indexProjectWithProgress(client, projectPath, workspaceId);
        }
      } else {
        console.log("\nSkipping indexing. You can index later with: contextstream-mcp index <path>");
      }
    }

    console.log("\nDone.");
    if (writeActions.length) {
      const created = writeActions.filter((a) => a.status === "created").length;
      const appended = writeActions.filter((a) => a.status === "appended").length;
      const updated = writeActions.filter((a) => a.status === "updated").length;
      const skipped = writeActions.filter((a) => a.status === "skipped").length;
      const dry = writeActions.filter((a) => a.status === "dry-run").length;
      console.log(
        `Summary: ${created} created, ${updated} updated, ${appended} appended, ${skipped} skipped, ${dry} dry-run.`
      );
      console.log(`Context Pack: ${contextPackEnabled ? "enabled (Pro+)" : "disabled"}`);
    }

    console.log("\nNext steps:");
    console.log("- Restart your editor/CLI to apply changes.");
    console.log(
      "- For UI-based MCP setup (Cline/Kilo/Roo global), see https://contextstream.io/docs/mcp"
    );

    console.log("");
    console.log("You're all set! ContextStream gives your AI persistent memory, semantic code search, and cross-session context.");
    console.log("More at: https://contextstream.io/docs/mcp");
  } finally {
    rl.close();
  }
}
