import { z, type ZodRawShape } from "zod";
import * as fs from "node:fs";
import * as path from "node:path";
import { homedir } from "node:os";
import type { McpServer } from "@modelcontextprotocol/sdk/server/mcp.js";
import type { MessageExtraInfo, ToolAnnotations } from "@modelcontextprotocol/sdk/types.js";
import type { ContextStreamClient } from "./client.js";
import { readFilesFromDirectory, readAllFilesInBatches, countIndexableFiles } from "./files.js";
import { SessionManager } from "./session-manager.js";
import {
  getAvailableEditors,
  generateRuleContent,
  generateAllRuleFiles,
  RULES_VERSION,
} from "./rules-templates.js";
import { VERSION, getUpdateNotice } from "./version.js";
import { generateToolCatalog, getCoreToolsHint, type CatalogFormat } from "./tool-catalog.js";
import { getAuthOverride, runWithAuthOverride, type AuthOverride } from "./auth-context.js";
import { installClaudeCodeHooks, markProjectIndexed } from "./hooks-config.js";
import { trackToolTokenSavings, type TokenSavingsToolType } from "./token-savings.js";

type StructuredContent = { [x: string]: unknown } | undefined;
type ToolTextResult = {
  content: Array<{ type: "text"; text: string }>;
  structuredContent?: StructuredContent;
  isError?: boolean;
};

const LESSON_DEDUP_WINDOW_MS = 2 * 60 * 1000;
const recentLessonCaptures = new Map<string, number>();

/**
 * Search rules reminder injected into session_init and context_smart responses.
 * This keeps the critical instruction in recent context to combat instruction decay.
 * Can be disabled via CONTEXTSTREAM_SEARCH_REMINDER=false environment variable.
 */
const SEARCH_RULES_REMINDER_ENABLED =
  process.env.CONTEXTSTREAM_SEARCH_REMINDER?.toLowerCase() !== "false";

const SEARCH_RULES_REMINDER = `
⚠️ [SEARCH RULES - READ EVERY TIME]
BEFORE using Glob/Grep/Read/Search/Explore → call mcp__contextstream__search(mode="hybrid", query="...") FIRST
BEFORE using EnterPlanMode/Task(Plan) → call mcp__contextstream__session(action="capture_plan", ...) instead
Local tools ONLY if ContextStream returns 0 results after retry.
`.trim();

/**
 * Lessons reminder injected when there are active lessons.
 * This ensures AI doesn't repeat past mistakes.
 */
const LESSONS_REMINDER_PREFIX = `
⚠️ [LESSONS - REVIEW BEFORE CHANGES]
Past mistakes found that may be relevant. STOP and review before proceeding:
`.trim();

/**
 * Generate a lessons reminder block if lessons are present in the result.
 */
function generateLessonsReminder(result: Record<string, unknown>): string {
  const lessons = result.lessons as Array<{
    title?: string;
    trigger?: string;
    prevention?: string;
    severity?: string;
  }> | undefined;

  if (!lessons || lessons.length === 0) {
    return "";
  }

  const lessonLines = lessons.slice(0, 5).map((l, i) => {
    const severity = l.severity === "critical" ? "🚨" : l.severity === "high" ? "⚠️" : "📝";
    const title = l.title || "Untitled lesson";
    const prevention = l.prevention || l.trigger || "";
    return `${i + 1}. ${severity} ${title}${prevention ? `: ${prevention.slice(0, 100)}` : ""}`;
  });

  return `\n\n${LESSONS_REMINDER_PREFIX}\n${lessonLines.join("\n")}`;
}

/**
 * Generate aggressive rules update warning when rules are outdated.
 * This nudges AI to update rules while preserving custom content.
 */
function generateRulesUpdateWarning(rulesNotice: RulesNotice | null): string {
  if (!rulesNotice || (rulesNotice.status !== "behind" && rulesNotice.status !== "missing")) {
    return "";
  }

  const isMissing = rulesNotice.status === "missing";
  const current = rulesNotice.current ?? "none";
  const latest = rulesNotice.latest;

  return `
🚨 [RULES ${isMissing ? "MISSING" : "OUTDATED"} - ACTION REQUIRED]
${isMissing ? "ContextStream rules are NOT installed." : `Rules version ${current} → ${latest} available.`}
${isMissing ? "AI behavior may be suboptimal without proper rules." : "New rules include important improvements for better AI behavior."}

**UPDATE NOW:** Run \`mcp__contextstream__generate_rules(overwrite_existing=true)\`
This is SAFE - only the ContextStream block is updated, your custom rules are preserved.
`.trim();
}

/**
 * Generate aggressive MCP version update warning.
 */
function generateVersionUpdateWarning(versionNotice: { behind?: boolean; current?: string; latest?: string; upgrade_command?: string } | null): string {
  if (!versionNotice?.behind) {
    return "";
  }

  return `
🚨 [MCP SERVER OUTDATED - UPDATE RECOMMENDED]
Current: ${versionNotice.current} → Latest: ${versionNotice.latest}
New version may include critical bug fixes, performance improvements, and new features.

**UPDATE NOW:** Run \`${versionNotice.upgrade_command || "npm update @contextstream/mcp-server"}\`
Then restart Claude Code to use the new version.
`.trim();
}

const DEFAULT_PARAM_DESCRIPTIONS: Record<string, string> = {
  api_key: "ContextStream API key.",
  apiKey: "ContextStream API key.",
  jwt: "ContextStream JWT for authentication.",
  workspace_id: "Workspace ID (UUID).",
  workspaceId: "Workspace ID (UUID).",
  project_id: "Project ID (UUID).",
  projectId: "Project ID (UUID).",
  node_id: "Node ID (UUID).",
  event_id: "Event ID (UUID).",
  reminder_id: "Reminder ID (UUID).",
  folder_path: "Absolute path to the local folder.",
  file_path: "Filesystem path to the file.",
  path: "Filesystem path.",
  name: "Name for the resource.",
  title: "Short descriptive title.",
  description: "Short description.",
  content: "Full content/body.",
  query: "Search query string.",
  limit: "Maximum number of results to return.",
  page: "Page number for pagination.",
  page_size: "Results per page.",
  include_decisions: "Include related decisions.",
  include_related: "Include related context.",
  include_transitive: "Include transitive dependencies.",
  max_depth: "Maximum traversal depth.",
  since: "ISO 8601 timestamp to query changes since.",
  remind_at: "ISO 8601 datetime for the reminder.",
  priority: "Priority level.",
  recurrence: "Recurrence pattern (daily, weekly, monthly).",
  keywords: "Keywords for matching.",
  overwrite: "Allow overwriting existing files on disk.",
  overwrite_existing: "Allow overwriting existing rule files (ContextStream block only).",
  write_to_disk: "Write ingested files to disk before indexing.",
  await_indexing: "Wait for indexing to finish before returning.",
  auto_index: "Automatically index on creation.",
  session_id: "Session identifier.",
  context_hint: "User message used to fetch relevant context.",
  context: "Context to match relevant reminders.",
};

const uuidSchema = z.string().uuid();

function normalizeUuid(value?: string): string | undefined {
  if (!value) return undefined;
  return uuidSchema.safeParse(value).success ? value : undefined;
}

type RulesNotice = {
  status: "behind" | "missing" | "unknown";
  current?: string;
  latest: string;
  files_checked: string[];
  files_outdated?: string[];
  files_missing_version?: string[];
  update_tool: "generate_rules";
  update_args: {
    folder_path?: string;
    editors?: string[];
    mode?: "minimal" | "full";
  };
  update_command: string;
};

const RULES_NOTICE_CACHE_TTL_MS = 10 * 60 * 1000;
const RULES_VERSION_REGEX = /Rules Version:\s*([0-9][0-9A-Za-z.\-]*)/i;
const CONTEXTSTREAM_START_MARKER = "<!-- BEGIN ContextStream -->";
const CONTEXTSTREAM_END_MARKER = "<!-- END ContextStream -->";

const RULES_PROJECT_FILES: Record<string, string> = {
  codex: "AGENTS.md",
  claude: "CLAUDE.md",
  cursor: ".cursorrules",
  windsurf: ".windsurfrules",
  cline: ".clinerules",
  kilo: path.join(".kilocode", "rules", "contextstream.md"),
  roo: path.join(".roo", "rules", "contextstream.md"),
  aider: ".aider.conf.yml",
};

const RULES_GLOBAL_FILES: Partial<Record<string, string[]>> = {
  codex: [path.join(homedir(), ".codex", "AGENTS.md")],
  windsurf: [path.join(homedir(), ".codeium", "windsurf", "memories", "global_rules.md")],
  kilo: [path.join(homedir(), ".kilocode", "rules", "contextstream.md")],
  roo: [path.join(homedir(), ".roo", "rules", "contextstream.md")],
};

const rulesNoticeCache = new Map<string, { checkedAt: number; notice: RulesNotice | null }>();

function compareVersions(v1: string, v2: string): number {
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

function extractRulesVersions(content: string): string[] {
  const regex = new RegExp(RULES_VERSION_REGEX.source, "gi");
  return Array.from(content.matchAll(regex))
    .map((match) => match[1]?.trim())
    .filter((version): version is string => Boolean(version));
}

function extractContextStreamMarkerBlock(content: string): string | null {
  const startIdx = content.indexOf(CONTEXTSTREAM_START_MARKER);
  const endIdx = content.indexOf(CONTEXTSTREAM_END_MARKER);
  if (startIdx === -1 || endIdx === -1 || endIdx <= startIdx) {
    return null;
  }
  return content.slice(startIdx + CONTEXTSTREAM_START_MARKER.length, endIdx).trim();
}

function extractRulesVersion(content: string): string | null {
  const markerBlock = extractContextStreamMarkerBlock(content);
  const candidates = markerBlock
    ? extractRulesVersions(markerBlock)
    : extractRulesVersions(content);
  if (candidates.length === 0) {
    return null;
  }
  return candidates.sort(compareVersions).at(-1) ?? null;
}

function detectEditorFromClientName(clientName?: string): string | null {
  if (!clientName) return null;
  const normalized = clientName.toLowerCase().trim();
  if (normalized.includes("cursor")) return "cursor";
  if (normalized.includes("windsurf") || normalized.includes("codeium")) return "windsurf";
  if (normalized.includes("claude")) return "claude";
  if (normalized.includes("cline")) return "cline";
  if (normalized.includes("kilo")) return "kilo";
  if (normalized.includes("roo")) return "roo";
  if (normalized.includes("codex")) return "codex";
  if (normalized.includes("aider")) return "aider";
  return null;
}

function resolveRulesCandidatePaths(folderPath: string | null, editorKey: string | null): string[] {
  const candidates = new Set<string>();

  const addProject = (key: string) => {
    if (!folderPath) return;
    const rel = RULES_PROJECT_FILES[key];
    if (rel) {
      candidates.add(path.join(folderPath, rel));
    }
  };

  const addGlobal = (key: string) => {
    const paths = RULES_GLOBAL_FILES[key];
    if (!paths) return;
    for (const p of paths) {
      candidates.add(p);
    }
  };

  if (editorKey) {
    addProject(editorKey);
    addGlobal(editorKey);
  } else {
    for (const key of Object.keys(RULES_PROJECT_FILES)) {
      addProject(key);
      addGlobal(key);
    }
  }

  return Array.from(candidates);
}

function resolveFolderPath(inputPath?: string, sessionManager?: SessionManager): string | null {
  if (inputPath) return inputPath;
  const fromSession = sessionManager?.getFolderPath();
  if (fromSession) return fromSession;
  const ctxPath = sessionManager?.getContext();
  const contextFolder =
    ctxPath && typeof ctxPath.folder_path === "string" ? (ctxPath.folder_path as string) : null;
  if (contextFolder) return contextFolder;

  const cwd = process.cwd();
  const indicators = [".git", "package.json", "Cargo.toml", "pyproject.toml", ".contextstream"];
  const hasIndicator = indicators.some((entry) => {
    try {
      return fs.existsSync(path.join(cwd, entry));
    } catch {
      return false;
    }
  });
  return hasIndicator ? cwd : null;
}

function getRulesNotice(folderPath: string | null, clientName?: string): RulesNotice | null {
  if (!RULES_VERSION || RULES_VERSION === "0.0.0") return null;

  const editorKey = detectEditorFromClientName(clientName);
  if (!folderPath && !editorKey) {
    return null;
  }
  const cacheKey = `${folderPath ?? "none"}|${editorKey ?? "all"}`;
  const cached = rulesNoticeCache.get(cacheKey);
  if (cached && Date.now() - cached.checkedAt < RULES_NOTICE_CACHE_TTL_MS) {
    return cached.notice;
  }

  const candidates = resolveRulesCandidatePaths(folderPath, editorKey);
  const existing = candidates.filter((filePath) => fs.existsSync(filePath));
  if (existing.length === 0) {
    const updateCommand = "generate_rules()";
    const notice: RulesNotice = {
      status: "missing",
      latest: RULES_VERSION,
      files_checked: candidates,
      update_tool: "generate_rules",
      update_args: {
        ...(folderPath ? { folder_path: folderPath } : {}),
        editors: editorKey ? [editorKey] : ["all"],
      },
      update_command: updateCommand,
    };
    rulesNoticeCache.set(cacheKey, { checkedAt: Date.now(), notice });
    return notice;
  }

  const filesMissingVersion: string[] = [];
  const filesOutdated: string[] = [];
  const versions: string[] = [];

  for (const filePath of existing) {
    try {
      const content = fs.readFileSync(filePath, "utf-8");
      const version = extractRulesVersion(content);
      if (!version) {
        filesMissingVersion.push(filePath);
        continue;
      }
      versions.push(version);
      if (compareVersions(version, RULES_VERSION) < 0) {
        filesOutdated.push(filePath);
      }
    } catch {
      filesMissingVersion.push(filePath);
    }
  }

  if (filesOutdated.length === 0 && filesMissingVersion.length === 0) {
    rulesNoticeCache.set(cacheKey, { checkedAt: Date.now(), notice: null });
    return null;
  }

  const current = versions.sort(compareVersions).at(-1);
  const updateCommand = "generate_rules()";

  const notice: RulesNotice = {
    status: filesOutdated.length > 0 ? "behind" : "unknown",
    current,
    latest: RULES_VERSION,
    files_checked: existing,
    ...(filesOutdated.length > 0 ? { files_outdated: filesOutdated } : {}),
    ...(filesMissingVersion.length > 0 ? { files_missing_version: filesMissingVersion } : {}),
    update_tool: "generate_rules",
    update_args: {
      ...(folderPath ? { folder_path: folderPath } : {}),
      editors: editorKey ? [editorKey] : ["all"],
    },
    update_command: updateCommand,
  };

  rulesNoticeCache.set(cacheKey, { checkedAt: Date.now(), notice });
  return notice;
}

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
  /^#\s+windsurf rules$/i,
  /^#\s+cline rules$/i,
  /^#\s+kilo code rules$/i,
  /^#\s+roo code rules$/i,
  /^#\s+aider configuration$/i,
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

async function upsertRuleFile(
  filePath: string,
  content: string
): Promise<"created" | "updated" | "appended"> {
  await fs.promises.mkdir(path.dirname(filePath), { recursive: true });
  const wrappedContent = wrapWithMarkers(content);

  let existing = "";
  try {
    existing = await fs.promises.readFile(filePath, "utf8");
  } catch {
    // file does not exist yet
  }

  if (!existing) {
    await fs.promises.writeFile(filePath, wrappedContent + "\n", "utf8");
    return "created";
  }

  if (!existing.trim()) {
    await fs.promises.writeFile(filePath, wrappedContent + "\n", "utf8");
    return "updated";
  }

  const replaced = replaceContextStreamBlock(existing, content);
  await fs.promises.writeFile(filePath, replaced.content, "utf8");
  return replaced.status;
}

async function writeEditorRules(options: {
  folderPath: string;
  editors?: string[];
  workspaceName?: string;
  workspaceId?: string;
  projectName?: string;
  additionalRules?: string;
  mode?: "minimal" | "full";
  overwriteExisting?: boolean;
}): Promise<Array<{ editor: string; filename: string; status: string }>> {
  const editors =
    options.editors && options.editors.length > 0 ? options.editors : getAvailableEditors();

  const results: Array<{ editor: string; filename: string; status: string }> = [];

  for (const editor of editors) {
    const rule = generateRuleContent(editor, {
      workspaceName: options.workspaceName,
      workspaceId: options.workspaceId,
      projectName: options.projectName,
      additionalRules: options.additionalRules,
      mode: options.mode,
    });

    if (!rule) {
      results.push({ editor, filename: "", status: "unknown editor" });
      continue;
    }

    const filePath = path.join(options.folderPath, rule.filename);
    if (fs.existsSync(filePath) && !options.overwriteExisting) {
      results.push({ editor, filename: rule.filename, status: "skipped (exists)" });
      continue;
    }
    try {
      const status = await upsertRuleFile(filePath, rule.content);
      results.push({ editor, filename: rule.filename, status });
    } catch (err) {
      results.push({
        editor,
        filename: rule.filename,
        status: `error: ${(err as Error).message}`,
      });
    }
  }

  for (const key of rulesNoticeCache.keys()) {
    if (key.startsWith(`${options.folderPath}|`)) {
      rulesNoticeCache.delete(key);
    }
  }

  return results;
}

type RuleWriteResult = { editor: string; filename: string; status: string };

function listGlobalRuleTargets(editors: string[]): Array<{ editor: string; filePath: string }> {
  const targets: Array<{ editor: string; filePath: string }> = [];
  for (const editor of editors) {
    const globalPaths = RULES_GLOBAL_FILES[editor];
    if (!globalPaths || globalPaths.length === 0) {
      continue;
    }
    for (const filePath of globalPaths) {
      targets.push({ editor, filePath });
    }
  }
  return targets;
}

async function writeGlobalRules(options: {
  editors: string[];
  mode?: "minimal" | "full";
  overwriteExisting?: boolean;
}): Promise<Array<RuleWriteResult & { scope: "global" }>> {
  const results: Array<RuleWriteResult & { scope: "global" }> = [];

  for (const editor of options.editors) {
    const rule = generateRuleContent(editor, {
      mode: options.mode,
    });

    if (!rule) {
      results.push({ editor, filename: "", status: "unknown editor", scope: "global" });
      continue;
    }

    const globalPaths = RULES_GLOBAL_FILES[editor] ?? [];
    if (globalPaths.length === 0) {
      results.push({
        editor,
        filename: rule.filename,
        status: "skipped (no global path)",
        scope: "global",
      });
      continue;
    }

    for (const filePath of globalPaths) {
      if (fs.existsSync(filePath) && !options.overwriteExisting) {
        results.push({ editor, filename: filePath, status: "skipped (exists)", scope: "global" });
        continue;
      }
      try {
        const status = await upsertRuleFile(filePath, rule.content);
        results.push({ editor, filename: filePath, status, scope: "global" });
      } catch (err) {
        results.push({
          editor,
          filename: filePath,
          status: `error: ${(err as Error).message}`,
          scope: "global",
        });
      }
    }
  }

  rulesNoticeCache.clear();
  return results;
}

const WRITE_VERBS = new Set([
  "create",
  "update",
  "delete",
  "ingest",
  "index",
  "capture",
  "remember",
  "associate",
  "bootstrap",
  "snooze",
  "complete",
  "dismiss",
  "generate",
  "sync",
  "publish",
  "set",
  "add",
  "remove",
  "revoke",
  "feedback",
  "upload",
  "compress",
  "init",
]);

const READ_ONLY_OVERRIDES = new Set([
  "session_tools",
  "context_smart",
  "session_summary",
  "session_recall",
  "session_get_user_context",
  "session_get_lessons",
  "session_smart_search",
  "session_delta",
  "projects_list",
  "projects_get",
  "projects_overview",
  "projects_statistics",
  "projects_files",
  "projects_index_status",
  "workspaces_list",
  "workspaces_get",
  "memory_search",
  "memory_decisions",
  "decision_trace",
  "memory_get_event",
  "memory_list_events",
  "memory_list_nodes",
  "memory_summary",
  "memory_timeline",
  "graph_related",
  "graph_decisions",
  "graph_path",
  "graph_dependencies",
  "graph_call_path",
  "graph_impact",
  "graph_circular_dependencies",
  "graph_unused_code",
  "search_semantic",
  "search_hybrid",
  "search_keyword",
  "search_pattern",
  "reminders_list",
  "reminders_active",
  "auth_me",
  "mcp_server_version",
]);

const DESTRUCTIVE_VERBS = new Set(["delete", "dismiss", "remove", "revoke", "supersede"]);

const OPEN_WORLD_PREFIXES = new Set(["github", "slack", "integrations"]);

function humanizeKey(raw: string): string {
  const withSpaces = raw.replace(/([a-z])([A-Z])/g, "$1 $2").replace(/_/g, " ");
  return withSpaces.toLowerCase();
}

function buildParamDescription(key: string, path: string[]): string {
  const normalized = key in DEFAULT_PARAM_DESCRIPTIONS ? key : key.toLowerCase();
  const parent = path[path.length - 1];

  if (parent === "target") {
    if (key === "id") return "Target identifier (module path, function id, etc.).";
    if (key === "type") return "Target type (module, file, function, type, variable).";
  }

  if (parent === "source") {
    if (key === "id") return "Source identifier (module path, function id, etc.).";
    if (key === "type") return "Source type (module, file, function, type, variable).";
  }

  if (DEFAULT_PARAM_DESCRIPTIONS[normalized]) {
    return DEFAULT_PARAM_DESCRIPTIONS[normalized];
  }

  if (normalized.endsWith("_id")) {
    return `ID for the ${humanizeKey(normalized.replace(/_id$/, ""))}.`;
  }

  if (normalized.startsWith("include_")) {
    return `Whether to include ${humanizeKey(normalized.replace("include_", ""))}.`;
  }

  if (normalized.startsWith("max_")) {
    return `Maximum ${humanizeKey(normalized.replace("max_", ""))}.`;
  }

  if (normalized.startsWith("min_")) {
    return `Minimum ${humanizeKey(normalized.replace("min_", ""))}.`;
  }

  return `Input parameter: ${humanizeKey(normalized)}.`;
}

function getDescription(schema: z.ZodTypeAny): string | undefined {
  const def = (schema as { _def?: { description?: string } })._def;
  if (def?.description && def.description.trim()) return def.description;
  return undefined;
}

function applyParamDescriptions(schema: z.ZodTypeAny, path: string[] = []): z.ZodTypeAny {
  if (!(schema instanceof z.ZodObject)) {
    return schema;
  }

  const shape = schema.shape;
  let changed = false;
  const nextShape: ZodRawShape = {};

  for (const [key, field] of Object.entries(shape) as Array<[string, z.ZodTypeAny]>) {
    let nextField: z.ZodTypeAny = field;
    const existingDescription = getDescription(field);

    if (field instanceof z.ZodObject) {
      const nested = applyParamDescriptions(field, [...path, key]);
      if (nested !== field) {
        nextField = nested;
        changed = true;
      }
    }

    if (existingDescription) {
      if (!getDescription(nextField)) {
        nextField = nextField.describe(existingDescription);
        changed = true;
      }
    } else {
      nextField = nextField.describe(buildParamDescription(key, path));
      changed = true;
    }

    nextShape[key] = nextField;
  }

  if (!changed) return schema;

  let nextSchema: z.ZodTypeAny = z.object(nextShape);
  const def = (schema as { _def?: { catchall?: z.ZodTypeAny; unknownKeys?: string } })._def;
  if (def?.catchall) nextSchema = (nextSchema as z.ZodObject<any>).catchall(def.catchall);
  if (def?.unknownKeys === "passthrough")
    nextSchema = (nextSchema as z.ZodObject<any>).passthrough();
  if (def?.unknownKeys === "strict") nextSchema = (nextSchema as z.ZodObject<any>).strict();

  return nextSchema;
}

function inferToolAnnotations(toolName: string): ToolAnnotations {
  const parts = toolName.split("_");
  const prefix = parts[0] || toolName;
  const readOnly =
    READ_ONLY_OVERRIDES.has(toolName) || !parts.some((part) => WRITE_VERBS.has(part));
  const destructive = readOnly ? false : parts.some((part) => DESTRUCTIVE_VERBS.has(part));
  const openWorld = OPEN_WORLD_PREFIXES.has(prefix);

  return {
    readOnlyHint: readOnly,
    destructiveHint: readOnly ? false : destructive,
    idempotentHint: readOnly,
    openWorldHint: openWorld,
  };
}

function normalizeHeaderValue(value: string | string[] | undefined): string | undefined {
  if (!value) return undefined;
  return Array.isArray(value) ? value[0] : value;
}

function resolveAuthOverride(extra?: MessageExtraInfo): AuthOverride | null {
  const token = extra?.authInfo?.token;
  const tokenType = extra?.authInfo?.extra?.tokenType;
  const headers = extra?.requestInfo?.headers;
  const existing = getAuthOverride();

  const workspaceId =
    normalizeHeaderValue(headers?.["x-contextstream-workspace-id"]) ||
    normalizeHeaderValue(headers?.["x-workspace-id"]);
  const projectId =
    normalizeHeaderValue(headers?.["x-contextstream-project-id"]) ||
    normalizeHeaderValue(headers?.["x-project-id"]);

  if (token) {
    if (tokenType === "jwt") {
      return { jwt: token, workspaceId, projectId };
    }
    return { apiKey: token, workspaceId, projectId };
  }

  if (existing?.apiKey || existing?.jwt) {
    return {
      apiKey: existing.apiKey,
      jwt: existing.jwt,
      workspaceId: workspaceId ?? existing.workspaceId,
      projectId: projectId ?? existing.projectId,
    };
  }

  if (!workspaceId && !projectId) return null;

  return { workspaceId, projectId };
}

// Light toolset: Core session, project, and basic memory tools (~31 tools)
const LIGHT_TOOLSET = new Set<string>([
  // Core session tools (13)
  "session_init",
  "session_tools",
  "context_smart",
  "context_feedback",
  "session_summary",
  "session_capture",
  "session_capture_lesson",
  "session_get_lessons",
  "session_recall",
  "session_remember",
  "session_get_user_context",
  "session_smart_search",
  "session_compress",
  "session_delta",
  // Setup and configuration (3)
  "generate_editor_rules",
  "generate_rules",
  "workspace_associate",
  "workspace_bootstrap",
  // Project management (5)
  "projects_create",
  "projects_list",
  "projects_get",
  "projects_overview",
  "projects_statistics",
  // Project indexing (4)
  "projects_ingest_local",
  "projects_index",
  "projects_index_status",
  "projects_files",
  // Memory basics (3)
  "memory_search",
  "memory_decisions",
  "memory_get_event",
  // Graph basics (2)
  "graph_related",
  "graph_decisions",
  // Reminders (2)
  "reminders_list",
  "reminders_active",
  // Utility (2)
  "auth_me",
  "mcp_server_version",
]);

// Standard toolset: Balanced set for most users (default) - ~58 tools
const STANDARD_TOOLSET = new Set<string>([
  // Core session tools (14)
  "session_init",
  "session_tools",
  "context_smart",
  "context_feedback",
  "session_summary",
  "session_capture",
  "session_capture_lesson",
  "session_get_lessons",
  "session_recall",
  "session_remember",
  "session_get_user_context",
  "session_smart_search",
  "session_compress",
  "session_delta",
  // Setup and configuration (3)
  "generate_editor_rules",
  "generate_rules",
  "workspace_associate",
  "workspace_bootstrap",
  // Workspace management (2)
  "workspaces_list",
  "workspaces_get",
  // Project management (6)
  "projects_create",
  "projects_list",
  "projects_get",
  "projects_overview",
  "projects_statistics",
  "projects_update",
  // Project indexing (4)
  "projects_ingest_local",
  "projects_index",
  "projects_index_status",
  "projects_files",
  // Memory events (9)
  "memory_search",
  "memory_decisions",
  "decision_trace",
  "memory_create_event",
  "memory_list_events",
  "memory_get_event",
  "memory_update_event",
  "memory_delete_event",
  "memory_timeline",
  "memory_summary",
  // Memory nodes (6) - full CRUD for memory hygiene
  "memory_create_node",
  "memory_list_nodes",
  "memory_get_node",
  "memory_update_node",
  "memory_delete_node",
  "memory_supersede_node",
  // Memory distillation (1)
  "memory_distill_event",
  // Knowledge graph analysis (8)
  "graph_related",
  "graph_decisions",
  "graph_path",
  "graph_dependencies",
  "graph_call_path",
  "graph_impact",
  "graph_circular_dependencies",
  "graph_unused_code",
  "graph_ingest",
  // Search (3)
  "search_semantic",
  "search_hybrid",
  "search_keyword",
  // Reminders (6)
  "reminders_list",
  "reminders_active",
  "reminders_create",
  "reminders_snooze",
  "reminders_complete",
  "reminders_dismiss",
  // Utility (2)
  "auth_me",
  "mcp_server_version",
]);

// Complete toolset: All tools (resolved as null allowlist)
// Includes: workspaces, projects, memory, knowledge graph, AI, integrations

// Integration tools - only exposed when integrations are connected
// This set is used for:
// 1. Auto-hiding when integrations are not connected (Option B - dynamic)
// 2. Lazy evaluation fallback with helpful error messages (Option A)
const SLACK_TOOLS = new Set<string>([
  "slack_stats",
  "slack_channels",
  "slack_search",
  "slack_discussions",
  "slack_activity",
  "slack_contributors",
  "slack_knowledge",
  "slack_summary",
  "slack_sync_users",
]);

const GITHUB_TOOLS = new Set<string>([
  "github_stats",
  "github_repos",
  "github_search",
  "github_issues",
  "github_activity",
  "github_contributors",
  "github_knowledge",
  "github_summary",
]);

const NOTION_TOOLS = new Set<string>([
  "notion_create_page",
  "notion_list_databases",
  "notion_search_pages",
  "notion_get_page",
  "notion_query_database",
  "notion_update_page",
  "notion_stats",
  "notion_activity",
  "notion_knowledge",
  "notion_summary",
]);

const CROSS_INTEGRATION_TOOLS = new Set<string>([
  "integrations_status",
  "integrations_search",
  "integrations_summary",
  "integrations_knowledge",
]);

// All integration tools combined
const ALL_INTEGRATION_TOOLS = new Set<string>([
  ...SLACK_TOOLS,
  ...GITHUB_TOOLS,
  ...NOTION_TOOLS,
  ...CROSS_INTEGRATION_TOOLS,
]);

// Environment variable to control integration tool auto-hiding
// CONTEXTSTREAM_AUTO_HIDE_INTEGRATIONS=true (default) | false
const AUTO_HIDE_INTEGRATIONS = process.env.CONTEXTSTREAM_AUTO_HIDE_INTEGRATIONS !== "false";

// ============================================
// CLIENT DETECTION (Strategy 3)
// ============================================

// Token-sensitive clients that benefit from smaller tool registries
const TOKEN_SENSITIVE_CLIENTS = new Set([
  "claude",
  "claude-code",
  "claude code",
  "claude desktop",
  "anthropic",
]);

// Environment variable to control auto-toolset behavior
// CONTEXTSTREAM_AUTO_TOOLSET=true | false (default: false - disabled until strategies 4-7 implemented)
const AUTO_TOOLSET_ENABLED = process.env.CONTEXTSTREAM_AUTO_TOOLSET === "true";

// =============================================================================
// Strategy 4: Schema Minimization Mode
// =============================================================================
// Environment variable to control schema verbosity
// CONTEXTSTREAM_SCHEMA_MODE=compact | full (default: full)
// Compact mode reduces tool descriptions and parameter descriptions to minimize token overhead
const SCHEMA_MODE = process.env.CONTEXTSTREAM_SCHEMA_MODE || "full";
const COMPACT_SCHEMA_ENABLED = SCHEMA_MODE === "compact";

/**
 * Compactify a tool description for token savings.
 * - Keeps only the first sentence or line
 * - Removes examples, code blocks, and verbose explanations
 * - Max ~100 characters
 */
function compactifyDescription(description: string): string {
  if (!description) return "";

  // Remove markdown code blocks
  let compact = description.replace(/```[\s\S]*?```/g, "");

  // Remove inline code examples after "Example:"
  compact = compact.replace(/\n*Example:[\s\S]*$/i, "");

  // Remove "Access: ..." lines (these are added separately anyway in non-compact mode)
  compact = compact.replace(/\n*Access:.*$/gm, "");

  // Take first line or first sentence
  const firstLine = compact.split("\n")[0].trim();
  const firstSentence = firstLine.split(/\.(?:\s|$)/)[0];

  // Prefer first sentence if it's not too short, otherwise use first line
  let result = firstSentence.length >= 20 ? firstSentence : firstLine;

  // Truncate if still too long
  if (result.length > 120) {
    result = result.substring(0, 117) + "...";
  }

  return result;
}

/**
 * Compactify parameter descriptions in a Zod schema.
 * In compact mode, we skip auto-generated descriptions entirely and only keep
 * explicitly provided descriptions, shortened to their essence.
 */
function compactifyParamDescription(description: string | undefined): string | undefined {
  if (!description) return undefined;

  // Keep only first clause, max 40 chars
  const firstClause = description.split(/[.,;]/)[0].trim();
  if (firstClause.length > 40) {
    return firstClause.substring(0, 37) + "...";
  }
  return firstClause;
}

/**
 * Apply compact parameter descriptions to a schema.
 * Unlike applyParamDescriptions, this SKIPS auto-generating descriptions
 * and only keeps/shortens explicitly provided ones.
 */
function applyCompactParamDescriptions(schema: z.ZodTypeAny): z.ZodTypeAny {
  if (!(schema instanceof z.ZodObject)) {
    return schema;
  }

  const shape = schema.shape;
  let changed = false;
  const nextShape: ZodRawShape = {};

  for (const [key, field] of Object.entries(shape) as Array<[string, z.ZodTypeAny]>) {
    let nextField: z.ZodTypeAny = field;
    const existingDescription = getDescription(field);

    if (field instanceof z.ZodObject) {
      const nested = applyCompactParamDescriptions(field);
      if (nested !== field) {
        nextField = nested;
        changed = true;
      }
    }

    // In compact mode, only keep explicitly provided descriptions (shortened)
    // Don't auto-generate descriptions for params without them
    if (existingDescription) {
      const compact = compactifyParamDescription(existingDescription);
      if (compact && compact !== existingDescription) {
        nextField = nextField.describe(compact);
        changed = true;
      }
    }
    // Note: We intentionally DON'T add descriptions for params without them

    nextShape[key] = nextField;
  }

  if (!changed) return schema;

  let nextSchema: z.ZodTypeAny = z.object(nextShape);
  const def = (schema as { _def?: { catchall?: z.ZodTypeAny; unknownKeys?: string } })._def;
  if (def?.catchall) nextSchema = (nextSchema as z.ZodObject<any>).catchall(def.catchall);
  if (def?.unknownKeys === "passthrough")
    nextSchema = (nextSchema as z.ZodObject<any>).passthrough();
  if (def?.unknownKeys === "strict") nextSchema = (nextSchema as z.ZodObject<any>).strict();

  return nextSchema;
}

/**
 * Detect if we're running inside Claude Code using environment variables (Option A - Fallback).
 * Claude Code sets CLAUDECODE=1 and CLAUDE_CODE_ENTRYPOINT when spawning MCP servers.
 */
function detectClaudeCodeFromEnv(): boolean {
  return (
    process.env.CLAUDECODE === "1" ||
    process.env.CLAUDE_CODE_ENTRYPOINT !== undefined ||
    process.env.CLAUDE_CODE === "1"
  );
}

/**
 * Check if a client name indicates a token-sensitive client.
 */
function isTokenSensitiveClient(clientName: string | undefined): boolean {
  if (!clientName) return false;
  const normalized = clientName.toLowerCase().trim();
  for (const pattern of TOKEN_SENSITIVE_CLIENTS) {
    if (normalized.includes(pattern)) {
      return true;
    }
  }
  return false;
}

/**
 * Get the recommended toolset for a detected client.
 * Returns null if no recommendation (use user's explicit setting or default).
 */
function getRecommendedToolset(
  clientName: string | undefined,
  fromEnv: boolean
): Set<string> | null {
  // If auto-toolset is disabled, don't make recommendations
  if (!AUTO_TOOLSET_ENABLED) return null;

  // Check if it's a token-sensitive client
  if (isTokenSensitiveClient(clientName) || fromEnv) {
    // Recommend light toolset for Claude Code / Claude Desktop
    return LIGHT_TOOLSET;
  }

  return null;
}

// Track detected client info (updated when MCP initialize is received)
let detectedClientInfo: { name?: string; version?: string } | null = null;
let clientDetectedFromEnv = false;

// ============================================
// END CLIENT DETECTION
// ============================================

// =============================================================================
// Strategy 5: Progressive Disclosure with Tool Bundles
// =============================================================================
// Environment variable to control progressive disclosure mode
// CONTEXTSTREAM_PROGRESSIVE_MODE=true | false (default: false)
// When enabled, only core tools are registered initially. AI can call
// tools_enable_bundle to unlock additional functionality dynamically.
const PROGRESSIVE_MODE = process.env.CONTEXTSTREAM_PROGRESSIVE_MODE === "true";

// Bundle definitions - group related tools together
// The 'core' bundle is always enabled and contains essential tools
const TOOL_BUNDLES: Record<string, Set<string>> = {
  // Core bundle (~12 tools) - always enabled, essential for any session
  core: new Set([
    "session_init",
    "session_tools",
    "context_smart",
    "context_feedback",
    "session_capture",
    "session_capture_lesson",
    "session_get_lessons",
    "session_recall",
    "session_remember",
    "session_get_user_context",
    "tools_enable_bundle", // Meta-tool to enable other bundles
    "auth_me",
    "mcp_server_version",
  ]),

  // Session bundle (~6 tools) - extended session management
  session: new Set([
    "session_summary",
    "session_smart_search",
    "session_compress",
    "session_delta",
    "decision_trace",
    "generate_editor_rules",
    "generate_rules",
  ]),

  // Memory bundle (~12 tools) - full memory CRUD operations
  memory: new Set([
    "memory_create_event",
    "memory_update_event",
    "memory_delete_event",
    "memory_list_events",
    "memory_get_event",
    "memory_search",
    "memory_decisions",
    "memory_timeline",
    "memory_summary",
    "memory_create_node",
    "memory_update_node",
    "memory_delete_node",
    "memory_list_nodes",
    "memory_get_node",
    "memory_supersede_node",
    "memory_distill_event",
  ]),

  // Search bundle (~4 tools) - search capabilities
  search: new Set(["search_semantic", "search_hybrid", "search_keyword"]),

  // Graph bundle (~9 tools) - code graph analysis
  graph: new Set([
    "graph_related",
    "graph_decisions",
    "graph_path",
    "graph_dependencies",
    "graph_call_path",
    "graph_impact",
    "graph_circular_dependencies",
    "graph_unused_code",
    "graph_ingest",
  ]),

  // Workspace bundle (~4 tools) - workspace management
  workspace: new Set([
    "workspaces_list",
    "workspaces_get",
    "workspace_associate",
    "workspace_bootstrap",
  ]),

  // Project bundle (~10 tools) - project management and indexing
  project: new Set([
    "projects_create",
    "projects_update",
    "projects_list",
    "projects_get",
    "projects_overview",
    "projects_statistics",
    "projects_index",
    "projects_index_status",
    "projects_files",
    "projects_ingest_local",
  ]),

  // Reminders bundle (~6 tools) - reminder management
  reminders: new Set([
    "reminders_list",
    "reminders_active",
    "reminders_create",
    "reminders_snooze",
    "reminders_complete",
    "reminders_dismiss",
  ]),

  // Integrations bundle - Slack/GitHub/Notion tools (auto-hidden when not connected)
  integrations: new Set([
    "slack_stats",
    "slack_channels",
    "slack_search",
    "slack_discussions",
    "slack_activity",
    "slack_contributors",
    "slack_knowledge",
    "slack_summary",
    "slack_sync_users",
    "github_stats",
    "github_repos",
    "github_search",
    "github_issues",
    "github_activity",
    "github_contributors",
    "github_knowledge",
    "github_summary",
    "notion_create_page",
    "notion_list_databases",
    "notion_search_pages",
    "notion_get_page",
    "notion_query_database",
    "notion_update_page",
    "notion_stats",
    "notion_activity",
    "notion_knowledge",
    "notion_summary",
    "integrations_status",
    "integrations_search",
    "integrations_summary",
    "integrations_knowledge",
  ]),
};

// Track which bundles are currently enabled (runtime state)
const enabledBundles = new Set<string>(["core"]);

// Check if a tool belongs to any enabled bundle
function isToolInEnabledBundles(toolName: string): boolean {
  for (const bundleName of enabledBundles) {
    const bundle = TOOL_BUNDLES[bundleName];
    if (bundle?.has(toolName)) {
      return true;
    }
  }
  return false;
}

// Find which bundle a tool belongs to
function findToolBundle(toolName: string): string | null {
  for (const [bundleName, tools] of Object.entries(TOOL_BUNDLES)) {
    if (tools.has(toolName)) {
      return bundleName;
    }
  }
  return null;
}

// Get bundle info for display
function getBundleInfo(): Array<{
  name: string;
  size: number;
  enabled: boolean;
  description: string;
}> {
  const descriptions: Record<string, string> = {
    core: "Essential session tools (always enabled)",
    session: "Extended session management and utilities",
    memory: "Full memory CRUD operations",
    search: "Semantic, hybrid, and keyword search",
    graph: "Code graph analysis and dependencies",
    workspace: "Workspace management",
    project: "Project management and indexing",
    reminders: "Reminder management",
    integrations: "Slack and GitHub integrations",
  };

  return Object.entries(TOOL_BUNDLES).map(([name, tools]) => ({
    name,
    size: tools.size,
    enabled: enabledBundles.has(name),
    description: descriptions[name] || `${name} tools`,
  }));
}

// Storage for deferred tool registrations (only used in progressive mode)
type DeferredToolConfig = {
  name: string;
  config: { title: string; description: string; inputSchema: z.ZodType };
  handler: (input: any, extra?: MessageExtraInfo) => Promise<ToolTextResult>;
};
const deferredTools = new Map<string, DeferredToolConfig>();

// =============================================================================
// END Strategy 5
// =============================================================================

// =============================================================================
// Strategy 6: Router Tool Pattern
// =============================================================================
// Environment variable to control router mode
// CONTEXTSTREAM_ROUTER_MODE=true | false (default: false)
// When enabled, instead of registering 50+ individual tools, we register only
// 2 meta-tools: `contextstream` (dispatcher) and `contextstream_help` (schema lookup).
// This reduces the tool registry from ~50k+ tokens to ~2-3k tokens.
const ROUTER_MODE = process.env.CONTEXTSTREAM_ROUTER_MODE === "true";

// Operations registry - stores all tool configs when router mode is enabled
type OperationConfig = {
  name: string;
  title: string;
  description: string;
  inputSchema: z.ZodType;
  handler: (input: any, extra?: MessageExtraInfo) => Promise<ToolTextResult>;
  category: string;
};
const operationsRegistry = new Map<string, OperationConfig>();

// Category mapping for operations
function inferOperationCategory(name: string): string {
  if (name.startsWith("session_") || name.startsWith("context_")) return "Session";
  if (name.startsWith("memory_")) return "Memory";
  if (name.startsWith("search_")) return "Search";
  if (name.startsWith("graph_")) return "Graph";
  if (name.startsWith("workspace")) return "Workspace";
  if (name.startsWith("project")) return "Project";
  if (name.startsWith("reminder")) return "Reminders";
  if (name.startsWith("slack_") || name.startsWith("github_") || name.startsWith("integration"))
    return "Integrations";
  if (name.startsWith("ai_")) return "AI";
  if (
    name === "auth_me" ||
    name === "mcp_server_version" ||
    name === "generate_editor_rules" ||
    name === "generate_rules"
  )
    return "Utility";
  if (name === "tools_enable_bundle" || name === "contextstream" || name === "contextstream_help")
    return "Meta";
  return "Other";
}

// Get compact operation list for help
function getOperationCatalog(category?: string): string {
  const ops: Record<string, string[]> = {};

  for (const [name, config] of operationsRegistry) {
    const cat = config.category;
    if (category && cat.toLowerCase() !== category.toLowerCase()) continue;
    if (!ops[cat]) ops[cat] = [];
    ops[cat].push(name);
  }

  const lines: string[] = [];
  for (const [cat, names] of Object.entries(ops).sort()) {
    lines.push(`${cat}: ${names.join(", ")}`);
  }

  return lines.join("\n");
}

// Get operation schema for help
type OperationSchemaInfo = {
  name: string;
  title: string;
  description: string;
  category: string;
  schema: { type: string; properties?: Record<string, any>; required?: string[] };
};

function getOperationSchema(name: string): OperationSchemaInfo | null {
  const op = operationsRegistry.get(name);
  if (!op) return null;

  // Convert Zod schema to JSON schema (simplified)
  const zodSchema = op.inputSchema;
  try {
    // Use zod's built-in JSON schema conversion if available
    const shape = (zodSchema as any)?._def?.shape?.() || (zodSchema as any)?.shape || {};
    const properties: Record<string, any> = {};
    const required: string[] = [];

    for (const [key, field] of Object.entries(shape)) {
      const f = field as any;
      const isOptional = f?._def?.typeName === "ZodOptional" || f?.isOptional?.();
      const innerType = isOptional ? f?._def?.innerType : f;
      const typeName = innerType?._def?.typeName || "unknown";
      const description = f?._def?.description || innerType?._def?.description;

      let type = "string";
      if (typeName === "ZodNumber") type = "number";
      else if (typeName === "ZodBoolean") type = "boolean";
      else if (typeName === "ZodArray") type = "array";
      else if (typeName === "ZodObject") type = "object";
      else if (typeName === "ZodEnum") type = "enum";

      properties[key] = { type };
      if (description) properties[key].description = description;
      if (typeName === "ZodEnum") {
        properties[key].enum = innerType?._def?.values;
      }

      if (!isOptional) required.push(key);
    }

    return {
      name: op.name,
      title: op.title,
      description: op.description,
      category: op.category,
      schema: {
        type: "object",
        properties,
        required: required.length > 0 ? required : undefined,
      },
    };
  } catch {
    return {
      name: op.name,
      title: op.title,
      description: op.description,
      category: op.category,
      schema: { type: "object" },
    };
  }
}

// =============================================================================
// END Strategy 6
// =============================================================================

// =============================================================================
// Strategy 7: Tool Output Verbosity Reduction
// =============================================================================
// Environment variable to control output format
// CONTEXTSTREAM_OUTPUT_FORMAT=compact | pretty (default: compact)
// Compact mode uses minified JSON (~30% fewer tokens per response)
function parsePositiveInt(raw: string | undefined, fallback: number): number {
  const parsed = Number.parseInt(raw ?? "", 10);
  return Number.isFinite(parsed) && parsed > 0 ? parsed : fallback;
}

const OUTPUT_FORMAT = process.env.CONTEXTSTREAM_OUTPUT_FORMAT || "compact";
const COMPACT_OUTPUT = OUTPUT_FORMAT === "compact";
const DEFAULT_SEARCH_LIMIT = parsePositiveInt(process.env.CONTEXTSTREAM_SEARCH_LIMIT, 3);
const DEFAULT_SEARCH_CONTENT_MAX_CHARS = parsePositiveInt(
  process.env.CONTEXTSTREAM_SEARCH_MAX_CHARS,
  400
);

// =============================================================================
// END Strategy 7
// =============================================================================

// =============================================================================
// Strategy 8: Consolidated Domain Tools (v0.4.x default)
// =============================================================================
// Environment variable to control consolidated mode
// CONTEXTSTREAM_CONSOLIDATED=true | false (default: true in v0.4.x)
// When enabled, registers ~11 domain tools instead of ~58 individual tools
// This provides ~75% token reduction while maintaining full functionality
const CONSOLIDATED_MODE = process.env.CONTEXTSTREAM_CONSOLIDATED !== "false";

// Consolidated tools list - these are the only tools registered in consolidated mode
const CONSOLIDATED_TOOLS = new Set<string>([
  "session_init", // Standalone - complex initialization
  "context_smart", // Standalone - called every message
  "generate_rules", // Standalone - rule generation helper
  "search", // Consolidates search_semantic, search_hybrid, search_keyword, search_pattern
  "session", // Consolidates session_capture, session_recall, etc.
  "memory", // Consolidates memory_create_event, memory_get_event, etc.
  "graph", // Consolidates graph_dependencies, graph_impact, etc.
  "project", // Consolidates projects_list, projects_create, etc.
  "workspace", // Consolidates workspaces_list, workspace_associate, etc.
  "reminder", // Consolidates reminders_list, reminders_create, etc.
  "integration", // Consolidates slack_*, github_*, notion_*, integrations_*
  "help", // Consolidates session_tools, auth_me, mcp_server_version, etc.
]);

// =============================================================================
// END Strategy 8
// =============================================================================

const TOOLSET_ALIASES: Record<string, Set<string> | null> = {
  // Light mode - minimal, fastest
  light: LIGHT_TOOLSET,
  minimal: LIGHT_TOOLSET,
  // Standard mode - balanced (default)
  standard: STANDARD_TOOLSET,
  core: STANDARD_TOOLSET,
  essential: STANDARD_TOOLSET,
  // Complete mode - all tools
  complete: null,
  full: null,
  all: null,
  // Auto mode - handled separately in resolveToolFilter, but listed here for reference
  // auto: STANDARD_TOOLSET (will be adjusted based on client detection)
};

function parseToolList(raw: string): Set<string> {
  return new Set(
    raw
      .split(",")
      .map((tool) => tool.trim())
      .filter(Boolean)
  );
}

function resolveToolFilter(): {
  allowlist: Set<string> | null;
  source: string;
  autoDetected: boolean;
} {
  const allowlistRaw = process.env.CONTEXTSTREAM_TOOL_ALLOWLIST;
  if (allowlistRaw) {
    const allowlist = parseToolList(allowlistRaw);
    if (allowlist.size === 0) {
      console.error(
        "[ContextStream] CONTEXTSTREAM_TOOL_ALLOWLIST is empty; using standard toolset."
      );
      return { allowlist: STANDARD_TOOLSET, source: "standard", autoDetected: false };
    }
    return { allowlist, source: "allowlist", autoDetected: false };
  }

  const toolsetRaw = process.env.CONTEXTSTREAM_TOOLSET;

  // If no explicit toolset is set, check for client detection (Strategy 3 - Option A Fallback)
  if (!toolsetRaw) {
    // Check environment for Claude Code indicators
    clientDetectedFromEnv = detectClaudeCodeFromEnv();

    if (clientDetectedFromEnv && AUTO_TOOLSET_ENABLED) {
      const recommended = getRecommendedToolset(undefined, true);
      if (recommended) {
        console.error(
          "[ContextStream] Detected Claude Code via environment. Using light toolset for optimal token usage."
        );
        return { allowlist: recommended, source: "auto-claude", autoDetected: true };
      }
    }

    // Default to standard toolset
    return { allowlist: STANDARD_TOOLSET, source: "standard", autoDetected: false };
  }

  // Handle 'auto' toolset explicitly
  if (toolsetRaw.trim().toLowerCase() === "auto") {
    clientDetectedFromEnv = detectClaudeCodeFromEnv();
    if (clientDetectedFromEnv) {
      console.error("[ContextStream] TOOLSET=auto: Detected Claude Code, using light toolset.");
      return { allowlist: LIGHT_TOOLSET, source: "auto-claude", autoDetected: true };
    }
    // Will be updated when clientInfo is received (Option B)
    console.error(
      "[ContextStream] TOOLSET=auto: Will adjust toolset based on MCP client (currently standard)."
    );
    return { allowlist: STANDARD_TOOLSET, source: "auto-pending", autoDetected: true };
  }

  const key = toolsetRaw.trim().toLowerCase();
  if (key in TOOLSET_ALIASES) {
    const resolved = TOOLSET_ALIASES[key];
    // null means complete/full toolset
    if (resolved === null) {
      return { allowlist: null, source: "complete", autoDetected: false };
    }
    return { allowlist: resolved, source: key, autoDetected: false };
  }

  console.error(
    `[ContextStream] Unknown CONTEXTSTREAM_TOOLSET "${toolsetRaw}". Using standard toolset.`
  );
  return { allowlist: STANDARD_TOOLSET, source: "standard", autoDetected: false };
}

// Strategy 7: Use compact JSON by default to reduce token usage
function formatContent(data: unknown, forceFormat?: "compact" | "pretty") {
  const usePretty = forceFormat === "pretty" || (!forceFormat && !COMPACT_OUTPUT);
  return usePretty ? JSON.stringify(data, null, 2) : JSON.stringify(data);
}

function toStructured(data: unknown): StructuredContent {
  if (data && typeof data === "object" && !Array.isArray(data)) {
    return data as { [x: string]: unknown };
  }
  return undefined;
}

// Token savings tracking is imported from ./token-savings.js

function readStatNumber(payload: unknown, key: string): number | undefined {
  if (!payload || typeof payload !== "object") return undefined;
  const direct = (payload as Record<string, unknown>)[key];
  if (typeof direct === "number") return direct;
  const nested = (payload as Record<string, unknown>).data;
  if (nested && typeof nested === "object") {
    const nestedValue = (nested as Record<string, unknown>)[key];
    if (typeof nestedValue === "number") return nestedValue;
  }
  return undefined;
}

function estimateGraphIngestMinutes(
  stats: unknown
): { min: number; max: number; basis?: string } | null {
  const totalFiles = readStatNumber(stats, "total_files");
  const totalLines = readStatNumber(stats, "total_lines");
  if (!totalFiles && !totalLines) return null;

  const fileScore = totalFiles ? totalFiles / 1000 : 0;
  const lineScore = totalLines ? totalLines / 50000 : 0;
  const sizeScore = Math.max(fileScore, lineScore);

  const minMinutes = Math.min(45, Math.max(1, Math.round(1 + sizeScore * 1.5)));
  const maxMinutes = Math.min(60, Math.max(minMinutes + 1, Math.round(2 + sizeScore * 3)));

  const basisParts = [];
  if (totalFiles) basisParts.push(`${totalFiles.toLocaleString()} files`);
  if (totalLines) basisParts.push(`${totalLines.toLocaleString()} lines`);

  return {
    min: minMinutes,
    max: maxMinutes,
    basis: basisParts.length > 0 ? basisParts.join(" / ") : undefined,
  };
}

function normalizeLessonField(value: string | undefined | null) {
  return (value || "").trim().toLowerCase().replace(/\s+/g, " ");
}

function buildLessonSignature(
  input: {
    title: string;
    category?: string;
    trigger: string;
    impact: string;
    prevention: string;
  },
  workspaceId: string,
  projectId?: string
) {
  return [
    workspaceId,
    projectId || "global",
    input.category,
    input.title,
    input.trigger,
    input.impact,
    input.prevention,
  ]
    .map(normalizeLessonField)
    .join("|");
}

function isDuplicateLessonCapture(signature: string) {
  const now = Date.now();
  for (const [key, ts] of recentLessonCaptures) {
    if (now - ts > LESSON_DEDUP_WINDOW_MS) {
      recentLessonCaptures.delete(key);
    }
  }
  const last = recentLessonCaptures.get(signature);
  if (last && now - last < LESSON_DEDUP_WINDOW_MS) {
    recentLessonCaptures.set(signature, now);
    return true;
  }
  recentLessonCaptures.set(signature, now);
  return false;
}

/**
 * Set up the MCP oninitialized callback to detect client info (Strategy 3 - Option B Primary).
 * This should be called after createing the McpServer but before connecting.
 *
 * When the MCP client sends clientInfo during initialization, we check if it's a
 * token-sensitive client and emit tools/list_changed if we should reduce the toolset.
 */
export function setupClientDetection(server: McpServer): void {
  // Skip if auto-toolset is disabled
  if (!AUTO_TOOLSET_ENABLED) {
    console.error(
      "[ContextStream] Auto-toolset: DISABLED (set CONTEXTSTREAM_AUTO_TOOLSET=true to enable)"
    );
    return;
  }

  // If we already detected from environment, no need for dynamic detection
  if (clientDetectedFromEnv) {
    console.error(
      "[ContextStream] Client detection: Already detected Claude Code from environment"
    );
    return;
  }

  // Set up the oninitialized callback on the low-level server
  const lowLevelServer = (server as any).server;
  if (!lowLevelServer) {
    console.error(
      "[ContextStream] Warning: Could not access low-level MCP server for client detection"
    );
    return;
  }

  lowLevelServer.oninitialized = () => {
    try {
      // Get clientInfo from the server
      const clientVersion = lowLevelServer.getClientVersion?.();
      if (clientVersion) {
        detectedClientInfo = clientVersion;
        const clientName = clientVersion.name || "unknown";
        const clientVer = clientVersion.version || "unknown";
        console.error(`[ContextStream] MCP Client detected: ${clientName} v${clientVer}`);

        // Check if we should switch to a lighter toolset
        if (isTokenSensitiveClient(clientName)) {
          console.error(
            "[ContextStream] Token-sensitive client detected. Consider using CONTEXTSTREAM_TOOLSET=light for optimal performance."
          );

          // Emit tools/list_changed notification if the client supports it
          // Note: This won't actually change the tools (they're already registered),
          // but it signals to clients that support dynamic updates
          try {
            lowLevelServer.sendToolsListChanged?.();
            console.error("[ContextStream] Emitted tools/list_changed notification");
          } catch (error) {
            // Client might not support this notification
          }
        }
      }
    } catch (error) {
      console.error("[ContextStream] Error in client detection callback:", error);
    }
  };

  console.error("[ContextStream] Client detection: Callback registered for MCP initialize");
}

export function registerTools(
  server: McpServer,
  client: ContextStreamClient,
  sessionManager?: SessionManager
) {
  const upgradeUrl = process.env.CONTEXTSTREAM_UPGRADE_URL || "https://contextstream.io/pricing";
  const toolFilter = resolveToolFilter();
  const toolAllowlist = toolFilter.allowlist;

  // Log toolset selection with auto-detection info
  if (toolAllowlist) {
    const source = toolFilter.source;
    const autoNote = toolFilter.autoDetected ? " (auto-detected)" : "";
    const hint =
      source === "light" || source === "auto-claude"
        ? " Set CONTEXTSTREAM_TOOLSET=standard or complete for more tools."
        : source === "standard"
          ? " Set CONTEXTSTREAM_TOOLSET=complete for all tools."
          : source === "auto-pending"
            ? " Toolset may be adjusted when MCP client is detected."
            : "";
    console.error(
      `[ContextStream] Toolset: ${source} (${toolAllowlist.size} tools)${autoNote}.${hint}`
    );
  } else {
    console.error(`[ContextStream] Toolset: complete (all tools).`);
  }

  // Log auto-toolset status (Strategy 3)
  if (AUTO_TOOLSET_ENABLED) {
    if (clientDetectedFromEnv) {
      console.error("[ContextStream] Auto-toolset: ACTIVE (Claude Code detected from environment)");
    } else {
      console.error("[ContextStream] Auto-toolset: ENABLED (will detect MCP client on initialize)");
    }
  }

  // Log integration auto-hide status (Strategy 2)
  if (AUTO_HIDE_INTEGRATIONS) {
    console.error(
      `[ContextStream] Integration auto-hide: ENABLED (${ALL_INTEGRATION_TOOLS.size} tools hidden until integrations connected)`
    );
    console.error("[ContextStream] Set CONTEXTSTREAM_AUTO_HIDE_INTEGRATIONS=false to disable.");
  } else {
    console.error("[ContextStream] Integration auto-hide: disabled");
  }

  // Log schema mode status (Strategy 4)
  if (COMPACT_SCHEMA_ENABLED) {
    console.error("[ContextStream] Schema mode: COMPACT (shorter descriptions, minimal params)");
  } else {
    console.error(
      "[ContextStream] Schema mode: full (set CONTEXTSTREAM_SCHEMA_MODE=compact to reduce token overhead)"
    );
  }

  // Log progressive disclosure status (Strategy 5)
  if (PROGRESSIVE_MODE) {
    const coreBundle = TOOL_BUNDLES.core;
    console.error(
      `[ContextStream] Progressive mode: ENABLED (starting with ${coreBundle.size} core tools)`
    );
    console.error(
      "[ContextStream] Use tools_enable_bundle to unlock additional tool bundles dynamically."
    );
  }

  // Log router mode status (Strategy 6)
  if (ROUTER_MODE) {
    console.error(
      "[ContextStream] Router mode: ENABLED (all operations accessed via contextstream/contextstream_help)"
    );
    console.error(
      "[ContextStream] Only 2 tools registered. Use contextstream_help to see available operations."
    );
  }

  // Log output format status (Strategy 7)
  if (COMPACT_OUTPUT) {
    console.error(
      "[ContextStream] Output format: COMPACT (minified JSON, ~30% fewer tokens per response)"
    );
  } else {
    console.error(
      "[ContextStream] Output format: pretty (set CONTEXTSTREAM_OUTPUT_FORMAT=compact for fewer tokens)"
    );
  }

  // Log consolidated mode status (Strategy 8)
  if (CONSOLIDATED_MODE) {
    console.error(
      `[ContextStream] Consolidated mode: ENABLED (~${CONSOLIDATED_TOOLS.size} domain tools, ~75% token reduction)`
    );
    console.error("[ContextStream] Set CONTEXTSTREAM_CONSOLIDATED=false to use individual tools.");
  } else {
    console.error("[ContextStream] Consolidated mode: disabled (using individual tools)");
  }

  // Store server reference for deferred tool registration
  const serverRef = server;

  const defaultProTools = new Set<string>([
    // AI endpoints (typically paid/credit-metered)
    "ai_context",
    "ai_enhanced_context",
    "ai_context_budget",
    "ai_embeddings",
    "ai_plan",
    "ai_tasks",
    // Slack integration tools
    "slack_stats",
    "slack_channels",
    "slack_contributors",
    "slack_activity",
    "slack_discussions",
    "slack_search",
    "slack_sync_users",
    // GitHub integration tools
    "github_stats",
    "github_repos",
    "github_contributors",
    "github_activity",
    "github_issues",
    "github_search",
    // Notion integration tools
    "notion_create_page",
    "notion_list_databases",
    "notion_search_pages",
    "notion_get_page",
    "notion_query_database",
    "notion_update_page",
    "notion_stats",
    "notion_activity",
    "notion_knowledge",
    "notion_summary",
  ]);

  const proTools = (() => {
    const raw = process.env.CONTEXTSTREAM_PRO_TOOLS;
    if (!raw) return defaultProTools;
    const parsed = raw
      .split(",")
      .map((t) => t.trim())
      .filter(Boolean);
    return parsed.length > 0 ? new Set(parsed) : defaultProTools;
  })();

  function getToolAccessTier(toolName: string): "free" | "pro" {
    return proTools.has(toolName) ? "pro" : "free";
  }

  function getToolAccessLabel(
    toolName: string
  ): "Free" | "PRO" | "Pro (Graph-Lite)" | "Elite/Team (Full Graph)" {
    const graphTier = graphToolTiers.get(toolName);
    if (graphTier === "lite") return "Pro (Graph-Lite)";
    if (graphTier === "full") return "Elite/Team (Full Graph)";
    return getToolAccessTier(toolName) === "pro" ? "PRO" : "Free";
  }

  async function gateIfProTool(toolName: string): Promise<ToolTextResult | null> {
    if (getToolAccessTier(toolName) !== "pro") return null;

    const planName = await client.getPlanName();
    if (planName !== "free") return null;

    return errorResult(
      [`Access denied: \`${toolName}\` requires ContextStream PRO.`, `Upgrade: ${upgradeUrl}`].join(
        "\n"
      )
    );
  }

  const graphToolTiers = new Map<string, "lite" | "full">([
    ["graph_dependencies", "lite"],
    ["graph_impact", "lite"],
    ["graph_related", "full"],
    ["graph_decisions", "full"],
    ["graph_path", "full"],
    ["graph_call_path", "full"],
    ["graph_circular_dependencies", "full"],
    ["graph_unused_code", "full"],
    ["graph_ingest", "lite"], // Pro can ingest (builds module-level graph), Elite gets full graph
    ["graph_contradictions", "full"],
  ]);

  const graphLiteMaxDepth = 1;

  function normalizeGraphTargetType(value: unknown): string {
    return String(value ?? "")
      .trim()
      .toLowerCase();
  }

  function isModuleTargetType(value: string): boolean {
    return value === "module" || value === "file" || value === "path";
  }

  function graphLiteConstraintError(toolName: string, detail: string): ToolTextResult {
    return errorResult(
      [
        `Access denied: \`${toolName}\` is limited to Graph-Lite (module-level, 1-hop queries).`,
        detail,
        `Upgrade to Elite or Team for full graph access: ${upgradeUrl}`,
      ].join("\n")
    );
  }

  // ============================================
  // INTEGRATION STATUS TRACKING (Strategy 2)
  // ============================================

  // Track integration status - updated when session_init or integrations_status is called
  let integrationStatus: {
    checked: boolean;
    slack: boolean;
    github: boolean;
    notion: boolean;
    workspaceId?: string;
  } = { checked: false, slack: false, github: false, notion: false };

  // Track if we've already notified about tools list change
  let toolsListChangedNotified = false;

  /**
   * Check integration status for the current workspace.
   * Caches result per workspace to avoid repeated API calls.
   */
  async function checkIntegrationStatus(
    workspaceId?: string
  ): Promise<{ slack: boolean; github: boolean; notion: boolean }> {
    // If we already checked for this workspace, return cached result
    if (integrationStatus.checked && integrationStatus.workspaceId === workspaceId) {
      return { slack: integrationStatus.slack, github: integrationStatus.github, notion: integrationStatus.notion };
    }

    // If no workspace, assume no integrations
    if (!workspaceId) {
      return { slack: false, github: false, notion: false };
    }

    try {
      const status = await client.integrationsStatus({ workspace_id: workspaceId });
      const slackConnected =
        status?.some(
          (s: { provider: string; status: string }) =>
            s.provider === "slack" && s.status === "connected"
        ) ?? false;
      const githubConnected =
        status?.some(
          (s: { provider: string; status: string }) =>
            s.provider === "github" && s.status === "connected"
        ) ?? false;
      const notionConnected =
        status?.some(
          (s: { provider: string; status: string }) =>
            s.provider === "notion" && s.status === "connected"
        ) ?? false;

      integrationStatus = {
        checked: true,
        slack: slackConnected,
        github: githubConnected,
        notion: notionConnected,
        workspaceId,
      };

      console.error(
        `[ContextStream] Integration status: Slack=${slackConnected}, GitHub=${githubConnected}, Notion=${notionConnected}`
      );

      return { slack: slackConnected, github: githubConnected, notion: notionConnected };
    } catch (error) {
      console.error("[ContextStream] Failed to check integration status:", error);
      // On error, assume no integrations
      return { slack: false, github: false, notion: false };
    }
  }

  /**
   * Update integration status (called from session_init or integrations_status tools).
   * If integrations are newly detected, emit tools/list_changed notification.
   */
  function updateIntegrationStatus(
    status: { slack: boolean; github: boolean; notion: boolean },
    workspaceId?: string
  ) {
    const hadSlack = integrationStatus.slack;
    const hadGithub = integrationStatus.github;
    const hadNotion = integrationStatus.notion;

    integrationStatus = {
      checked: true,
      slack: status.slack,
      github: status.github,
      notion: status.notion,
      workspaceId,
    };

    // If integrations were newly detected and we're auto-hiding, notify about tool list change
    if (AUTO_HIDE_INTEGRATIONS && !toolsListChangedNotified) {
      const newlyConnected = (!hadSlack && status.slack) || (!hadGithub && status.github) || (!hadNotion && status.notion);
      if (newlyConnected) {
        try {
          // Emit notification that tools list has changed
          // This allows clients that support dynamic tool updates to refresh
          (server as any).server?.sendToolsListChanged?.();
          toolsListChangedNotified = true;
          console.error(
            "[ContextStream] Emitted tools/list_changed notification (integrations detected)"
          );
        } catch (error) {
          console.error("[ContextStream] Failed to emit tools/list_changed:", error);
        }
      }
    }
  }

  /**
   * Gate function for integration tools (Option A - Lazy Evaluation).
   * Returns an error result if the required integration is not connected.
   */
  async function gateIfIntegrationTool(toolName: string): Promise<ToolTextResult | null> {
    // Skip gating if auto-hide is disabled
    if (!AUTO_HIDE_INTEGRATIONS) return null;

    // Determine which integration this tool requires
    const requiresSlack = SLACK_TOOLS.has(toolName);
    const requiresGithub = GITHUB_TOOLS.has(toolName);
    const requiresNotion = NOTION_TOOLS.has(toolName);
    const requiresCrossIntegration = CROSS_INTEGRATION_TOOLS.has(toolName);

    // Not an integration tool
    if (!requiresSlack && !requiresGithub && !requiresNotion && !requiresCrossIntegration) {
      return null;
    }

    // Get workspace ID from session context
    const workspaceId = sessionManager?.getContext()?.workspace_id as string | undefined;

    // Check integration status
    const status = await checkIntegrationStatus(workspaceId);

    // Gate Slack tools
    if (requiresSlack && !status.slack) {
      return errorResult(
        [
          `Integration not connected: \`${toolName}\` requires Slack integration.`,
          "",
          "To use Slack tools:",
          "1. Go to https://contextstream.io/settings/integrations",
          "2. Connect your Slack workspace",
          "3. Try this command again",
          "",
          "Note: Even without explicit Slack tools, context_smart and session_smart_search",
          "will automatically include relevant Slack context when the integration is connected.",
        ].join("\n")
      );
    }

    // Gate GitHub tools
    if (requiresGithub && !status.github) {
      return errorResult(
        [
          `Integration not connected: \`${toolName}\` requires GitHub integration.`,
          "",
          "To use GitHub tools:",
          "1. Go to https://contextstream.io/settings/integrations",
          "2. Connect your GitHub repositories",
          "3. Try this command again",
          "",
          "Note: Even without explicit GitHub tools, context_smart and session_smart_search",
          "will automatically include relevant GitHub context when the integration is connected.",
        ].join("\n")
      );
    }

    // Gate Notion tools
    if (requiresNotion && !status.notion) {
      return errorResult(
        [
          `Integration not connected: \`${toolName}\` requires Notion integration.`,
          "",
          "To use Notion tools:",
          "1. Go to https://contextstream.io/settings/integrations",
          "2. Connect your Notion workspace",
          "3. Try this command again",
        ].join("\n")
      );
    }

    // Gate cross-integration tools (require at least one integration)
    if (requiresCrossIntegration && !status.slack && !status.github) {
      return errorResult(
        [
          `Integration not connected: \`${toolName}\` requires at least one integration (Slack or GitHub).`,
          "",
          "To use cross-integration tools:",
          "1. Go to https://contextstream.io/settings/integrations",
          "2. Connect Slack and/or GitHub",
          "3. Try this command again",
        ].join("\n")
      );
    }

    return null;
  }

  /**
   * Determine if an integration tool should be registered at startup.
   * When AUTO_HIDE_INTEGRATIONS is true, integration tools are hidden by default
   * and only shown after integration status is confirmed.
   */
  function shouldRegisterIntegrationTool(toolName: string): boolean {
    if (!AUTO_HIDE_INTEGRATIONS) return true;

    // If we haven't checked integrations yet, don't register integration tools
    // They'll be gated by lazy evaluation if called anyway
    if (!integrationStatus.checked) {
      return !ALL_INTEGRATION_TOOLS.has(toolName);
    }

    // Register Slack tools only if Slack is connected
    if (SLACK_TOOLS.has(toolName)) {
      return integrationStatus.slack;
    }

    // Register GitHub tools only if GitHub is connected
    if (GITHUB_TOOLS.has(toolName)) {
      return integrationStatus.github;
    }

    // Register Notion tools only if Notion is connected
    if (NOTION_TOOLS.has(toolName)) {
      return integrationStatus.notion;
    }

    // Register cross-integration tools if at least one integration is connected
    if (CROSS_INTEGRATION_TOOLS.has(toolName)) {
      return integrationStatus.slack || integrationStatus.github;
    }

    return true;
  }

  // ============================================
  // END INTEGRATION STATUS TRACKING
  // ============================================

  async function gateIfGraphTool(toolName: string, input?: any): Promise<ToolTextResult | null> {
    const requiredTier = graphToolTiers.get(toolName);
    if (!requiredTier) return null;

    const graphTier = await client.getGraphTier();

    if (graphTier === "full") return null;

    if (graphTier === "lite") {
      if (requiredTier === "full") {
        return errorResult(
          [
            `Access denied: \`${toolName}\` requires Elite or Team (Full Graph).`,
            "Pro includes Graph-Lite (module-level dependencies and 1-hop impact only).",
            `Upgrade: ${upgradeUrl}`,
          ].join("\n")
        );
      }

      if (toolName === "graph_dependencies") {
        const targetType = normalizeGraphTargetType(input?.target?.type);
        if (!isModuleTargetType(targetType)) {
          return graphLiteConstraintError(toolName, "Set target.type to module, file, or path.");
        }
        if (typeof input?.max_depth === "number" && input.max_depth > graphLiteMaxDepth) {
          return graphLiteConstraintError(
            toolName,
            `Set max_depth to ${graphLiteMaxDepth} or lower.`
          );
        }
        if (input?.include_transitive === true) {
          return graphLiteConstraintError(toolName, "Set include_transitive to false.");
        }
      }

      if (toolName === "graph_impact") {
        const targetType = normalizeGraphTargetType(input?.target?.type);
        if (!isModuleTargetType(targetType)) {
          return graphLiteConstraintError(toolName, "Set target.type to module, file, or path.");
        }
        if (typeof input?.max_depth === "number" && input.max_depth > graphLiteMaxDepth) {
          return graphLiteConstraintError(
            toolName,
            `Set max_depth to ${graphLiteMaxDepth} or lower.`
          );
        }
      }

      return null;
    }

    return errorResult(
      [
        `Access denied: \`${toolName}\` requires ContextStream Pro (Graph-Lite) or Elite/Team (Full Graph).`,
        `Upgrade: ${upgradeUrl}`,
      ].join("\n")
    );
  }

  /**
   * AUTO-CONTEXT WRAPPER
   *
   * This wraps tool handlers to automatically initialize session context
   * on the FIRST tool call of any conversation.
   *
   * Benefits:
   * - Works with ALL MCP clients (Windsurf, Cursor, Claude Desktop, VS Code, etc.)
   * - No client-side changes required
   * - Context is loaded regardless of which tool the AI calls first
   * - Only runs once per session (efficient)
   */
  function wrapWithAutoContext<T, R>(
    toolName: string,
    handler: (input: T, extra?: MessageExtraInfo) => Promise<R>
  ): (input: T, extra?: MessageExtraInfo) => Promise<R> {
    if (!sessionManager) {
      return async (input: T, extra?: MessageExtraInfo): Promise<R> => {
        const authOverride = resolveAuthOverride(extra);
        return runWithAuthOverride(authOverride, async () => handler(input, extra));
      };
    }

    return async (input: T, extra?: MessageExtraInfo): Promise<R> => {
      const authOverride = resolveAuthOverride(extra);

      return runWithAuthOverride(authOverride, async () => {
        // Skip auto-init for session_init itself
        const skipAutoInit = toolName === "session_init";

        let contextPrefix = "";

        if (!skipAutoInit) {
          const autoInitResult = await sessionManager.autoInitialize();
          if (autoInitResult) {
            contextPrefix = autoInitResult.contextSummary + "\n\n";
          }
        }

        // Warn if context_smart hasn't been called yet
        sessionManager.warnIfContextSmartNotCalled(toolName);

        // Call the original handler
        const result = await handler(input, extra);

        // Prepend context to the response if we auto-initialized
        if (contextPrefix && result && typeof result === "object") {
          const r = result as { content?: Array<{ type: string; text: string }> };
          if (r.content && r.content.length > 0 && r.content[0].type === "text") {
            r.content[0] = {
              ...r.content[0],
              text: contextPrefix + "--- Tool Response ---\n\n" + r.content[0].text,
            };
          }
        }

        return result;
      });
    };
  }

  /**
   * Helper to register a tool with auto-context wrapper applied.
   * This is a drop-in replacement for server.registerTool that adds auto-context.
   *
   * Includes integration-aware tool hiding:
   * - Option B (dynamic): Skip registration if integration tools and integrations not connected
   * - Option A (lazy): Gate at runtime with helpful error if integration not connected
   */
  /**
   * Actually register a tool with the MCP server.
   * This is the internal registration function used both at startup and when enabling bundles.
   */
  function actuallyRegisterTool<T extends z.ZodType>(
    name: string,
    config: { title: string; description: string; inputSchema: T },
    handler: (input: z.infer<T>, extra?: MessageExtraInfo) => Promise<ToolTextResult>
  ) {
    const accessLabel = getToolAccessLabel(name);
    const showUpgrade = accessLabel !== "Free";

    // Strategy 4: Apply schema compactification in compact mode
    let finalDescription: string;
    let finalSchema: z.ZodTypeAny | undefined;

    if (COMPACT_SCHEMA_ENABLED) {
      finalDescription = compactifyDescription(config.description);
      finalSchema = config.inputSchema
        ? applyCompactParamDescriptions(config.inputSchema)
        : undefined;
    } else {
      finalDescription = `${config.description}\n\nAccess: ${accessLabel}${showUpgrade ? ` (upgrade: ${upgradeUrl})` : ""}`;
      finalSchema = config.inputSchema ? applyParamDescriptions(config.inputSchema) : undefined;
    }

    const labeledConfig = {
      ...config,
      title: COMPACT_SCHEMA_ENABLED ? config.title : `${config.title} (${accessLabel})`,
      description: finalDescription,
    };
    const annotatedConfig = {
      ...labeledConfig,
      inputSchema: finalSchema,
      annotations: {
        ...inferToolAnnotations(name),
        ...(labeledConfig as { annotations?: ToolAnnotations }).annotations,
      },
    };

    // Wrap handler with error handling
    const safeHandler = async (input: z.infer<T>, extra?: MessageExtraInfo) => {
      try {
        const proGated = await gateIfProTool(name);
        if (proGated) return proGated;

        const integrationGated = await gateIfIntegrationTool(name);
        if (integrationGated) return integrationGated;

        return await handler(input, extra);
      } catch (error: any) {
        const errorMessage = error?.message || String(error);
        const errorDetails = error?.body || error?.details || null;
        const errorCode = error?.code || error?.status || "UNKNOWN_ERROR";

        const isPlanLimit =
          String(errorCode).toUpperCase() === "FORBIDDEN" &&
          String(errorMessage).toLowerCase().includes("plan limit reached");
        const upgradeHint = isPlanLimit ? `\nUpgrade: ${upgradeUrl}` : "";

        const isUnauthorized = String(errorCode).toUpperCase() === "UNAUTHORIZED";
        const sessionHint = isUnauthorized
          ? `\nHint: Run session_init(folder_path="<your_project_path>") first to establish a session, or check that your API key is valid.`
          : "";

        const errorPayload = {
          success: false,
          error: {
            code: errorCode,
            message: errorMessage,
            details: errorDetails,
          },
        };
        const errorText = `[${errorCode}] ${errorMessage}${upgradeHint}${sessionHint}${errorDetails ? `: ${JSON.stringify(errorDetails)}` : ""}`;
        return {
          content: [{ type: "text" as const, text: errorText }],
          structuredContent: errorPayload,
          isError: true,
        };
      }
    };

    serverRef.registerTool(name, annotatedConfig, wrapWithAutoContext(name, safeHandler));
  }

  /**
   * Enable a tool bundle dynamically (Strategy 5).
   * Registers all deferred tools from the bundle and notifies clients.
   */
  function enableBundle(bundleName: string): {
    success: boolean;
    message: string;
    toolsEnabled: number;
    hint?: string;
  } {
    if (enabledBundles.has(bundleName)) {
      return {
        success: true,
        message: `Bundle '${bundleName}' is already enabled.`,
        toolsEnabled: 0,
      };
    }

    const bundle = TOOL_BUNDLES[bundleName];
    if (!bundle) {
      return {
        success: false,
        message: `Unknown bundle '${bundleName}'. Available: ${Object.keys(TOOL_BUNDLES).join(", ")}`,
        toolsEnabled: 0,
      };
    }

    enabledBundles.add(bundleName);
    let toolsEnabled = 0;

    // Register all deferred tools from this bundle
    for (const toolName of bundle) {
      const deferred = deferredTools.get(toolName);
      if (deferred) {
        actuallyRegisterTool(deferred.name, deferred.config, deferred.handler);
        deferredTools.delete(toolName);
        toolsEnabled++;
      }
    }

    // Notify clients that tool list has changed
    try {
      const lowLevelServer = serverRef as unknown as { sendToolsListChanged?: () => void };
      lowLevelServer.sendToolsListChanged?.();
    } catch {
      // Client might not support this notification
    }

    console.error(`[ContextStream] Bundle '${bundleName}' enabled with ${toolsEnabled} tools.`);
    return {
      success: true,
      message: `Enabled bundle '${bundleName}' with ${toolsEnabled} tools.`,
      toolsEnabled,
      hint: "If your client doesn't auto-refresh tools, restart the MCP connection to see new tools.",
    };
  }

  // Meta-tools that should always be registered directly (not routed)
  const ROUTER_DIRECT_TOOLS = new Set(["contextstream", "contextstream_help"]);

  function registerTool<T extends z.ZodType>(
    name: string,
    config: { title: string; description: string; inputSchema: T },
    handler: (input: z.infer<T>, extra?: MessageExtraInfo) => Promise<ToolTextResult>
  ) {
    // Strategy 8: Consolidated mode - only register consolidated domain tools
    // Skip individual tools (they're accessed via domain tool dispatch)
    if (CONSOLIDATED_MODE && !CONSOLIDATED_TOOLS.has(name)) {
      // Store handler for consolidated tools to dispatch to
      operationsRegistry.set(name, {
        name,
        title: config.title,
        description: config.description,
        inputSchema: config.inputSchema,
        handler,
        category: inferOperationCategory(name),
      });
      return;
    }

    // Check toolset allowlist first (only applies in non-consolidated mode)
    if (!CONSOLIDATED_MODE && toolAllowlist && !toolAllowlist.has(name)) {
      // In router mode, still store in registry even if not in allowlist
      // (the router can access all operations)
      if (ROUTER_MODE && !ROUTER_DIRECT_TOOLS.has(name)) {
        operationsRegistry.set(name, {
          name,
          title: config.title,
          description: config.description,
          inputSchema: config.inputSchema,
          handler,
          category: inferOperationCategory(name),
        });
      }
      return;
    }

    // Option B: Skip registration for integration tools when auto-hide is enabled
    // and integrations are not connected. This reduces the tool registry size.
    if (!CONSOLIDATED_MODE && !shouldRegisterIntegrationTool(name)) {
      return;
    }

    // Strategy 6: Router mode - store in operations registry instead of registering
    if (ROUTER_MODE && !ROUTER_DIRECT_TOOLS.has(name)) {
      operationsRegistry.set(name, {
        name,
        title: config.title,
        description: config.description,
        inputSchema: config.inputSchema,
        handler,
        category: inferOperationCategory(name),
      });
      return;
    }

    // Strategy 5: Progressive disclosure - defer tools not in enabled bundles
    if (PROGRESSIVE_MODE && !isToolInEnabledBundles(name)) {
      // Store for later registration when bundle is enabled
      deferredTools.set(name, { name, config, handler });
      return;
    }

    // Register the tool immediately
    actuallyRegisterTool(name, config, handler);
  }

  function errorResult(text: string): ToolTextResult {
    return {
      content: [{ type: "text" as const, text }],
      isError: true,
    };
  }

  function resolveWorkspaceId(explicitWorkspaceId?: string): string | undefined {
    const normalizedExplicit = normalizeUuid(explicitWorkspaceId);
    if (normalizedExplicit) return normalizedExplicit;
    const ctx = sessionManager?.getContext();
    return normalizeUuid(
      typeof ctx?.workspace_id === "string" ? (ctx.workspace_id as string) : undefined
    );
  }

  function resolveProjectId(explicitProjectId?: string): string | undefined {
    const normalizedExplicit = normalizeUuid(explicitProjectId);
    if (normalizedExplicit) return normalizedExplicit;
    const ctx = sessionManager?.getContext();
    return normalizeUuid(
      typeof ctx?.project_id === "string" ? (ctx.project_id as string) : undefined
    );
  }

  async function validateReadableDirectory(
    inputPath: string
  ): Promise<{ ok: true; resolvedPath: string } | { ok: false; error: string }> {
    const resolvedPath = path.resolve(inputPath);
    let stats: fs.Stats;
    try {
      stats = await fs.promises.stat(resolvedPath);
    } catch (error: any) {
      if (error?.code === "ENOENT") {
        return { ok: false, error: `Error: path does not exist: ${inputPath}` };
      }
      return {
        ok: false,
        error: `Error: unable to access path: ${inputPath}${error?.message ? ` (${error.message})` : ""}`,
      };
    }

    if (!stats.isDirectory()) {
      return { ok: false, error: `Error: path is not a directory: ${inputPath}` };
    }

    try {
      await fs.promises.access(resolvedPath, fs.constants.R_OK | fs.constants.X_OK);
    } catch (error: any) {
      return {
        ok: false,
        error: `Error: path is not readable: ${inputPath}${error?.code ? ` (${error.code})` : ""}`,
      };
    }

    return { ok: true, resolvedPath };
  }

  function startBackgroundIngest(
    projectId: string,
    resolvedPath: string,
    ingestOptions: { write_to_disk?: boolean; overwrite?: boolean },
    options: { preflight?: boolean } = {}
  ): void {
    (async () => {
      try {
        if (options.preflight) {
          const fileCheck = await countIndexableFiles(resolvedPath, { maxFiles: 1 });
          if (fileCheck.count === 0) {
            console.error(
              `[ContextStream] No indexable files found in ${resolvedPath}. Skipping ingest.`
            );
            return;
          }
        }

        let totalIndexed = 0;
        let batchCount = 0;

        console.error(
          `[ContextStream] Starting background ingestion for project ${projectId} from ${resolvedPath}`
        );

        for await (const batch of readAllFilesInBatches(resolvedPath, { batchSize: 50 })) {
          const result = (await client.ingestFiles(projectId, batch, ingestOptions)) as {
            data?: { files_indexed: number };
          };
          totalIndexed += result.data?.files_indexed ?? batch.length;
          batchCount++;
        }

        console.error(
          `[ContextStream] Completed background ingestion: ${totalIndexed} files in ${batchCount} batches`
        );

        // Mark project as indexed so hooks know to enforce ContextStream-first behavior
        try {
          await markProjectIndexed(resolvedPath, { project_id: projectId });
          console.error(`[ContextStream] Marked project as indexed: ${resolvedPath}`);
        } catch (markError) {
          console.error(`[ContextStream] Failed to mark project as indexed:`, markError);
        }
      } catch (error) {
        console.error(`[ContextStream] Ingestion failed:`, error);
      }
    })();
  }

  // Auth
  registerTool(
    "mcp_server_version",
    {
      title: "Get MCP server version",
      description: "Return the running ContextStream MCP server package version",
      inputSchema: z.object({}),
    },
    async () => {
      const result = { name: "contextstream-mcp", version: VERSION };
      return {
        content: [{ type: "text" as const, text: formatContent(result) }],
        structuredContent: toStructured(result),
      };
    }
  );

  registerTool(
    "auth_me",
    {
      title: "Get current user",
      description: "Fetch authenticated user profile",
      inputSchema: z.object({}),
    },
    async () => {
      const result = await client.me();
      return {
        content: [{ type: "text" as const, text: formatContent(result) }],
        structuredContent: toStructured(result),
      };
    }
  );

  // Strategy 5: Tool bundle management
  registerTool(
    "tools_enable_bundle",
    {
      title: "Enable tool bundle",
      description: `Enable a bundle of related tools dynamically. Only available when CONTEXTSTREAM_PROGRESSIVE_MODE=true.

Available bundles:
- session: Extended session management (~6 tools)
- memory: Full memory CRUD operations (~16 tools)
- search: Semantic, hybrid, and keyword search (~3 tools)
- graph: Code graph analysis and dependencies (~9 tools)
- workspace: Workspace management (~4 tools)
- project: Project management and indexing (~10 tools)
- reminders: Reminder management (~6 tools)
- integrations: Slack and GitHub integrations (~21 tools)

Example: Enable memory tools before using memory_create_event.`,
      inputSchema: z.object({
        bundle: z
          .enum([
            "session",
            "memory",
            "search",
            "graph",
            "workspace",
            "project",
            "reminders",
            "integrations",
          ])
          .describe("Name of the bundle to enable"),
        list_bundles: z
          .boolean()
          .optional()
          .describe("If true, list all available bundles and their status"),
      }),
    },
    async (input) => {
      // If just listing bundles, return bundle info
      if (input.list_bundles) {
        const bundles = getBundleInfo();
        const result = {
          progressive_mode: PROGRESSIVE_MODE,
          bundles,
          hint: PROGRESSIVE_MODE
            ? "Call tools_enable_bundle with a bundle name to enable additional tools."
            : "Progressive mode is disabled. All tools from your toolset are already available.",
        };
        return {
          content: [{ type: "text" as const, text: formatContent(result) }],
          structuredContent: toStructured(result),
        };
      }

      // If progressive mode is disabled, all tools are already available
      if (!PROGRESSIVE_MODE) {
        const result = {
          success: true,
          message: `Progressive mode is disabled. All tools from your toolset are already available. Bundle '${input.bundle}' tools are accessible.`,
          progressive_mode: false,
        };
        return {
          content: [{ type: "text" as const, text: formatContent(result) }],
          structuredContent: toStructured(result),
        };
      }

      // Enable the bundle
      const result = enableBundle(input.bundle);
      const response = {
        ...result,
        progressive_mode: true,
        enabled_bundles: Array.from(enabledBundles),
        hint:
          result.success && result.toolsEnabled > 0
            ? "New tools are now available. The client should refresh its tool list."
            : undefined,
      };
      return {
        content: [{ type: "text" as const, text: formatContent(response) }],
        structuredContent: toStructured(response),
      };
    }
  );

  // Strategy 6: Router meta-tools (only registered when router mode is enabled)
  if (ROUTER_MODE) {
    // Main dispatcher tool
    serverRef.registerTool(
      "contextstream",
      {
        title: "ContextStream Operation",
        description: `Execute any ContextStream operation. Use contextstream_help to see available operations.

Example: contextstream({ op: "session_init", args: { folder_path: "/path/to/project" } })

This single tool replaces 50+ individual tools, dramatically reducing token overhead.
All ContextStream functionality is accessible through this dispatcher.`,
        inputSchema: {
          type: "object" as const,
          properties: {
            op: {
              type: "string",
              description: "Operation name (e.g., session_init, memory_create_event)",
            },
            args: { type: "object", description: "Operation arguments (varies by operation)" },
          },
          required: ["op"],
        },
        annotations: {
          readOnlyHint: false,
          destructiveHint: false,
          idempotentHint: false,
          openWorldHint: false,
        },
      },
      async (input: { op: string; args?: Record<string, unknown> }) => {
        const opName = input.op;
        const operation = operationsRegistry.get(opName);

        if (!operation) {
          const available = getOperationCatalog();
          return {
            content: [
              {
                type: "text" as const,
                text: `Unknown operation: ${opName}\n\nAvailable operations:\n${available}\n\nUse contextstream_help({ op: "operation_name" }) for details.`,
              },
            ],
            isError: true,
          };
        }

        // Validate args against schema
        const args = input.args || {};
        const parsed = operation.inputSchema.safeParse(args);
        if (!parsed.success) {
          const schema = getOperationSchema(opName);
          return {
            content: [
              {
                type: "text" as const,
                text: `Invalid arguments for ${opName}: ${parsed.error.message}\n\nExpected schema:\n${JSON.stringify(schema?.schema || {}, null, 2)}`,
              },
            ],
            isError: true,
          };
        }

        // Execute the operation
        try {
          return await operation.handler(parsed.data);
        } catch (error: any) {
          return {
            content: [
              {
                type: "text" as const,
                text: `Error executing ${opName}: ${error?.message || String(error)}`,
              },
            ],
            isError: true,
          };
        }
      }
    );

    // Help/schema lookup tool
    serverRef.registerTool(
      "contextstream_help",
      {
        title: "ContextStream Help",
        description: `Get help on available ContextStream operations or schema for a specific operation.

Examples:
- contextstream_help({}) - List all operations by category
- contextstream_help({ op: "session_init" }) - Get schema for session_init
- contextstream_help({ category: "Memory" }) - List memory operations only`,
        inputSchema: {
          type: "object" as const,
          properties: {
            op: { type: "string", description: "Operation name to get schema for" },
            category: {
              type: "string",
              description: "Category to filter (Session, Memory, Search, Graph, etc.)",
            },
          },
        },
        annotations: {
          readOnlyHint: true,
          destructiveHint: false,
          idempotentHint: true,
          openWorldHint: false,
        },
      },
      async (input: { op?: string; category?: string }) => {
        // If specific operation requested, return its schema
        if (input.op) {
          const schema = getOperationSchema(input.op);
          if (!schema) {
            return {
              content: [
                {
                  type: "text" as const,
                  text: `Unknown operation: ${input.op}\n\nUse contextstream_help({}) to see available operations.`,
                },
              ],
              isError: true,
            };
          }
          return {
            content: [{ type: "text" as const, text: JSON.stringify(schema, null, 2) }],
            structuredContent: schema as StructuredContent,
          };
        }

        // Return operation catalog
        const catalog = getOperationCatalog(input.category);
        const result = {
          router_mode: true,
          total_operations: operationsRegistry.size,
          categories: catalog,
          usage:
            'Call contextstream({ op: "operation_name", args: {...} }) to execute an operation.',
          hint: 'Use contextstream_help({ op: "operation_name" }) to see the schema for a specific operation.',
        };
        return {
          content: [{ type: "text" as const, text: formatContent(result) }],
          structuredContent: toStructured(result),
        };
      }
    );

    console.error(
      `[ContextStream] Router mode: Registered 2 meta-tools, ${operationsRegistry.size} operations available via dispatcher.`
    );
  }

  // Workspaces
  registerTool(
    "workspaces_list",
    {
      title: "List workspaces",
      description:
        "List accessible workspaces (paginated list: items, total, page, per_page, has_next, has_prev).",
      inputSchema: z.object({ page: z.number().optional(), page_size: z.number().optional() }),
    },
    async (input) => {
      const result = await client.listWorkspaces(input);
      return {
        content: [{ type: "text" as const, text: formatContent(result) }],
        structuredContent: toStructured(result),
      };
    }
  );

  registerTool(
    "workspaces_create",
    {
      title: "Create workspace",
      description: "Create a new workspace (returns ApiResponse with created workspace in data).",
      inputSchema: z.object({
        name: z.string(),
        description: z.string().optional(),
        visibility: z.enum(["private", "team", "org"]).optional(),
      }),
    },
    async (input) => {
      const result = await client.createWorkspace(input);
      return {
        content: [{ type: "text" as const, text: formatContent(result) }],
        structuredContent: toStructured(result),
      };
    }
  );

  registerTool(
    "workspaces_update",
    {
      title: "Update workspace",
      description: "Update a workspace (rename, change description, or visibility)",
      inputSchema: z.object({
        workspace_id: z.string().uuid(),
        name: z.string().optional(),
        description: z.string().optional(),
        visibility: z.enum(["private", "team", "org"]).optional(),
      }),
    },
    async (input) => {
      const { workspace_id, ...updates } = input;
      const result = await client.updateWorkspace(workspace_id, updates);
      return {
        content: [{ type: "text" as const, text: formatContent(result) }],
        structuredContent: toStructured(result),
      };
    }
  );

  registerTool(
    "workspaces_delete",
    {
      title: "Delete workspace",
      description:
        "Delete a workspace and all its contents (projects, memory, etc.). This action is irreversible.",
      inputSchema: z.object({
        workspace_id: z.string().uuid(),
      }),
    },
    async (input) => {
      const result = await client.deleteWorkspace(input.workspace_id);
      // Normalize response to match {success, data, error, metadata} structure
      const normalized = result || {
        success: true,
        data: { id: input.workspace_id, deleted: true },
        error: null,
        metadata: {},
      };
      return {
        content: [{ type: "text" as const, text: formatContent(normalized) }],
        structuredContent: toStructured(normalized),
      };
    }
  );

  // Projects
  registerTool(
    "projects_list",
    {
      title: "List projects",
      description:
        "List projects (optionally by workspace; paginated list: items, total, page, per_page, has_next, has_prev).",
      inputSchema: z.object({
        workspace_id: z.string().uuid().optional(),
        page: z.number().optional(),
        page_size: z.number().optional(),
      }),
    },
    async (input) => {
      const result = await client.listProjects(input);
      return {
        content: [{ type: "text" as const, text: formatContent(result) }],
        structuredContent: toStructured(result),
      };
    }
  );

  registerTool(
    "projects_create",
    {
      title: "Create project",
      description: `Create a new project within a workspace.
Use this when you need to create a project for a specific folder/codebase.
If workspace_id is not provided, uses the current session's workspace.
Optionally associates a local folder and generates AI editor rules.

Access: Free`,
      inputSchema: z.object({
        name: z.string().describe("Project name"),
        description: z.string().optional().describe("Project description"),
        workspace_id: z
          .string()
          .uuid()
          .optional()
          .describe("Workspace ID (uses current session workspace if not provided)"),
        folder_path: z
          .string()
          .optional()
          .describe("Optional: Local folder path to associate with this project"),
        generate_editor_rules: z
          .boolean()
          .optional()
          .describe("Generate AI editor rules in folder_path (requires folder_path)"),
        overwrite_existing: z
          .boolean()
          .optional()
          .describe("Allow overwriting existing rule files when generating editor rules"),
      }),
    },
    async (input) => {
      // Resolve workspace ID from session if not provided
      const workspaceId = resolveWorkspaceId(input.workspace_id);
      if (!workspaceId) {
        return errorResult(
          "Error: workspace_id is required. Please call session_init first or provide workspace_id explicitly."
        );
      }

      // Create the project
      const result = await client.createProject({
        name: input.name,
        description: input.description,
        workspace_id: workspaceId,
      });

      const projectData = result as { id?: string; name?: string };
      let rulesGenerated: string[] = [];
      let rulesSkipped: string[] = [];

      // If folder_path provided, associate it with the project
      if (input.folder_path && projectData.id) {
        try {
          // Write project config to folder
          const configDir = path.join(input.folder_path, ".contextstream");
          const configPath = path.join(configDir, "config.json");

          if (!fs.existsSync(configDir)) {
            fs.mkdirSync(configDir, { recursive: true });
          }

          const config = {
            workspace_id: workspaceId,
            project_id: projectData.id,
            project_name: input.name,
          };
          fs.writeFileSync(configPath, JSON.stringify(config, null, 2));

          // Generate editor rules if requested
          if (input.generate_editor_rules) {
            const ruleResults = await writeEditorRules({
              folderPath: input.folder_path,
              editors: getAvailableEditors(),
              workspaceId: workspaceId,
              projectName: input.name,
              overwriteExisting: input.overwrite_existing,
            });
            rulesGenerated = ruleResults
              .filter(
                (r) => r.status === "created" || r.status === "updated" || r.status === "appended"
              )
              .map((r) => (r.status === "created" ? r.filename : `${r.filename} (${r.status})`));
            rulesSkipped = ruleResults
              .filter((r) => r.status.startsWith("skipped"))
              .map((r) => r.filename);
          }
        } catch (err: unknown) {
          // Log but don't fail - project was created successfully
          console.error("[ContextStream] Failed to write project config:", err);
        }
      }

      const response = {
        ...(result && typeof result === "object" ? result : {}),
        folder_path: input.folder_path,
        config_written: input.folder_path ? true : undefined,
        editor_rules_generated: rulesGenerated.length > 0 ? rulesGenerated : undefined,
        editor_rules_skipped: rulesSkipped.length > 0 ? rulesSkipped : undefined,
      };

      return {
        content: [{ type: "text" as const, text: formatContent(response) }],
        structuredContent: toStructured(response),
      };
    }
  );

  registerTool(
    "projects_update",
    {
      title: "Update project",
      description: "Update a project (rename or change description)",
      inputSchema: z.object({
        project_id: z.string().uuid(),
        name: z.string().optional(),
        description: z.string().optional(),
      }),
    },
    async (input) => {
      const { project_id, ...updates } = input;
      const result = await client.updateProject(project_id, updates);
      return {
        content: [{ type: "text" as const, text: formatContent(result) }],
        structuredContent: toStructured(result),
      };
    }
  );

  registerTool(
    "projects_delete",
    {
      title: "Delete project",
      description:
        "Delete a project and all its contents (indexed files, memory events, etc.). This action is irreversible.",
      inputSchema: z.object({
        project_id: z.string().uuid(),
      }),
    },
    async (input) => {
      const result = await client.deleteProject(input.project_id);
      // Normalize response to match {success, data, error, metadata} structure
      const normalized = result || {
        success: true,
        data: { id: input.project_id, deleted: true },
        error: null,
        metadata: {},
      };
      return {
        content: [{ type: "text" as const, text: formatContent(normalized) }],
        structuredContent: toStructured(normalized),
      };
    }
  );

  registerTool(
    "projects_index",
    {
      title: "Index project",
      description: "Trigger indexing for a project",
      inputSchema: z.object({ project_id: z.string().uuid().optional() }),
    },
    async (input) => {
      const projectId = resolveProjectId(input.project_id);
      if (!projectId) {
        return errorResult(
          "Error: project_id is required. Please call session_init first or provide project_id explicitly."
        );
      }

      const result = await client.indexProject(projectId);
      return {
        content: [{ type: "text" as const, text: formatContent(result) }],
        structuredContent: toStructured(result),
      };
    }
  );

  // Search
  const searchSchema = z.object({
    query: z.string(),
    workspace_id: z.string().uuid().optional(),
    project_id: z.string().uuid().optional(),
    limit: z.number().optional().describe("Max results to return (default: 3)"),
    offset: z.number().optional().describe("Offset for pagination"),
    content_max_chars: z
      .number()
      .optional()
      .describe("Max chars per result content (default: 400)"),
  });

  function normalizeSearchParams(input: {
    query: string;
    workspace_id?: string;
    project_id?: string;
    limit?: number;
    offset?: number;
    content_max_chars?: number;
    context_lines?: number;
    exact_match_boost?: number;
    output_format?: "full" | "paths" | "minimal" | "count";
  }) {
    const limit =
      typeof input.limit === "number" && input.limit > 0
        ? Math.min(Math.floor(input.limit), 100)
        : DEFAULT_SEARCH_LIMIT;
    const offset =
      typeof input.offset === "number" && input.offset > 0 ? Math.floor(input.offset) : undefined;
    const contentMax =
      typeof input.content_max_chars === "number" && input.content_max_chars > 0
        ? Math.max(50, Math.min(Math.floor(input.content_max_chars), 10000))
        : DEFAULT_SEARCH_CONTENT_MAX_CHARS;
    const contextLines =
      typeof input.context_lines === "number" && input.context_lines >= 0
        ? Math.min(Math.floor(input.context_lines), 10)
        : undefined;
    const exactMatchBoost =
      typeof input.exact_match_boost === "number" && input.exact_match_boost >= 1
        ? Math.min(input.exact_match_boost, 10)
        : undefined;
    return {
      query: input.query,
      workspace_id: resolveWorkspaceId(input.workspace_id),
      project_id: resolveProjectId(input.project_id),
      limit,
      offset,
      content_max_chars: contentMax,
      context_lines: contextLines,
      exact_match_boost: exactMatchBoost,
      output_format: input.output_format,
    };
  }

  registerTool(
    "search_semantic",
    { title: "Semantic search", description: "Semantic vector search", inputSchema: searchSchema },
    async (input) => {
      const result = await client.searchSemantic(normalizeSearchParams(input));
      return {
        content: [{ type: "text" as const, text: formatContent(result) }],
        structuredContent: toStructured(result),
      };
    }
  );

  registerTool(
    "search_hybrid",
    {
      title: "Hybrid search",
      description: "Hybrid search (semantic + keyword)",
      inputSchema: searchSchema,
    },
    async (input) => {
      const result = await client.searchHybrid(normalizeSearchParams(input));
      return {
        content: [{ type: "text" as const, text: formatContent(result) }],
        structuredContent: toStructured(result),
      };
    }
  );

  registerTool(
    "search_keyword",
    { title: "Keyword search", description: "Keyword search", inputSchema: searchSchema },
    async (input) => {
      const result = await client.searchKeyword(normalizeSearchParams(input));
      return {
        content: [{ type: "text" as const, text: formatContent(result) }],
        structuredContent: toStructured(result),
      };
    }
  );

  registerTool(
    "search_pattern",
    { title: "Pattern search", description: "Pattern/regex search", inputSchema: searchSchema },
    async (input) => {
      const result = await client.searchPattern(normalizeSearchParams(input));
      return {
        content: [{ type: "text" as const, text: formatContent(result) }],
        structuredContent: toStructured(result),
      };
    }
  );

  // Memory / Knowledge
  registerTool(
    "memory_create_event",
    {
      title: "Create memory event",
      description: "Create a memory event for a workspace/project",
      inputSchema: z.object({
        workspace_id: z.string().uuid().optional(),
        project_id: z.string().uuid().optional(),
        event_type: z.string(),
        title: z.string(),
        content: z.string(),
        metadata: z.record(z.any()).optional(),
        provenance: z
          .object({
            repo: z.string().optional(),
            branch: z.string().optional(),
            commit_sha: z.string().optional(),
            pr_url: z.string().url().optional(),
            issue_url: z.string().url().optional(),
            slack_thread_url: z.string().url().optional(),
          })
          .optional(),
        code_refs: z
          .array(
            z.object({
              file_path: z.string(),
              symbol_id: z.string().optional(),
              symbol_name: z.string().optional(),
            })
          )
          .optional(),
      }),
    },
    async (input) => {
      const result = await client.createMemoryEvent(input);
      return {
        content: [{ type: "text" as const, text: formatContent(result) }],
        structuredContent: toStructured(result),
      };
    }
  );

  registerTool(
    "memory_bulk_ingest",
    {
      title: "Bulk ingest events",
      description: "Bulk ingest multiple memory events",
      inputSchema: z.object({
        workspace_id: z.string().uuid().optional(),
        project_id: z.string().uuid().optional(),
        events: z.array(z.record(z.any())),
      }),
    },
    async (input) => {
      const result = await client.bulkIngestEvents(input);
      return {
        content: [{ type: "text" as const, text: formatContent(result) }],
        structuredContent: toStructured(result),
      };
    }
  );

  registerTool(
    "memory_list_events",
    {
      title: "List memory events",
      description: "List memory events (optionally scoped)",
      inputSchema: z.object({
        workspace_id: z.string().uuid().optional(),
        project_id: z.string().uuid().optional(),
        limit: z.number().optional(),
      }),
    },
    async (input) => {
      const result = await client.listMemoryEvents(input);
      return {
        content: [{ type: "text" as const, text: formatContent(result) }],
        structuredContent: toStructured(result),
      };
    }
  );

  registerTool(
    "memory_create_node",
    {
      title: "Create knowledge node",
      description: "Create a knowledge node with optional relations",
      inputSchema: z.object({
        workspace_id: z.string().uuid().optional(),
        project_id: z.string().uuid().optional(),
        node_type: z.string(),
        title: z.string(),
        content: z.string(),
        relations: z
          .array(
            z.object({
              type: z.string(),
              target_id: z.string().uuid(),
            })
          )
          .optional(),
      }),
    },
    async (input) => {
      const result = await client.createKnowledgeNode(input);
      return {
        content: [{ type: "text" as const, text: formatContent(result) }],
        structuredContent: toStructured(result),
      };
    }
  );

  registerTool(
    "memory_list_nodes",
    {
      title: "List knowledge nodes",
      description: "List knowledge graph nodes",
      inputSchema: z.object({
        workspace_id: z.string().uuid().optional(),
        project_id: z.string().uuid().optional(),
        limit: z.number().optional(),
      }),
    },
    async (input) => {
      const result = await client.listKnowledgeNodes(input);
      return {
        content: [{ type: "text" as const, text: formatContent(result) }],
        structuredContent: toStructured(result),
      };
    }
  );

  registerTool(
    "memory_search",
    {
      title: "Memory-aware search",
      description: "Search memory events/notes",
      inputSchema: z.object({
        query: z.string(),
        workspace_id: z.string().uuid().optional(),
        project_id: z.string().uuid().optional(),
        limit: z.number().optional(),
      }),
    },
    async (input) => {
      const result = await client.memorySearch(input);
      return {
        content: [{ type: "text" as const, text: formatContent(result) }],
        structuredContent: toStructured(result),
      };
    }
  );

  registerTool(
    "memory_decisions",
    {
      title: "Decision summaries",
      description: "List decision summaries from workspace memory",
      inputSchema: z.object({
        workspace_id: z.string().uuid().optional(),
        project_id: z.string().uuid().optional(),
        category: z
          .string()
          .optional()
          .describe(
            "Optional category filter. If not specified, returns all decisions regardless of category."
          ),
        limit: z.number().optional(),
      }),
    },
    async (input) => {
      const result = await client.memoryDecisions(input);
      return {
        content: [{ type: "text" as const, text: formatContent(result) }],
        structuredContent: toStructured(result),
      };
    }
  );

  registerTool(
    "decision_trace",
    {
      title: "Decision trace",
      description: "Trace decisions to provenance, code references, and impact",
      inputSchema: z.object({
        query: z.string(),
        workspace_id: z.string().uuid().optional(),
        project_id: z.string().uuid().optional(),
        limit: z.number().optional(),
        include_impact: z
          .boolean()
          .optional()
          .describe("Include impact analysis when graph data is available"),
      }),
    },
    async (input) => {
      const result = await client.decisionTrace(input);
      return {
        content: [{ type: "text" as const, text: formatContent(result) }],
        structuredContent: toStructured(result),
      };
    }
  );

  // Graph
  registerTool(
    "graph_related",
    {
      title: "Related knowledge nodes",
      description: "Find related nodes in the knowledge graph",
      inputSchema: z.object({
        node_id: z.string(),
        workspace_id: z.string().uuid().optional(),
        project_id: z.string().uuid().optional(),
        limit: z.number().optional(),
      }),
    },
    async (input) => {
      const gate = await gateIfGraphTool("graph_related", input);
      if (gate) return gate;
      const result = await client.graphRelated(input);
      return {
        content: [{ type: "text" as const, text: formatContent(result) }],
        structuredContent: toStructured(result),
      };
    }
  );

  registerTool(
    "graph_path",
    {
      title: "Knowledge path",
      description: "Find path between two nodes",
      inputSchema: z.object({
        source_id: z.string().uuid(),
        target_id: z.string().uuid(),
        workspace_id: z.string().uuid().optional(),
        project_id: z.string().uuid().optional(),
      }),
    },
    async (input) => {
      const gate = await gateIfGraphTool("graph_path", input);
      if (gate) return gate;
      const result = await client.graphPath(input);
      return {
        content: [{ type: "text" as const, text: formatContent(result) }],
        structuredContent: toStructured(result),
      };
    }
  );

  registerTool(
    "graph_decisions",
    {
      title: "Decision graph",
      description: "Decision history in the knowledge graph",
      inputSchema: z.object({
        workspace_id: z.string().uuid().optional(),
        project_id: z.string().uuid().optional(),
        limit: z.number().optional(),
      }),
    },
    async (input) => {
      const gate = await gateIfGraphTool("graph_decisions", input);
      if (gate) return gate;
      const result = await client.graphDecisions(input);
      return {
        content: [{ type: "text" as const, text: formatContent(result) }],
        structuredContent: toStructured(result),
      };
    }
  );

  registerTool(
    "graph_dependencies",
    {
      title: "Code dependencies",
      description: "Dependency graph query",
      inputSchema: z.object({
        target: z.object({
          type: z
            .string()
            .describe(
              "Code element type. Accepted values: module (aliases: file, path), function (alias: method), type (aliases: struct, enum, trait, class), variable (aliases: data, const, constant). For knowledge/memory nodes, use graph_path with UUID ids instead."
            ),
          id: z
            .string()
            .describe(
              'Element identifier. For module type, use file path (e.g., "src/auth.rs"). For function/type/variable, use the element id.'
            ),
        }),
        max_depth: z.number().optional(),
        include_transitive: z.boolean().optional(),
      }),
    },
    async (input) => {
      const gate = await gateIfGraphTool("graph_dependencies", input);
      if (gate) return gate;
      const result = await client.graphDependencies(input);
      return {
        content: [{ type: "text" as const, text: formatContent(result) }],
        structuredContent: toStructured(result),
      };
    }
  );

  registerTool(
    "graph_call_path",
    {
      title: "Call path",
      description: "Find call path between two targets",
      inputSchema: z.object({
        source: z.object({
          type: z
            .string()
            .describe(
              'Must be "function" (alias: method). Only function types are supported for call path analysis. For knowledge/memory nodes, use graph_path with UUID ids instead.'
            ),
          id: z.string().describe("Source function identifier."),
        }),
        target: z.object({
          type: z
            .string()
            .describe(
              'Must be "function" (alias: method). Only function types are supported for call path analysis.'
            ),
          id: z.string().describe("Target function identifier."),
        }),
        max_depth: z.number().optional(),
      }),
    },
    async (input) => {
      const gate = await gateIfGraphTool("graph_call_path", input);
      if (gate) return gate;
      const result = await client.graphCallPath(input);
      return {
        content: [{ type: "text" as const, text: formatContent(result) }],
        structuredContent: toStructured(result),
      };
    }
  );

  registerTool(
    "graph_impact",
    {
      title: "Impact analysis",
      description: "Analyze impact of a target node",
      inputSchema: z.object({
        target: z.object({
          type: z
            .string()
            .describe(
              "Code element type. Accepted values: module (aliases: file, path), function (alias: method), type (aliases: struct, enum, trait, class), variable (aliases: data, const, constant). For knowledge/memory nodes, use graph_path with UUID ids instead."
            ),
          id: z
            .string()
            .describe(
              'Element identifier. For module type, use file path (e.g., "src/auth.rs"). For function/type/variable, use the element id.'
            ),
        }),
        max_depth: z.number().optional(),
      }),
    },
    async (input) => {
      const gate = await gateIfGraphTool("graph_impact", input);
      if (gate) return gate;
      const result = await client.graphImpact(input);
      return {
        content: [{ type: "text" as const, text: formatContent(result) }],
        structuredContent: toStructured(result),
      };
    }
  );

  registerTool(
    "graph_ingest",
    {
      title: "Ingest code graph",
      description:
        "Build and persist the dependency graph for a project. Runs async by default (wait=false) and can take a few minutes for larger repos.",
      inputSchema: z.object({
        project_id: z.string().uuid().optional(),
        wait: z
          .boolean()
          .optional()
          .describe(
            "If true, wait for ingestion to finish before returning. Defaults to false (async)."
          ),
      }),
    },
    async (input) => {
      const gate = await gateIfGraphTool("graph_ingest", input);
      if (gate) return gate;
      const projectId = resolveProjectId(input.project_id);
      if (!projectId) {
        return errorResult(
          "Error: project_id is required. Please call session_init first or provide project_id explicitly."
        );
      }

      const wait = input.wait ?? false;
      let estimate: { min: number; max: number; basis?: string } | null = null;

      try {
        const stats = await client.projectStatistics(projectId);
        estimate = estimateGraphIngestMinutes(stats);
      } catch (error) {
        console.error(
          "[ContextStream] Failed to fetch project statistics for graph ingest estimate:",
          error
        );
      }

      const result = await client.graphIngest({ project_id: projectId, wait });
      const estimateText = estimate
        ? `Estimated time: ${estimate.min}-${estimate.max} min${estimate.basis ? ` (based on ${estimate.basis})` : ""}.`
        : "Estimated time varies with repo size.";
      const note = `Graph ingestion is running ${wait ? "synchronously" : "asynchronously"} and can take a few minutes. ${estimateText}`;
      const structured = toStructured(result);
      const structuredContent =
        structured && typeof structured === "object"
          ? {
              ...structured,
              wait,
              note,
              ...(estimate
                ? {
                    estimate_minutes: { min: estimate.min, max: estimate.max },
                    estimate_basis: estimate.basis,
                  }
                : {}),
            }
          : {
              wait,
              note,
              ...(estimate
                ? {
                    estimate_minutes: { min: estimate.min, max: estimate.max },
                    estimate_basis: estimate.basis,
                  }
                : {}),
            };

      return {
        content: [
          {
            type: "text" as const,
            text: `${note}\n${formatContent(result)}`,
          },
        ],
        structuredContent,
      };
    }
  );

  // AI
  registerTool(
    "ai_context",
    {
      title: "Build AI context",
      description: "Build LLM context (docs/memory/code) for a query",
      inputSchema: z.object({
        query: z.string(),
        workspace_id: z.string().uuid().optional(),
        project_id: z.string().uuid().optional(),
        include_code: z.boolean().optional(),
        include_docs: z.boolean().optional(),
        include_memory: z.boolean().optional(),
        limit: z.number().optional(),
      }),
    },
    async (input) => {
      const result = await client.aiContext(input);
      return {
        content: [{ type: "text" as const, text: formatContent(result) }],
        structuredContent: toStructured(result),
      };
    }
  );

  registerTool(
    "ai_embeddings",
    {
      title: "Generate embeddings",
      description: "Generate embeddings for a text",
      inputSchema: z.object({ text: z.string() }),
    },
    async (input) => {
      const result = await client.aiEmbeddings(input);
      return {
        content: [{ type: "text" as const, text: formatContent(result) }],
        structuredContent: toStructured(result),
      };
    }
  );

  registerTool(
    "ai_plan",
    {
      title: "Generate dev plan",
      description: "Generate development plan from description",
      inputSchema: z.object({
        description: z.string(),
        project_id: z.string().uuid().optional(),
        complexity: z.string().optional(),
      }),
    },
    async (input) => {
      const result = await client.aiPlan(input);
      return {
        content: [{ type: "text" as const, text: formatContent(result) }],
        structuredContent: toStructured(result),
      };
    }
  );

  registerTool(
    "ai_tasks",
    {
      title: "Generate tasks",
      description: "Generate tasks from plan or description",
      inputSchema: z.object({
        plan_id: z.string().optional(),
        description: z.string().optional(),
        project_id: z.string().uuid().optional(),
        granularity: z.string().optional(),
      }),
    },
    async (input) => {
      const result = await client.aiTasks(input);
      return {
        content: [{ type: "text" as const, text: formatContent(result) }],
        structuredContent: toStructured(result),
      };
    }
  );

  registerTool(
    "ai_enhanced_context",
    {
      title: "Enhanced AI context",
      description: "Build enhanced LLM context with deeper analysis",
      inputSchema: z.object({
        query: z.string(),
        workspace_id: z.string().uuid().optional(),
        project_id: z.string().uuid().optional(),
        include_code: z.boolean().optional(),
        include_docs: z.boolean().optional(),
        include_memory: z.boolean().optional(),
        limit: z.number().optional(),
      }),
    },
    async (input) => {
      const result = await client.aiEnhancedContext(input);
      return {
        content: [{ type: "text" as const, text: formatContent(result) }],
        structuredContent: toStructured(result),
      };
    }
  );

  // Extended project operations
  registerTool(
    "projects_get",
    {
      title: "Get project",
      description: "Get project details by ID",
      inputSchema: z.object({ project_id: z.string().uuid().optional() }),
    },
    async (input) => {
      const projectId = resolveProjectId(input.project_id);
      if (!projectId) {
        return errorResult(
          "Error: project_id is required. Please call session_init first or provide project_id explicitly."
        );
      }

      const result = await client.getProject(projectId);
      return {
        content: [{ type: "text" as const, text: formatContent(result) }],
        structuredContent: toStructured(result),
      };
    }
  );

  registerTool(
    "projects_overview",
    {
      title: "Project overview",
      description: "Get project overview with summary information",
      inputSchema: z.object({ project_id: z.string().uuid().optional() }),
    },
    async (input) => {
      const projectId = resolveProjectId(input.project_id);
      if (!projectId) {
        return errorResult(
          "Error: project_id is required. Please call session_init first or provide project_id explicitly."
        );
      }

      const result = await client.projectOverview(projectId);
      return {
        content: [{ type: "text" as const, text: formatContent(result) }],
        structuredContent: toStructured(result),
      };
    }
  );

  registerTool(
    "projects_statistics",
    {
      title: "Project statistics",
      description: "Get project statistics (files, lines, complexity)",
      inputSchema: z.object({ project_id: z.string().uuid().optional() }),
    },
    async (input) => {
      const projectId = resolveProjectId(input.project_id);
      if (!projectId) {
        return errorResult(
          "Error: project_id is required. Please call session_init first or provide project_id explicitly."
        );
      }

      const result = await client.projectStatistics(projectId);
      return {
        content: [{ type: "text" as const, text: formatContent(result) }],
        structuredContent: toStructured(result),
      };
    }
  );

  registerTool(
    "projects_files",
    {
      title: "List project files",
      description: "List all indexed files in a project",
      inputSchema: z.object({ project_id: z.string().uuid().optional() }),
    },
    async (input) => {
      const projectId = resolveProjectId(input.project_id);
      if (!projectId) {
        return errorResult(
          "Error: project_id is required. Please call session_init first or provide project_id explicitly."
        );
      }

      const result = await client.projectFiles(projectId);
      return {
        content: [{ type: "text" as const, text: formatContent(result) }],
        structuredContent: toStructured(result),
      };
    }
  );

  registerTool(
    "projects_index_status",
    {
      title: "Index status",
      description: "Get project indexing status",
      inputSchema: z.object({ project_id: z.string().uuid().optional() }),
    },
    async (input) => {
      const projectId = resolveProjectId(input.project_id);
      if (!projectId) {
        return errorResult(
          "Error: project_id is required. Please call session_init first or provide project_id explicitly."
        );
      }

      const result = await client.projectIndexStatus(projectId);
      return {
        content: [{ type: "text" as const, text: formatContent(result) }],
        structuredContent: toStructured(result),
      };
    }
  );

  registerTool(
    "projects_ingest_local",
    {
      title: "Ingest local files",
      description: `Read ALL files from a local directory and ingest them for indexing.
This indexes your entire project by reading files in batches.
Automatically detects code files and skips ignored directories like node_modules, target, dist, etc.
Runs in the background and returns immediately; use 'projects_index_status' to monitor progress.`,
      inputSchema: z.object({
        project_id: z
          .string()
          .uuid()
          .optional()
          .describe("Project to ingest files into (defaults to current session project)"),
        path: z.string().describe("Local directory path to read files from"),
        write_to_disk: z
          .boolean()
          .optional()
          .describe(
            "When true, write files to disk under QA_FILE_WRITE_ROOT before indexing (for testing/QA)"
          ),
        overwrite: z
          .boolean()
          .optional()
          .describe("Allow overwriting existing files when write_to_disk is enabled"),
      }),
    },
    async (input) => {
      const projectId = resolveProjectId(input.project_id);
      if (!projectId) {
        return errorResult(
          "Error: project_id is required. Please call session_init first or provide project_id explicitly."
        );
      }

      const pathCheck = await validateReadableDirectory(input.path);
      if (!pathCheck.ok) {
        return errorResult(pathCheck.error);
      }

      // Capture ingest options for passing to API
      const ingestOptions = {
        ...(input.write_to_disk !== undefined && { write_to_disk: input.write_to_disk }),
        ...(input.overwrite !== undefined && { overwrite: input.overwrite }),
      };

      startBackgroundIngest(projectId, pathCheck.resolvedPath, ingestOptions, { preflight: true });

      const summary = {
        status: "started",
        message: "Ingestion running in background",
        project_id: projectId,
        path: input.path,
        ...(input.write_to_disk && { write_to_disk: input.write_to_disk }),
        ...(input.overwrite && { overwrite: input.overwrite }),
        note: "Use 'projects_index_status' to monitor progress.",
      };

      return {
        content: [
          {
            type: "text" as const,
            text: `Ingestion started in background for directory: ${input.path}. Use 'projects_index_status' to monitor progress.`,
          },
        ],
        structuredContent: toStructured(summary),
      };
    }
  );

  // Extended workspace operations
  registerTool(
    "workspaces_get",
    {
      title: "Get workspace",
      description: "Get workspace details by ID",
      inputSchema: z.object({ workspace_id: z.string().uuid().optional() }),
    },
    async (input) => {
      const workspaceId = resolveWorkspaceId(input.workspace_id);
      if (!workspaceId) {
        return errorResult(
          "Error: workspace_id is required. Please call session_init first or provide workspace_id explicitly."
        );
      }

      const result = await client.getWorkspace(workspaceId);
      return {
        content: [{ type: "text" as const, text: formatContent(result) }],
        structuredContent: toStructured(result),
      };
    }
  );

  registerTool(
    "workspaces_overview",
    {
      title: "Workspace overview",
      description: "Get workspace overview with summary information",
      inputSchema: z.object({ workspace_id: z.string().uuid().optional() }),
    },
    async (input) => {
      const workspaceId = resolveWorkspaceId(input.workspace_id);
      if (!workspaceId) {
        return errorResult(
          "Error: workspace_id is required. Please call session_init first or provide workspace_id explicitly."
        );
      }

      const result = await client.workspaceOverview(workspaceId);
      return {
        content: [{ type: "text" as const, text: formatContent(result) }],
        structuredContent: toStructured(result),
      };
    }
  );

  registerTool(
    "workspaces_analytics",
    {
      title: "Workspace analytics",
      description: "Get workspace usage analytics",
      inputSchema: z.object({ workspace_id: z.string().uuid().optional() }),
    },
    async (input) => {
      const workspaceId = resolveWorkspaceId(input.workspace_id);
      if (!workspaceId) {
        return errorResult(
          "Error: workspace_id is required. Please call session_init first or provide workspace_id explicitly."
        );
      }

      const result = await client.workspaceAnalytics(workspaceId);
      return {
        content: [{ type: "text" as const, text: formatContent(result) }],
        structuredContent: toStructured(result),
      };
    }
  );

  registerTool(
    "workspaces_content",
    {
      title: "Workspace content",
      description:
        "List content in a workspace (paginated list: items, total, page, per_page, has_next, has_prev).",
      inputSchema: z.object({ workspace_id: z.string().uuid().optional() }),
    },
    async (input) => {
      const workspaceId = resolveWorkspaceId(input.workspace_id);
      if (!workspaceId) {
        return errorResult(
          "Error: workspace_id is required. Please call session_init first or provide workspace_id explicitly."
        );
      }

      const result = await client.workspaceContent(workspaceId);
      return {
        content: [{ type: "text" as const, text: formatContent(result) }],
        structuredContent: toStructured(result),
      };
    }
  );

  // Extended memory operations
  registerTool(
    "memory_get_event",
    {
      title: "Get memory event",
      description:
        "Get a specific memory event by ID with FULL content (not truncated). Use this when you need the complete content of a memory event, not just the preview returned by search/recall.",
      inputSchema: z.object({
        event_id: z.string().uuid().describe("The UUID of the memory event to retrieve"),
      }),
    },
    async (input) => {
      const result = await client.getMemoryEvent(input.event_id);
      return {
        content: [{ type: "text" as const, text: formatContent(result) }],
        structuredContent: toStructured(result),
      };
    }
  );

  registerTool(
    "memory_update_event",
    {
      title: "Update memory event",
      description: "Update a memory event",
      inputSchema: z.object({
        event_id: z.string().uuid(),
        title: z.string().optional(),
        content: z.string().optional(),
        metadata: z.record(z.any()).optional(),
      }),
    },
    async (input) => {
      const { event_id, ...body } = input;
      const result = await client.updateMemoryEvent(event_id, body);
      return {
        content: [{ type: "text" as const, text: formatContent(result) }],
        structuredContent: toStructured(result),
      };
    }
  );

  registerTool(
    "memory_delete_event",
    {
      title: "Delete memory event",
      description: "Delete a memory event",
      inputSchema: z.object({ event_id: z.string().uuid() }),
    },
    async (input) => {
      const result = await client.deleteMemoryEvent(input.event_id);
      return {
        content: [{ type: "text" as const, text: formatContent(result) }],
        structuredContent: toStructured(result),
      };
    }
  );

  registerTool(
    "memory_distill_event",
    {
      title: "Distill memory event",
      description: "Extract and condense key insights from a memory event",
      inputSchema: z.object({ event_id: z.string().uuid() }),
    },
    async (input) => {
      const result = await client.distillMemoryEvent(input.event_id);
      return {
        content: [{ type: "text" as const, text: formatContent(result) }],
        structuredContent: toStructured(result),
      };
    }
  );

  registerTool(
    "memory_get_node",
    {
      title: "Get knowledge node",
      description: "Get a specific knowledge node by ID",
      inputSchema: z.object({ node_id: z.string().uuid() }),
    },
    async (input) => {
      const result = await client.getKnowledgeNode(input.node_id);
      return {
        content: [{ type: "text" as const, text: formatContent(result) }],
        structuredContent: toStructured(result),
      };
    }
  );

  registerTool(
    "memory_update_node",
    {
      title: "Update knowledge node",
      description: "Update a knowledge node",
      inputSchema: z.object({
        node_id: z.string().uuid(),
        title: z.string().optional(),
        content: z.string().optional(),
        relations: z.array(z.object({ type: z.string(), target_id: z.string().uuid() })).optional(),
      }),
    },
    async (input) => {
      const { node_id, ...body } = input;
      const result = await client.updateKnowledgeNode(node_id, body);
      return {
        content: [{ type: "text" as const, text: formatContent(result) }],
        structuredContent: toStructured(result),
      };
    }
  );

  registerTool(
    "memory_delete_node",
    {
      title: "Delete knowledge node",
      description: "Delete a knowledge node",
      inputSchema: z.object({ node_id: z.string().uuid() }),
    },
    async (input) => {
      const result = await client.deleteKnowledgeNode(input.node_id);
      return {
        content: [{ type: "text" as const, text: formatContent(result) }],
        structuredContent: toStructured(result),
      };
    }
  );

  registerTool(
    "memory_supersede_node",
    {
      title: "Supersede knowledge node",
      description: "Replace a knowledge node with updated information (maintains history)",
      inputSchema: z.object({
        node_id: z.string().uuid(),
        new_content: z.string(),
        reason: z.string().optional(),
      }),
    },
    async (input) => {
      const { node_id, ...body } = input;
      const result = await client.supersedeKnowledgeNode(node_id, body);
      return {
        content: [{ type: "text" as const, text: formatContent(result) }],
        structuredContent: toStructured(result),
      };
    }
  );

  registerTool(
    "memory_timeline",
    {
      title: "Memory timeline",
      description: "Get chronological timeline of memory events for a workspace",
      inputSchema: z.object({ workspace_id: z.string().uuid().optional() }),
    },
    async (input) => {
      const workspaceId = resolveWorkspaceId(input.workspace_id);
      if (!workspaceId) {
        return errorResult(
          "Error: workspace_id is required. Please call session_init first or provide workspace_id explicitly."
        );
      }

      const result = await client.memoryTimeline(workspaceId);
      return {
        content: [{ type: "text" as const, text: formatContent(result) }],
        structuredContent: toStructured(result),
      };
    }
  );

  registerTool(
    "memory_summary",
    {
      title: "Memory summary",
      description: "Get condensed summary of workspace memory",
      inputSchema: z.object({ workspace_id: z.string().uuid().optional() }),
    },
    async (input) => {
      const workspaceId = resolveWorkspaceId(input.workspace_id);
      if (!workspaceId) {
        return errorResult(
          "Error: workspace_id is required. Please call session_init first or provide workspace_id explicitly."
        );
      }

      const result = await client.memorySummary(workspaceId);
      return {
        content: [{ type: "text" as const, text: formatContent(result) }],
        structuredContent: toStructured(result),
      };
    }
  );

  // Extended graph operations
  registerTool(
    "graph_circular_dependencies",
    {
      title: "Find circular dependencies",
      description: "Detect circular dependencies in project code",
      inputSchema: z.object({ project_id: z.string().uuid().optional() }),
    },
    async (input) => {
      const gate = await gateIfGraphTool("graph_circular_dependencies", input);
      if (gate) return gate;
      const projectId = resolveProjectId(input.project_id);
      if (!projectId) {
        return errorResult(
          "Error: project_id is required. Please call session_init first or provide project_id explicitly."
        );
      }

      const result = await client.findCircularDependencies(projectId);
      return {
        content: [{ type: "text" as const, text: formatContent(result) }],
        structuredContent: toStructured(result),
      };
    }
  );

  registerTool(
    "graph_unused_code",
    {
      title: "Find unused code",
      description: "Detect unused code in project",
      inputSchema: z.object({ project_id: z.string().uuid().optional() }),
    },
    async (input) => {
      const gate = await gateIfGraphTool("graph_unused_code", input);
      if (gate) return gate;
      const projectId = resolveProjectId(input.project_id);
      if (!projectId) {
        return errorResult(
          "Error: project_id is required. Please call session_init first or provide project_id explicitly."
        );
      }

      const result = await client.findUnusedCode(projectId);
      return {
        content: [{ type: "text" as const, text: formatContent(result) }],
        structuredContent: toStructured(result),
      };
    }
  );

  registerTool(
    "graph_contradictions",
    {
      title: "Find contradictions",
      description: "Find contradicting information related to a knowledge node",
      inputSchema: z.object({ node_id: z.string().uuid() }),
    },
    async (input) => {
      const gate = await gateIfGraphTool("graph_contradictions", input);
      if (gate) return gate;
      const result = await client.findContradictions(input.node_id);
      return {
        content: [{ type: "text" as const, text: formatContent(result) }],
        structuredContent: toStructured(result),
      };
    }
  );

  // Search suggestions
  registerTool(
    "search_suggestions",
    {
      title: "Search suggestions",
      description: "Get search suggestions based on partial query",
      inputSchema: z.object({
        query: z.string(),
        workspace_id: z.string().uuid().optional(),
        project_id: z.string().uuid().optional(),
      }),
    },
    async (input) => {
      const result = await client.searchSuggestions(input);
      return {
        content: [{ type: "text" as const, text: formatContent(result) }],
        structuredContent: toStructured(result),
      };
    }
  );

  // ============================================
  // Session & Auto-Context Tools (CORE-like)
  // ============================================

  registerTool(
    "session_init",
    {
      title: "Initialize conversation session",
      description: `Initialize a new conversation session and automatically retrieve relevant context.
This is the FIRST tool AI assistants should call when starting a conversation.
Returns: workspace info, project info, recent memory, recent decisions, relevant context, high-priority lessons, and ingest_recommendation.

The ingest_recommendation field indicates if the project needs indexing for code search:
- If [INGEST_RECOMMENDED] appears, ask the user if they want to enable semantic code search
- Benefits: AI-powered code understanding, dependency analysis, better context retrieval
- If user agrees, run: project(action="ingest_local", path="<project_path>")

IMPORTANT: Pass the user's FIRST MESSAGE as context_hint to get semantically relevant context!
Example: session_init(folder_path="/path/to/project", context_hint="how do I implement auth?")

This does semantic search on the first message. You only need context_smart on subsequent messages.`,
      inputSchema: z.object({
        folder_path: z
          .string()
          .optional()
          .describe(
            "Current workspace/project folder path (absolute). Use this when IDE roots are not available."
          ),
        workspace_id: z.string().uuid().optional().describe("Workspace to initialize context for"),
        project_id: z.string().uuid().optional().describe("Project to initialize context for"),
        session_id: z
          .string()
          .optional()
          .describe("Custom session ID (auto-generated if not provided)"),
        context_hint: z
          .string()
          .optional()
          .describe(
            "RECOMMENDED: Pass the user's first message here for semantic search. This finds relevant context from ANY time, not just recent items."
          ),
        include_recent_memory: z
          .boolean()
          .optional()
          .describe("Include recent memory events (default: true)"),
        include_decisions: z
          .boolean()
          .optional()
          .describe("Include recent decisions (default: true)"),
        auto_index: z
          .boolean()
          .optional()
          .describe("Automatically create and index project from IDE workspace (default: true)"),
        allow_no_workspace: z
          .boolean()
          .optional()
          .describe(
            "If true, allow session_init to return connected even if no workspace is resolved (workspace-level tools may not work)."
          ),
      }),
    },
    async (input) => {
      // Get IDE workspace roots if available
      let ideRoots: string[] = [];
      try {
        const rootsResponse = await server.server.listRoots();
        if (rootsResponse?.roots) {
          ideRoots = rootsResponse.roots.map((r: { uri: string; name?: string }) =>
            r.uri.replace("file://", "")
          );
        }
      } catch {
        // IDE may not support roots - that's okay
      }

      // Fallback to explicit folder_path if IDE roots not available
      if (ideRoots.length === 0 && input.folder_path) {
        ideRoots = [input.folder_path];
      }

      const result = (await client.initSession(input, ideRoots)) as Record<string, unknown>;

      // Add compact tool reference to help AI know available tools
      result.tools_hint = getCoreToolsHint();

      // Mark session as initialized to prevent auto-init on subsequent tool calls
      if (sessionManager) {
        sessionManager.markInitialized(result);
      }

      const folderPathForRules =
        input.folder_path || ideRoots[0] || resolveFolderPath(undefined, sessionManager);
      if (sessionManager && folderPathForRules) {
        sessionManager.setFolderPath(folderPathForRules);
      }

      let rulesNotice: RulesNotice | null = null;
      if (folderPathForRules || detectedClientInfo?.name) {
        rulesNotice = getRulesNotice(folderPathForRules, detectedClientInfo?.name);
        if (rulesNotice) {
          (result as any).rules_notice = rulesNotice;
        }
      }

      let versionNotice: Awaited<ReturnType<typeof getUpdateNotice>> | null = null;
      try {
        versionNotice = await getUpdateNotice();
      } catch {
        // ignore version check failures
      }
      if (versionNotice) {
        (result as any).version_notice = versionNotice;
      }

      // Check integration status and update tracking (Strategy 2)
      // This enables dynamic tool list updates for connected integrations
      const workspaceId =
        typeof result.workspace_id === "string" ? (result.workspace_id as string) : undefined;
      if (workspaceId && AUTO_HIDE_INTEGRATIONS) {
        try {
          const intStatus = await checkIntegrationStatus(workspaceId);
          updateIntegrationStatus(intStatus, workspaceId);

          // Add integration info to result for visibility
          (result as any).integrations = {
            slack_connected: intStatus.slack,
            github_connected: intStatus.github,
            auto_hide_enabled: true,
            hint:
              intStatus.slack || intStatus.github
                ? "Integration tools are now available in the tool list."
                : "Connect integrations at https://contextstream.io/settings/integrations to enable Slack/GitHub tools.",
          };
        } catch (error) {
          console.error(
            "[ContextStream] Failed to check integration status in session_init:",
            error
          );
        }
      }

      // Add mode/status block for AI visibility (v0.4.x)
      (result as any).modes = {
        consolidated: CONSOLIDATED_MODE,
        progressive: PROGRESSIVE_MODE,
        router: ROUTER_MODE,
        auto_hide_integrations: AUTO_HIDE_INTEGRATIONS,
        bundles: PROGRESSIVE_MODE ? getBundleInfo() : undefined,
      };

      const status = typeof result.status === "string" ? (result.status as string) : "";
      const workspaceWarning =
        typeof (result as any).workspace_warning === "string"
          ? ((result as any).workspace_warning as string)
          : "";

      let text = formatContent(result);

      if (status === "requires_workspace_name") {
        const folderPath =
          typeof (result as any).folder_path === "string"
            ? ((result as any).folder_path as string)
            : typeof input.folder_path === "string"
              ? input.folder_path
              : "";

        text = [
          "Action required: no workspaces found for this account.",
          "Ask the user for a name for the new workspace (recommended), then run `workspace_bootstrap`.",
          folderPath
            ? `Recommended: workspace_bootstrap(workspace_name: \"<name>\", folder_path: \"${folderPath}\")`
            : 'Recommended: workspace_bootstrap(workspace_name: \"<name>\", folder_path: \"<your repo folder>\")',
          "",
          "If you want to continue without a workspace for now, re-run:",
          folderPath
            ? `  session_init(folder_path: \"${folderPath}\", allow_no_workspace: true)`
            : '  session_init(folder_path: \"<your repo folder>\", allow_no_workspace: true)',
          "",
          "--- Raw Response ---",
          "",
          formatContent(result),
        ].join("\n");
      } else if (status === "requires_workspace_selection") {
        const folderName =
          typeof (result as any).folder_name === "string"
            ? ((result as any).folder_name as string)
            : typeof input.folder_path === "string"
              ? path.basename(input.folder_path) || "this folder"
              : "this folder";

        const candidates = Array.isArray((result as any).workspace_candidates)
          ? ((result as any).workspace_candidates as Array<{
              id?: string;
              name?: string;
              description?: string;
            }>)
          : [];

        const lines: string[] = [];
        lines.push(
          `Action required: select a workspace for "${folderName}" (or create a new one).`
        );
        if (candidates.length > 0) {
          lines.push("");
          lines.push("Available workspaces:");
          candidates.slice(0, 25).forEach((w, i) => {
            const name = w.name || "Untitled";
            const id = w.id ? ` (${w.id})` : "";
            const desc = w.description ? ` - ${w.description}` : "";
            lines.push(`  ${i + 1}. ${name}${id}${desc}`);
          });
        }
        lines.push("");
        lines.push(
          "Then run `workspace_associate` with the selected workspace_id and your folder_path."
        );
        lines.push("");
        lines.push("If you want to continue without a workspace for now, re-run:");
        if (typeof input.folder_path === "string" && input.folder_path) {
          lines.push(
            `  session_init(folder_path: \"${input.folder_path}\", allow_no_workspace: true)`
          );
        } else {
          lines.push(
            '  session_init(folder_path: \"<your repo folder>\", allow_no_workspace: true)'
          );
        }
        lines.push("");
        lines.push("--- Raw Response ---");
        lines.push("");
        lines.push(formatContent(result));
        text = lines.join("\n");
      } else if (workspaceWarning) {
        text = [`Warning: ${workspaceWarning}`, "", formatContent(result)].join("\n");
      }

      const noticeLines: string[] = [];

      // Aggressive rules update warning
      const rulesWarning = generateRulesUpdateWarning(rulesNotice);
      if (rulesWarning) {
        noticeLines.push(rulesWarning);
      }

      // Aggressive version update warning
      const versionWarning = generateVersionUpdateWarning(versionNotice);
      if (versionWarning) {
        noticeLines.push(versionWarning);
      }

      // Add ingest recommendation notice if applicable
      const ingestRec = result.ingest_recommendation as
        | {
            recommended?: boolean;
            status?: string;
            reason?: string;
            benefits?: string[];
            command?: string;
          }
        | undefined;

      if (ingestRec?.recommended) {
        const benefitsList =
          ingestRec.benefits
            ?.slice(0, 3)
            .map((b) => `  • ${b}`)
            .join("\n") || "";
        noticeLines.push(
          `[INGEST_RECOMMENDED] status=${ingestRec.status}`,
          `Reason: ${ingestRec.reason}`,
          ingestRec.benefits ? `Benefits:\n${benefitsList}` : "",
          `Action: Ask the user if they want to enable code search by running:`,
          `  ${ingestRec.command || 'project(action="ingest_local", path="<project_path>")'}`
        );
      } else if (ingestRec?.status === "auto_started") {
        noticeLines.push(
          `[INGEST_STATUS] Background indexing started. Codebase will be searchable shortly.`
        );
      } else if (folderPathForRules && !ingestRec?.recommended) {
        // Project is already indexed - mark it so hooks know to enforce ContextStream-first
        const projectId = typeof result.project_id === "string" ? result.project_id : undefined;
        const projectName = typeof result.project_name === "string" ? result.project_name : undefined;
        markProjectIndexed(folderPathForRules, { project_id: projectId, project_name: projectName }).catch(
          (err) => console.error("[ContextStream] Failed to mark project as indexed:", err)
        );
      }

      if (noticeLines.length > 0) {
        text = `${text}\n\n${noticeLines.filter(Boolean).join("\n")}`;
      }

      // Inject lessons reminder if there are lessons from past mistakes
      const lessonsReminder = generateLessonsReminder(result);
      if (lessonsReminder) {
        text = `${text}${lessonsReminder}`;
      }

      // Inject search rules reminder to combat instruction decay
      if (SEARCH_RULES_REMINDER_ENABLED) {
        text = `${text}\n\n${SEARCH_RULES_REMINDER}`;
      }

      return {
        content: [{ type: "text" as const, text }],
        structuredContent: toStructured(result),
      };
    }
  );

  registerTool(
    "session_tools",
    {
      title: "Get available ContextStream tools",
      description: `Get an ultra-compact list of all available ContextStream MCP tools.
Use this when you need to know what tools are available without reading full descriptions.

Returns a token-efficient tool catalog (~120 tokens) organized by category.

Format options:
- 'grouped' (default): Category: tool(hint) tool(hint) - best for quick reference
- 'minimal': Category:tool|tool|tool - most compact
- 'full': Detailed list with descriptions

Example output (grouped):
Session: init(start-conv) smart(each-msg) capture(save) recall(find) remember(quick)
Search: semantic(meaning) hybrid(combo) keyword(exact)
Memory: events(crud) nodes(knowledge) search(find) decisions(choices)`,
      inputSchema: z.object({
        format: z
          .enum(["grouped", "minimal", "full"])
          .optional()
          .default("grouped")
          .describe(
            "Output format: grouped (default, ~120 tokens), minimal (~80 tokens), or full (~200 tokens)"
          ),
        category: z
          .string()
          .optional()
          .describe(
            "Filter to specific category: Session, Search, Memory, Knowledge, Graph, Workspace, Project, AI"
          ),
      }),
    },
    async (input) => {
      const format = (input.format || "grouped") as CatalogFormat;
      const catalog = generateToolCatalog(format, input.category);

      // Add bundle info when progressive mode is enabled
      let bundleInfo = "";
      if (PROGRESSIVE_MODE) {
        const bundles = getBundleInfo();
        const enabledList = bundles
          .filter((b) => b.enabled)
          .map((b) => b.name)
          .join(", ");
        const availableList = bundles
          .filter((b) => !b.enabled)
          .map((b) => `${b.name}(${b.size})`)
          .join(", ");
        bundleInfo = `\n\n[Progressive Mode]\nEnabled: ${enabledList}\nAvailable: ${availableList}\nUse tools_enable_bundle to unlock more tools.`;
      }

      return {
        content: [{ type: "text" as const, text: catalog + bundleInfo }],
        structuredContent: {
          format,
          catalog,
          progressive_mode: PROGRESSIVE_MODE,
          bundles: PROGRESSIVE_MODE ? getBundleInfo() : undefined,
        },
      };
    }
  );

  registerTool(
    "session_get_user_context",
    {
      title: "Get user context and preferences",
      description: `Retrieve user preferences, coding style, and persona from memory.
Use this to understand how the user likes to work and adapt your responses accordingly.`,
      inputSchema: z.object({
        workspace_id: z
          .string()
          .optional()
          .describe("Workspace ID (UUID). Invalid values are ignored."),
      }),
    },
    async (input) => {
      const workspaceId = resolveWorkspaceId(input.workspace_id);
      const result = await client.getUserContext({ workspace_id: workspaceId });
      return {
        content: [{ type: "text" as const, text: formatContent(result) }],
        structuredContent: toStructured(result),
      };
    }
  );

  registerTool(
    "workspace_associate",
    {
      title: "Associate folder with workspace",
      description: `Associate a folder/repo with a workspace after user selection.
Call this after session_init returns status='requires_workspace_selection' and the user has chosen a workspace.
This persists the selection to .contextstream/config.json so future sessions auto-connect.
Optionally creates a parent folder mapping (e.g., all repos under /dev/company/* map to the same workspace).
Optionally generates AI editor rules for automatic ContextStream usage.`,
      inputSchema: z.object({
        folder_path: z.string().describe("Absolute path to the folder/repo to associate"),
        workspace_id: z.string().uuid().describe("Workspace ID to associate with"),
        workspace_name: z.string().optional().describe("Workspace name for reference"),
        create_parent_mapping: z
          .boolean()
          .optional()
          .describe("Also create a parent folder mapping (e.g., /dev/maker/* -> workspace)"),
        generate_editor_rules: z
          .boolean()
          .optional()
          .describe(
            "Generate AI editor rules for Windsurf, Cursor, Cline, Kilo Code, Roo Code, Claude Code, and Aider"
          ),
        overwrite_existing: z
          .boolean()
          .optional()
          .describe("Allow overwriting existing rule files when generating editor rules"),
      }),
    },
    async (input) => {
      const result = await client.associateWorkspace(input);

      // Optionally generate editor rules
      let rulesGenerated: string[] = [];
      let rulesSkipped: string[] = [];
      if (input.generate_editor_rules) {
        const ruleResults = await writeEditorRules({
          folderPath: input.folder_path,
          editors: getAvailableEditors(),
          workspaceName: input.workspace_name,
          workspaceId: input.workspace_id,
          overwriteExisting: input.overwrite_existing,
        });
        rulesGenerated = ruleResults
          .filter(
            (r) => r.status === "created" || r.status === "updated" || r.status === "appended"
          )
          .map((r) => (r.status === "created" ? r.filename : `${r.filename} (${r.status})`));
        rulesSkipped = ruleResults
          .filter((r) => r.status.startsWith("skipped"))
          .map((r) => r.filename);
      }

      const response = {
        ...result,
        editor_rules_generated: rulesGenerated.length > 0 ? rulesGenerated : undefined,
        editor_rules_skipped: rulesSkipped.length > 0 ? rulesSkipped : undefined,
      };

      return {
        content: [{ type: "text" as const, text: formatContent(response) }],
        structuredContent: toStructured(response),
      };
    }
  );

  registerTool(
    "workspace_bootstrap",
    {
      title: "Create workspace + project from folder",
      description: `Create a new workspace (user-provided name) and onboard the current folder as a project.
This is useful when session_init returns status='requires_workspace_name' (no workspaces exist yet) or when you want to create a new workspace for a repo.

Behavior:
- Creates a workspace with the given name
- Associates the folder to that workspace (writes .contextstream/config.json)
- Initializes a session for the folder, which creates the project (folder name) and starts indexing (if enabled)`,
      inputSchema: z.object({
        workspace_name: z.string().min(1).describe("Name for the new workspace (ask the user)"),
        folder_path: z
          .string()
          .optional()
          .describe("Absolute folder path (defaults to IDE root/cwd)"),
        description: z.string().optional().describe("Optional workspace description"),
        visibility: z
          .enum(["private", "public"])
          .optional()
          .describe("Workspace visibility (default: private)"),
        create_parent_mapping: z
          .boolean()
          .optional()
          .describe("Also create a parent folder mapping (e.g., /dev/company/* -> workspace)"),
        generate_editor_rules: z
          .boolean()
          .optional()
          .describe("Generate AI editor rules in the folder for automatic ContextStream usage"),
        overwrite_existing: z
          .boolean()
          .optional()
          .describe("Allow overwriting existing rule files when generating editor rules"),
        context_hint: z
          .string()
          .optional()
          .describe("Optional context hint for session initialization"),
        auto_index: z
          .boolean()
          .optional()
          .describe("Automatically create and index project from folder (default: true)"),
      }),
    },
    async (input) => {
      // Resolve folder path (prefer explicit; fallback to IDE roots; then cwd)
      let folderPath = input.folder_path;
      if (!folderPath) {
        try {
          const rootsResponse = await server.server.listRoots();
          if (rootsResponse?.roots && rootsResponse.roots.length > 0) {
            folderPath = rootsResponse.roots[0].uri.replace("file://", "");
          }
        } catch {
          // IDE may not support roots - that's okay
        }
      }

      if (!folderPath) {
        folderPath = process.cwd();
      }

      if (!folderPath) {
        return errorResult(
          "Error: folder_path is required. Provide folder_path or run from a project directory."
        );
      }

      const folderName = path.basename(folderPath) || "My Project";

      let newWorkspace: { id?: string; name?: string };
      try {
        newWorkspace = (await client.createWorkspace({
          name: input.workspace_name,
          description: input.description || `Workspace created for ${folderPath}`,
          visibility: input.visibility || "private",
        })) as { id?: string; name?: string };
      } catch (err: any) {
        const message = err?.message || String(err);
        if (typeof message === "string" && message.includes("workspaces_slug_key")) {
          return errorResult(
            [
              "Failed to create workspace: the workspace slug is already taken (or reserved by a deleted workspace).",
              "",
              "Try a slightly different workspace name (e.g., add a suffix) and re-run `workspace_bootstrap`.",
            ].join("\n")
          );
        }
        throw err;
      }

      if (!newWorkspace?.id) {
        return errorResult("Error: failed to create workspace.");
      }

      // Persist folder -> workspace mapping (and optional parent mapping)
      const associateResult = await client.associateWorkspace({
        folder_path: folderPath,
        workspace_id: newWorkspace.id,
        workspace_name: newWorkspace.name || input.workspace_name,
        create_parent_mapping: input.create_parent_mapping,
      });

      // Optionally generate editor rules
      let rulesGenerated: string[] = [];
      let rulesSkipped: string[] = [];
      if (input.generate_editor_rules) {
        const ruleResults = await writeEditorRules({
          folderPath,
          editors: getAvailableEditors(),
          workspaceName: newWorkspace.name || input.workspace_name,
          workspaceId: newWorkspace.id,
          overwriteExisting: input.overwrite_existing,
        });
        rulesGenerated = ruleResults
          .filter(
            (r) => r.status === "created" || r.status === "updated" || r.status === "appended"
          )
          .map((r) => (r.status === "created" ? r.filename : `${r.filename} (${r.status})`));
        rulesSkipped = ruleResults
          .filter((r) => r.status.startsWith("skipped"))
          .map((r) => r.filename);
      }

      // Initialize a session for this folder; this creates the project (folder name) and starts indexing (if enabled)
      const session = (await client.initSession(
        {
          workspace_id: newWorkspace.id,
          context_hint: input.context_hint,
          include_recent_memory: true,
          include_decisions: true,
          auto_index: input.auto_index,
        },
        [folderPath]
      )) as Record<string, unknown>;

      // Mark session as initialized so subsequent tool calls can omit IDs
      if (sessionManager) {
        sessionManager.markInitialized(session);
      }

      const response = {
        ...session,
        bootstrap: {
          folder_path: folderPath,
          project_name: folderName,
          workspace: {
            id: newWorkspace.id,
            name: newWorkspace.name || input.workspace_name,
          },
          association: associateResult,
          editor_rules_generated: rulesGenerated.length > 0 ? rulesGenerated : undefined,
          editor_rules_skipped: rulesSkipped.length > 0 ? rulesSkipped : undefined,
        },
      };

      return {
        content: [{ type: "text" as const, text: formatContent(response) }],
        structuredContent: toStructured(response),
      };
    }
  );

  registerTool(
    "session_capture",
    {
      title: "Capture context to memory",
      description: `Automatically capture and store important context from the conversation.
Use this to persist decisions, insights, preferences, or important information.`,
      inputSchema: z.object({
        workspace_id: z.string().uuid().optional(),
        project_id: z.string().uuid().optional(),
        session_id: z.string().optional().describe("Session ID to associate with this capture"),
        event_type: z
          .enum([
            "conversation",
            "decision",
            "insight",
            "preference",
            "note",
            "implementation",
            "task",
            "bug",
            "feature",
            // Plans & Tasks feature
            "plan", // Implementation plan
            // Lesson system types
            "correction", // User corrected the AI
            "lesson", // Extracted lesson from correction
            "warning", // Proactive reminder
            "frustration", // User expressed frustration
          ])
          .describe("Type of context being captured"),
        title: z.string().describe("Brief title for the captured context"),
        content: z.string().describe("Full content/details to capture"),
        tags: z.array(z.string()).optional().describe("Tags for categorization"),
        importance: z
          .enum(["low", "medium", "high", "critical"])
          .optional()
          .describe("Importance level"),
        provenance: z
          .object({
            repo: z.string().optional(),
            branch: z.string().optional(),
            commit_sha: z.string().optional(),
            pr_url: z.string().url().optional(),
            issue_url: z.string().url().optional(),
            slack_thread_url: z.string().url().optional(),
          })
          .optional(),
        code_refs: z
          .array(
            z.object({
              file_path: z.string(),
              symbol_id: z.string().optional(),
              symbol_name: z.string().optional(),
            })
          )
          .optional(),
      }),
    },
    async (input) => {
      // Get workspace_id and project_id from session context if not provided
      let workspaceId = input.workspace_id;
      let projectId = input.project_id;

      if (!workspaceId && sessionManager) {
        const ctx = sessionManager.getContext();
        if (ctx) {
          workspaceId = ctx.workspace_id as string | undefined;
          projectId = projectId || (ctx.project_id as string | undefined);
        }
      }

      if (!workspaceId) {
        return {
          content: [
            {
              type: "text" as const,
              text: "Error: workspace_id is required. Please call session_init first or provide workspace_id explicitly.",
            },
          ],
          isError: true,
        };
      }

      const result = await client.captureContext({
        ...input,
        workspace_id: workspaceId,
        project_id: projectId,
      });
      return {
        content: [{ type: "text" as const, text: formatContent(result) }],
        structuredContent: toStructured(result),
      };
    }
  );

  // ============================================
  // Lesson System Tools
  // ============================================

  registerTool(
    "session_capture_lesson",
    {
      title: "Capture a lesson learned",
      description: `Capture a lesson learned from a mistake or correction.
Use this when the user corrects you, expresses frustration, or points out an error.
These lessons are surfaced in future sessions to prevent repeating the same mistakes.

Example triggers:
- User says "No, you should..." or "That's wrong"
- User expresses frustration (caps, "COME ON", "WTF")
- Code breaks due to a preventable mistake

The lesson will be tagged with 'lesson' and stored with structured metadata for easy retrieval.`,
      inputSchema: z.object({
        workspace_id: z.string().uuid().optional(),
        project_id: z.string().uuid().optional(),
        title: z
          .string()
          .describe(
            'Lesson title - what to remember (e.g., "Always verify assets in git before pushing")'
          ),
        severity: z
          .enum(["low", "medium", "high", "critical"])
          .default("medium")
          .describe(
            "Severity: critical for production issues, high for breaking changes, medium for workflow, low for minor"
          ),
        category: z
          .enum(["workflow", "code_quality", "verification", "communication", "project_specific"])
          .describe("Category of the lesson"),
        trigger: z
          .string()
          .describe(
            'What action caused the problem (e.g., "Pushed code referencing images without committing them")'
          ),
        impact: z
          .string()
          .describe('What went wrong (e.g., "Production 404 errors - broken landing page")'),
        prevention: z
          .string()
          .describe(
            'How to prevent in future (e.g., "Run git status to check untracked files before pushing")'
          ),
        keywords: z
          .array(z.string())
          .optional()
          .describe(
            'Keywords for matching in future contexts (e.g., ["git", "images", "assets", "push"])'
          ),
      }),
    },
    async (input) => {
      // Get workspace_id from session context if not provided
      let workspaceId = input.workspace_id;
      let projectId = input.project_id;

      if (!workspaceId && sessionManager) {
        const ctx = sessionManager.getContext();
        if (ctx) {
          workspaceId = ctx.workspace_id as string | undefined;
          projectId = projectId || (ctx.project_id as string | undefined);
        }
      }

      if (!workspaceId) {
        return {
          content: [
            {
              type: "text" as const,
              text: "Error: workspace_id is required. Please call session_init first or provide workspace_id explicitly.",
            },
          ],
          isError: true,
        };
      }

      const lessonSignature = buildLessonSignature(
        {
          title: input.title,
          category: input.category,
          trigger: input.trigger,
          impact: input.impact,
          prevention: input.prevention,
        },
        workspaceId,
        projectId
      );

      if (isDuplicateLessonCapture(lessonSignature)) {
        return {
          content: [
            {
              type: "text" as const,
              text: `ℹ️ Duplicate lesson capture ignored: "${input.title}" was already recorded recently.`,
            },
          ],
          structuredContent: {
            deduped: true,
            title: input.title,
          },
        };
      }

      // Build structured content for the lesson
      const lessonContent = [
        `## ${input.title}`,
        "",
        `**Severity:** ${input.severity}`,
        `**Category:** ${input.category}`,
        "",
        "### Trigger",
        input.trigger,
        "",
        "### Impact",
        input.impact,
        "",
        "### Prevention",
        input.prevention,
        input.keywords?.length ? `\n**Keywords:** ${input.keywords.join(", ")}` : "",
      ]
        .filter(Boolean)
        .join("\n");

      const result = await client.captureContext({
        workspace_id: workspaceId,
        project_id: projectId,
        event_type: "lesson",
        title: input.title,
        content: lessonContent,
        importance: input.severity,
        tags: ["lesson", input.category, `severity:${input.severity}`, ...(input.keywords || [])],
      });

      return {
        content: [
          {
            type: "text" as const,
            text: `✅ Lesson captured: "${input.title}"\n\nThis lesson will be surfaced in future sessions when relevant context is detected.`,
          },
        ],
        structuredContent: toStructured(result),
      };
    }
  );

  registerTool(
    "session_get_lessons",
    {
      title: "Get lessons learned",
      description: `Retrieve lessons learned from past mistakes and corrections.
Use this to check for relevant warnings before taking actions that have caused problems before.

Returns lessons filtered by:
- Query: semantic search for relevant lessons
- Category: workflow, code_quality, verification, communication, project_specific
- Severity: low, medium, high, critical`,
      inputSchema: z.object({
        workspace_id: z
          .string()
          .optional()
          .describe("Workspace ID (UUID). Invalid values are ignored."),
        project_id: z
          .string()
          .optional()
          .describe("Project ID (UUID). Invalid values are ignored."),
        query: z
          .string()
          .optional()
          .describe('Search for relevant lessons (e.g., "git push images")'),
        category: z
          .enum(["workflow", "code_quality", "verification", "communication", "project_specific"])
          .optional()
          .describe("Filter by category"),
        severity: z
          .enum(["low", "medium", "high", "critical"])
          .optional()
          .describe("Filter by minimum severity"),
        limit: z.number().default(10).describe("Maximum lessons to return"),
      }),
    },
    async (input) => {
      const workspaceId = resolveWorkspaceId(input.workspace_id);
      const projectId = resolveProjectId(input.project_id);

      // Build search query with lesson-specific terms
      const searchQuery = input.query
        ? `${input.query} lesson prevention warning`
        : "lesson prevention warning mistake";

      const searchResult = (await client.memorySearch({
        query: searchQuery,
        workspace_id: workspaceId,
        project_id: projectId,
        limit: input.limit * 2, // Fetch more to filter
      })) as { results?: any[] };

      // Filter for lessons and apply filters
      const lessons = (searchResult.results || [])
        .filter((item: any) => {
          const tags = item.metadata?.tags || [];
          const isLesson = tags.includes("lesson");
          if (!isLesson) return false;

          // Filter by category if specified
          if (input.category && !tags.includes(input.category)) {
            return false;
          }

          // Filter by severity if specified
          if (input.severity) {
            const severityOrder = ["low", "medium", "high", "critical"];
            const minSeverityIndex = severityOrder.indexOf(input.severity);
            const itemSeverity =
              tags.find((t: string) => t.startsWith("severity:"))?.split(":")[1] || "medium";
            const itemSeverityIndex = severityOrder.indexOf(itemSeverity);
            if (itemSeverityIndex < minSeverityIndex) return false;
          }

          return true;
        })
        .slice(0, input.limit);

      if (lessons.length === 0) {
        return {
          content: [{ type: "text" as const, text: "No lessons found matching your criteria." }],
          structuredContent: toStructured({ lessons: [], count: 0 }),
        };
      }

      // Format lessons for display
      const formattedLessons = lessons
        .map((lesson: any, i: number) => {
          const tags = lesson.metadata?.tags || [];
          const severity =
            tags.find((t: string) => t.startsWith("severity:"))?.split(":")[1] || "medium";
          const category =
            tags.find((t: string) =>
              [
                "workflow",
                "code_quality",
                "verification",
                "communication",
                "project_specific",
              ].includes(t)
            ) || "unknown";

          const severityEmoji =
            (
              {
                low: "🟢",
                medium: "🟡",
                high: "🟠",
                critical: "🔴",
              } as Record<string, string>
            )[severity] || "⚪";

          return `${i + 1}. ${severityEmoji} **${lesson.title}**\n   Category: ${category} | Severity: ${severity}\n   ${lesson.content?.slice(0, 200)}...`;
        })
        .join("\n\n");

      return {
        content: [
          {
            type: "text" as const,
            text: `📚 Found ${lessons.length} lesson(s):\n\n${formattedLessons}`,
          },
        ],
        structuredContent: toStructured({ lessons, count: lessons.length }),
      };
    }
  );

  registerTool(
    "session_smart_search",
    {
      title: "Smart context search",
      description: `Search memory with automatic context enrichment.
Returns memory matches, relevant code, and related decisions in one call.`,
      inputSchema: z.object({
        query: z.string().describe("What to search for"),
        workspace_id: z.string().uuid().optional(),
        project_id: z.string().uuid().optional(),
        include_related: z.boolean().optional().describe("Include related context (default: true)"),
        include_decisions: z
          .boolean()
          .optional()
          .describe("Include related decisions (default: true)"),
      }),
    },
    async (input) => {
      // Get workspace_id from session context if not provided
      let workspaceId = input.workspace_id;
      let projectId = input.project_id;

      if (!workspaceId && sessionManager) {
        const ctx = sessionManager.getContext();
        if (ctx) {
          workspaceId = ctx.workspace_id as string | undefined;
          projectId = projectId || (ctx.project_id as string | undefined);
        }
      }

      const result = await client.smartSearch({
        ...input,
        workspace_id: workspaceId,
        project_id: projectId,
      });
      return {
        content: [{ type: "text" as const, text: formatContent(result) }],
        structuredContent: toStructured(result),
      };
    }
  );

  registerTool(
    "session_remember",
    {
      title: "Remember this",
      description: `Quick way to store something in memory. Use natural language.
Example: "Remember that I prefer TypeScript strict mode" or "Remember we decided to use PostgreSQL"`,
      inputSchema: z.object({
        content: z.string().describe("What to remember (natural language)"),
        workspace_id: z.string().uuid().optional(),
        project_id: z.string().uuid().optional(),
        importance: z.enum(["low", "medium", "high"]).optional(),
        await_indexing: z
          .boolean()
          .optional()
          .describe(
            "If true, wait for indexing to complete before returning. This ensures the content is immediately searchable."
          ),
      }),
    },
    async (input) => {
      // Get workspace_id from session context if not provided
      let workspaceId = input.workspace_id;
      let projectId = input.project_id;

      if (!workspaceId && sessionManager) {
        const ctx = sessionManager.getContext();
        if (ctx) {
          workspaceId = ctx.workspace_id as string | undefined;
          projectId = projectId || (ctx.project_id as string | undefined);
        }
      }

      if (!workspaceId) {
        return {
          content: [
            {
              type: "text" as const,
              text: "Error: workspace_id is required. Please call session_init first.",
            },
          ],
          isError: true,
        };
      }

      const result = await client.sessionRemember({
        content: input.content,
        workspace_id: workspaceId,
        project_id: projectId,
        importance: input.importance,
        await_indexing: input.await_indexing,
      });
      return {
        content: [{ type: "text" as const, text: `Remembered: ${input.content.slice(0, 100)}...` }],
        structuredContent: toStructured(result),
      };
    }
  );

  registerTool(
    "session_recall",
    {
      title: "Recall from memory",
      description: `Quick way to recall relevant context. Use natural language.
Example: "What were the auth decisions?" or "What are my TypeScript preferences?"`,
      inputSchema: z.object({
        query: z.string().describe("What to recall (natural language)"),
        workspace_id: z.string().uuid().optional(),
        project_id: z.string().uuid().optional(),
      }),
    },
    async (input) => {
      // Get workspace_id from session context if not provided
      let workspaceId = input.workspace_id;
      let projectId = input.project_id;

      if (!workspaceId && sessionManager) {
        const ctx = sessionManager.getContext();
        if (ctx) {
          workspaceId = ctx.workspace_id as string | undefined;
          projectId = projectId || (ctx.project_id as string | undefined);
        }
      }

      const result = await client.smartSearch({
        query: input.query,
        workspace_id: workspaceId,
        project_id: projectId,
        include_decisions: true,
      });
      return {
        content: [{ type: "text" as const, text: formatContent(result) }],
        structuredContent: toStructured(result),
      };
    }
  );

  // Editor rules generation
  registerTool(
    "generate_rules",
    {
      title: "Generate ContextStream rules",
      description: `Generate AI rule files for editors (Windsurf, Cursor, Cline, Kilo Code, Roo Code, Claude Code, Aider).
Defaults to the current project folder; no folder_path required when run from a project.
Supported editors: ${getAvailableEditors().join(", ")}`,
      inputSchema: z.object({
        folder_path: z
          .string()
          .optional()
          .describe("Absolute path to the project folder (defaults to IDE root/cwd)"),
        editors: z
          .array(
            z.enum([
              "codex",
              "windsurf",
              "cursor",
              "cline",
              "kilo",
              "roo",
              "claude",
              "aider",
              "all",
            ])
          )
          .optional()
          .describe("Which editors to generate rules for. Defaults to all."),
        workspace_name: z.string().optional().describe("Workspace name to include in rules"),
        workspace_id: z.string().uuid().optional().describe("Workspace ID to include in rules"),
        project_name: z.string().optional().describe("Project name to include in rules"),
        additional_rules: z
          .string()
          .optional()
          .describe("Additional project-specific rules to append"),
        mode: z
          .enum(["minimal", "full"])
          .optional()
          .describe("Rule verbosity mode (default: minimal)"),
        overwrite_existing: z
          .boolean()
          .optional()
          .describe("Allow overwriting existing rule files (ContextStream block only)"),
        apply_global: z
          .boolean()
          .optional()
          .describe("Also write global rule files for supported editors"),
        install_hooks: z
          .boolean()
          .optional()
          .describe("Install Claude Code hooks to enforce ContextStream-first search. Defaults to true for Claude users. Set to false to skip."),
        dry_run: z.boolean().optional().describe("If true, return content without writing files"),
      }),
    },
    async (input) => {
      const folderPath = resolveFolderPath(input.folder_path, sessionManager);
      if (!folderPath) {
        return errorResult(
          "Error: folder_path is required. Provide folder_path or run from a project directory."
        );
      }

      const editors =
        input.editors?.includes("all") || !input.editors
          ? getAvailableEditors()
          : input.editors.filter((e) => e !== "all");

      const results: RuleWriteResult[] = [];

      if (input.dry_run) {
        for (const editor of editors) {
          const rule = generateRuleContent(editor, {
            workspaceName: input.workspace_name,
            workspaceId: input.workspace_id,
            projectName: input.project_name,
            additionalRules: input.additional_rules,
            mode: input.mode,
          });

          if (!rule) {
            results.push({ editor, filename: "", status: "unknown editor" });
            continue;
          }

          results.push({
            editor,
            filename: rule.filename,
            status: "dry run - would update",
          });
        }
      } else {
        const writeResults = await writeEditorRules({
          folderPath,
          editors,
          workspaceName: input.workspace_name,
          workspaceId: input.workspace_id,
          projectName: input.project_name,
          additionalRules: input.additional_rules,
          mode: input.mode,
          overwriteExisting: input.overwrite_existing,
        });
        results.push(...writeResults);
      }

      const globalTargets = listGlobalRuleTargets(editors);
      let globalResults: Array<RuleWriteResult & { scope: "global" }> | undefined;

      if (input.apply_global) {
        if (input.dry_run) {
          globalResults = globalTargets.map((target) => ({
            editor: target.editor,
            filename: target.filePath,
            status: "dry run - would update",
            scope: "global",
          }));
        } else {
          globalResults = await writeGlobalRules({
            editors,
            mode: input.mode,
            overwriteExisting: input.overwrite_existing,
          });
        }
      }

      const createdCount = results.filter(
        (r) => r.status === "created" || r.status === "updated" || r.status === "appended"
      ).length;
      const skippedCount = results.filter((r) => r.status.startsWith("skipped")).length;
      const baseMessage = input.dry_run
        ? "Dry run complete. Use dry_run: false to write files."
        : skippedCount > 0
          ? `Generated ${createdCount} rule files. ${skippedCount} skipped (existing files). Re-run with overwrite_existing: true to replace ContextStream blocks.`
          : `Generated ${createdCount} rule files.`;

      const globalPrompt = input.apply_global
        ? "Global rule update complete."
        : globalTargets.length > 0
          ? "Apply rules globally too? Re-run with apply_global: true."
          : "No global rule locations are known for these editors.";

      // Install Claude Code hooks by default when claude is in editors (unless explicitly disabled)
      let hooksResults: Array<{ file: string; status: string }> | undefined;
      let hooksPrompt: string | undefined;
      const hasClaude = editors.includes("claude");
      const shouldInstallHooks = hasClaude && input.install_hooks !== false;

      if (shouldInstallHooks) {
        try {
          if (input.dry_run) {
            hooksResults = [
              { file: "~/.claude/hooks/contextstream-redirect.py", status: "dry run - would create" },
              { file: "~/.claude/hooks/contextstream-reminder.py", status: "dry run - would create" },
              { file: "~/.claude/settings.json", status: "dry run - would update" },
            ];
          } else {
            const hookResult = await installClaudeCodeHooks({ scope: "user" });
            hooksResults = [
              ...hookResult.scripts.map((f) => ({ file: f, status: "created" })),
              ...hookResult.settings.map((f) => ({ file: f, status: "updated" })),
            ];
          }
        } catch (err) {
          hooksResults = [{ file: "hooks", status: `error: ${(err as Error).message}` }];
        }
      } else if (hasClaude && input.install_hooks === false) {
        hooksPrompt = "Hooks skipped. Claude may use default tools instead of ContextStream search.";
      }

      const summary = {
        folder: folderPath,
        results,
        ...(globalResults ? { global_results: globalResults } : {}),
        ...(globalTargets.length > 0 ? { global_targets: globalTargets } : {}),
        ...(hooksResults ? { hooks_results: hooksResults } : {}),
        message: baseMessage,
        global_prompt: globalPrompt,
        ...(hooksPrompt ? { hooks_prompt: hooksPrompt } : {}),
      };

      return {
        content: [{ type: "text" as const, text: formatContent(summary) }],
        structuredContent: toStructured(summary),
      };
    }
  );

  registerTool(
    "generate_editor_rules",
    {
      title: "Generate editor AI rules",
      description: `Generate AI rule files for editors (Windsurf, Cursor, Cline, Kilo Code, Roo Code, Claude Code, Aider).
These rules instruct the AI to automatically use ContextStream for memory and context.
Supported editors: ${getAvailableEditors().join(", ")}`,
      inputSchema: z.object({
        folder_path: z
          .string()
          .optional()
          .describe("Absolute path to the project folder (defaults to IDE root/cwd)"),
        editors: z
          .array(
            z.enum([
              "codex",
              "windsurf",
              "cursor",
              "cline",
              "kilo",
              "roo",
              "claude",
              "aider",
              "all",
            ])
          )
          .optional()
          .describe("Which editors to generate rules for. Defaults to all."),
        workspace_name: z.string().optional().describe("Workspace name to include in rules"),
        workspace_id: z.string().uuid().optional().describe("Workspace ID to include in rules"),
        project_name: z.string().optional().describe("Project name to include in rules"),
        additional_rules: z
          .string()
          .optional()
          .describe("Additional project-specific rules to append"),
        mode: z
          .enum(["minimal", "full"])
          .optional()
          .describe("Rule verbosity mode (default: minimal)"),
        overwrite_existing: z
          .boolean()
          .optional()
          .describe("Allow overwriting existing rule files (ContextStream block only)"),
        dry_run: z.boolean().optional().describe("If true, return content without writing files"),
      }),
    },
    async (input) => {
      const folderPath = resolveFolderPath(input.folder_path, sessionManager);
      if (!folderPath) {
        return errorResult(
          "Error: folder_path is required. Provide folder_path or run from a project directory."
        );
      }

      const editors =
        input.editors?.includes("all") || !input.editors
          ? getAvailableEditors()
          : input.editors.filter((e) => e !== "all");

      const results: Array<{ editor: string; filename: string; status: string; content?: string }> =
        [];

      if (input.dry_run) {
        for (const editor of editors) {
          const rule = generateRuleContent(editor, {
            workspaceName: input.workspace_name,
            workspaceId: input.workspace_id,
            projectName: input.project_name,
            additionalRules: input.additional_rules,
            mode: input.mode,
          });

          if (!rule) {
            results.push({ editor, filename: "", status: "unknown editor" });
            continue;
          }

          results.push({
            editor,
            filename: rule.filename,
            status: "dry run - would update",
            content: rule.content,
          });
        }
      } else {
        const writeResults = await writeEditorRules({
          folderPath,
          editors,
          workspaceName: input.workspace_name,
          workspaceId: input.workspace_id,
          projectName: input.project_name,
          additionalRules: input.additional_rules,
          mode: input.mode,
          overwriteExisting: input.overwrite_existing,
        });
        results.push(...writeResults);
      }

      const createdCount = results.filter(
        (r) => r.status === "created" || r.status === "updated" || r.status === "appended"
      ).length;
      const skippedCount = results.filter((r) => r.status.startsWith("skipped")).length;
      const summary = {
        folder: folderPath,
        results,
        message: input.dry_run
          ? "Dry run complete. Use dry_run: false to write files."
          : skippedCount > 0
            ? `Generated ${createdCount} rule files. ${skippedCount} skipped (existing files). Re-run with overwrite_existing: true to replace ContextStream blocks.`
            : `Generated ${createdCount} rule files.`,
      };

      return {
        content: [{ type: "text" as const, text: formatContent(summary) }],
        structuredContent: toStructured(summary),
      };
    }
  );

  // ============================================
  // Token-Saving Context Tools
  // ============================================

  registerTool(
    "session_summary",
    {
      title: "Get compact context summary",
      description: `Get a compact, token-efficient summary of workspace context (~500 tokens).
This is designed to replace loading full chat history in AI prompts.
Returns: workspace/project info, top decisions (titles only), preferences, memory count.
Use this at conversation start instead of loading everything.
For specific details, use session_recall or session_smart_search.`,
      inputSchema: z.object({
        workspace_id: z.string().uuid().optional(),
        project_id: z.string().uuid().optional(),
        max_tokens: z.number().optional().describe("Maximum tokens for summary (default: 500)"),
      }),
    },
    async (input) => {
      // Get workspace_id from session context if not provided
      let workspaceId = input.workspace_id;
      let projectId = input.project_id;

      if (!workspaceId && sessionManager) {
        const ctx = sessionManager.getContext();
        if (ctx) {
          workspaceId = ctx.workspace_id as string | undefined;
          projectId = projectId || (ctx.project_id as string | undefined);
        }
      }

      const result = await client.getContextSummary({
        workspace_id: workspaceId,
        project_id: projectId,
        max_tokens: input.max_tokens,
      });

      // Return the summary as plain text for easy inclusion in prompts
      return {
        content: [{ type: "text" as const, text: result.summary }],
        structuredContent: toStructured(result),
      };
    }
  );

  registerTool(
    "session_compress",
    {
      title: "Compress chat history to memory",
      description: `Extract and store key information from chat history as memory events.
This allows clearing chat history while preserving important context.
Use at conversation end or when context window is getting full.

Extracts:
- Decisions made
- User preferences learned
- Insights discovered
- Tasks/action items
- Code patterns established

After compression, the AI can use session_recall to retrieve this context in future conversations.`,
      inputSchema: z.object({
        chat_history: z.string().describe("The chat history to compress and extract from"),
        workspace_id: z.string().uuid().optional(),
        project_id: z.string().uuid().optional(),
        extract_types: z
          .array(z.enum(["decisions", "preferences", "insights", "tasks", "code_patterns"]))
          .optional()
          .describe("Types of information to extract (default: all)"),
      }),
    },
    async (input) => {
      // Get workspace_id from session context if not provided
      let workspaceId = input.workspace_id;
      let projectId = input.project_id;

      if (!workspaceId && sessionManager) {
        const ctx = sessionManager.getContext();
        if (ctx) {
          workspaceId = ctx.workspace_id as string | undefined;
          projectId = projectId || (ctx.project_id as string | undefined);
        }
      }

      if (!workspaceId) {
        return {
          content: [
            {
              type: "text" as const,
              text: "Error: workspace_id is required. Please call session_init first or provide workspace_id explicitly.",
            },
          ],
          isError: true,
        };
      }

      const result = await client.compressChat({
        workspace_id: workspaceId,
        project_id: projectId,
        chat_history: input.chat_history,
        extract_types: input.extract_types as
          | Array<"decisions" | "preferences" | "insights" | "tasks" | "code_patterns">
          | undefined,
      });

      const summary = [
        `✅ Compressed chat history into ${result.events_created} memory events:`,
        "",
        `📋 Decisions: ${result.extracted.decisions.length}`,
        `⚙️ Preferences: ${result.extracted.preferences.length}`,
        `💡 Insights: ${result.extracted.insights.length}`,
        `📝 Tasks: ${result.extracted.tasks.length}`,
        `🔧 Code patterns: ${result.extracted.code_patterns.length}`,
        "",
        "These are now stored in ContextStream memory.",
        "Future conversations can access them via session_recall.",
      ].join("\n");

      return {
        content: [{ type: "text" as const, text: summary }],
        structuredContent: toStructured(result),
      };
    }
  );

  registerTool(
    "ai_context_budget",
    {
      title: "Get context within token budget",
      description: `Get the most relevant context that fits within a specified token budget.
This is the key tool for token-efficient AI interactions:

1. AI calls this with a query and token budget
2. Gets optimally selected context (decisions, memory, code)
3. No need to include full chat history in the prompt

The tool prioritizes:
1. Relevant decisions (highest value per token)
2. Query-matched memory events
3. Related code snippets (if requested and budget allows)

Example: ai_context_budget(query="authentication", max_tokens=1000)`,
      inputSchema: z.object({
        query: z.string().describe("What context to retrieve"),
        max_tokens: z.number().describe("Maximum tokens for the context (e.g., 500, 1000, 2000)"),
        workspace_id: z.string().uuid().optional(),
        project_id: z.string().uuid().optional(),
        include_decisions: z
          .boolean()
          .optional()
          .describe("Include relevant decisions (default: true)"),
        include_memory: z
          .boolean()
          .optional()
          .describe("Include memory search results (default: true)"),
        include_code: z
          .boolean()
          .optional()
          .describe("Include code search results (default: false)"),
      }),
    },
    async (input) => {
      // Get workspace_id from session context if not provided
      let workspaceId = input.workspace_id;
      let projectId = input.project_id;

      if (!workspaceId && sessionManager) {
        const ctx = sessionManager.getContext();
        if (ctx) {
          workspaceId = ctx.workspace_id as string | undefined;
          projectId = projectId || (ctx.project_id as string | undefined);
        }
      }

      const result = await client.getContextWithBudget({
        query: input.query,
        workspace_id: workspaceId,
        project_id: projectId,
        max_tokens: input.max_tokens,
        include_decisions: input.include_decisions,
        include_memory: input.include_memory,
        include_code: input.include_code,
      });

      const footer = `\n---\n📊 Token estimate: ${result.token_estimate}/${input.max_tokens} | Sources: ${result.sources.length}`;

      return {
        content: [{ type: "text" as const, text: result.context + footer }],
        structuredContent: toStructured(result),
      };
    }
  );

  registerTool(
    "session_delta",
    {
      title: "Get context changes since timestamp",
      description: `Get new context added since a specific timestamp.
Useful for efficient context synchronization without reloading everything.

Returns:
- Count of new decisions and memory events
- List of new items with titles and timestamps

Use case: AI can track what's new since last session_init.`,
      inputSchema: z.object({
        since: z
          .string()
          .describe('ISO timestamp to get changes since (e.g., "2025-12-05T00:00:00Z")'),
        workspace_id: z.string().uuid().optional(),
        project_id: z.string().uuid().optional(),
        limit: z.number().optional().describe("Maximum items to return (default: 20)"),
      }),
    },
    async (input) => {
      // Get workspace_id from session context if not provided
      let workspaceId = input.workspace_id;
      let projectId = input.project_id;

      if (!workspaceId && sessionManager) {
        const ctx = sessionManager.getContext();
        if (ctx) {
          workspaceId = ctx.workspace_id as string | undefined;
          projectId = projectId || (ctx.project_id as string | undefined);
        }
      }

      const result = await client.getContextDelta({
        workspace_id: workspaceId,
        project_id: projectId,
        since: input.since,
        limit: input.limit,
      });

      const summary = [
        `📈 Context changes since ${input.since}:`,
        `   New decisions: ${result.new_decisions}`,
        `   New memory events: ${result.new_memory}`,
        "",
        ...result.items.slice(0, 10).map((i) => `• [${i.type}] ${i.title}`),
        result.items.length > 10 ? `   (+${result.items.length - 10} more)` : "",
      ]
        .filter(Boolean)
        .join("\n");

      return {
        content: [{ type: "text" as const, text: summary }],
        structuredContent: toStructured(result),
      };
    }
  );

  registerTool(
    "context_smart",
    {
      title: "Get smart context for user query",
      description: `**CALL THIS BEFORE EVERY AI RESPONSE** to get relevant context.

This is the KEY tool for token-efficient AI interactions. It:
1. Analyzes the user's message to understand what context is needed
2. Retrieves only relevant context in a minified, token-efficient format
3. Replaces the need to include full chat history in prompts

Format options:
- 'minified': Ultra-compact D:decision|P:preference|M:memory (default, ~200 tokens)
- 'readable': Line-separated with labels
- 'structured': JSON-like grouped format

Type codes: W=Workspace, P=Project, D=Decision, M=Memory, I=Insight, T=Task, L=Lesson

Context Pack:
- mode='pack' adds code context + distillation (higher credit cost)

Example usage:
1. User asks "how should I implement auth?"
2. AI calls context_smart(user_message="how should I implement auth?")
3. Gets: "W:Maker|P:contextstream|D:Use JWT for auth|D:No session cookies|M:Auth API at /auth/..."
4. AI responds with relevant context already loaded

This saves ~80% tokens compared to including full chat history.`,
      inputSchema: z.object({
        user_message: z.string().describe("The user message to analyze and get context for"),
        workspace_id: z.string().uuid().optional(),
        project_id: z.string().uuid().optional(),
        max_tokens: z.number().optional().describe("Maximum tokens for context (default: 800)"),
        format: z
          .enum(["minified", "readable", "structured"])
          .optional()
          .describe("Context format (default: minified)"),
        mode: z
          .enum(["standard", "pack"])
          .optional()
          .describe("Context pack mode (default: pack when enabled)"),
        distill: z
          .boolean()
          .optional()
          .describe("Use distillation for context pack (default: true)"),
      }),
    },
    async (input) => {
      // Mark that context_smart has been called in this session
      if (sessionManager) {
        sessionManager.markContextSmartCalled();
      }

      // Get workspace_id from session context if not provided
      let workspaceId = input.workspace_id;
      let projectId = input.project_id;

      if (!workspaceId && sessionManager) {
        const ctx = sessionManager.getContext();
        if (ctx) {
          workspaceId = ctx.workspace_id as string | undefined;
          projectId = projectId || (ctx.project_id as string | undefined);
        }
      }

      const result = await client.getSmartContext({
        user_message: input.user_message,
        workspace_id: workspaceId,
        project_id: projectId,
        max_tokens: input.max_tokens,
        format: input.format,
        mode: input.mode,
        distill: input.distill,
      });

      // Return context directly for easy inclusion in AI prompts
      const footer = `\n---\n🎯 ${result.sources_used} sources | ~${result.token_estimate} tokens | format: ${result.format}`;
      const folderPathForRules = resolveFolderPath(undefined, sessionManager);
      const rulesNotice = getRulesNotice(folderPathForRules, detectedClientInfo?.name);

      let versionNotice = result.version_notice;
      if (!versionNotice) {
        try {
          versionNotice = (await getUpdateNotice()) ?? undefined;
        } catch {
          // ignore version check failures
        }
      }

      // Generate aggressive warnings for outdated rules/version
      const rulesWarningLine = generateRulesUpdateWarning(rulesNotice);
      const versionWarningLine = generateVersionUpdateWarning(versionNotice ?? null);

      const enrichedResult = {
        ...result,
        ...(rulesNotice ? { rules_notice: rulesNotice } : {}),
        ...(versionNotice ? { version_notice: versionNotice } : {}),
      };

      // Track token savings (fire-and-forget)
      // context_smart is the most frequently called tool, so tracking is important
      trackToolTokenSavings(client, "context_smart", result.context, {
        workspace_id: workspaceId,
        project_id: projectId,
        max_tokens: input.max_tokens,
      });

      // Check if lessons are present in the context (L: prefix in minified, or "lesson" keyword)
      const hasLessons =
        result.context.includes("|L:") ||
        result.context.includes("L:") ||
        result.context.toLowerCase().includes("lesson");
      const lessonsWarningLine = hasLessons
        ? "\n\n⚠️ [LESSONS DETECTED] Review the L: items above - these are past mistakes. STOP and review before making similar changes."
        : "";

      // Inject search rules reminder to combat instruction decay
      const searchRulesLine = SEARCH_RULES_REMINDER_ENABLED ? `\n\n${SEARCH_RULES_REMINDER}` : "";

      // Combine all warnings (only add non-empty ones with proper spacing)
      const allWarnings = [
        lessonsWarningLine,
        rulesWarningLine ? `\n\n${rulesWarningLine}` : "",
        versionWarningLine ? `\n\n${versionWarningLine}` : "",
        searchRulesLine,
      ].filter(Boolean).join("");

      return {
        content: [
          {
            type: "text" as const,
            text: result.context + footer + allWarnings,
          },
        ],
        structuredContent: toStructured(enrichedResult),
      };
    }
  );

  registerTool(
    "context_feedback",
    {
      title: "Submit smart context feedback",
      description: "Send relevance feedback (relevant/irrelevant/pin) for context_smart items.",
      inputSchema: z.object({
        workspace_id: z.string().uuid().optional(),
        project_id: z.string().uuid().optional(),
        item_id: z.string().describe("Item ID returned by context_smart"),
        item_type: z.enum(["memory_event", "knowledge_node", "code_chunk"]),
        feedback_type: z.enum(["relevant", "irrelevant", "pin"]),
        query_text: z.string().optional(),
        metadata: z.record(z.any()).optional(),
      }),
    },
    async (input) => {
      // Get workspace_id from session context if not provided
      let workspaceId = input.workspace_id;
      let projectId = input.project_id;

      if (!workspaceId && sessionManager) {
        const ctx = sessionManager.getContext();
        if (ctx) {
          workspaceId = ctx.workspace_id as string | undefined;
          projectId = projectId || (ctx.project_id as string | undefined);
        }
      }

      const result = await client.submitContextFeedback({
        ...input,
        workspace_id: workspaceId,
        project_id: projectId,
      });

      return {
        content: [{ type: "text" as const, text: formatContent(result) }],
        structuredContent: toStructured(result),
      };
    }
  );

  // ============================================
  // Slack Integration Tools
  // ============================================

  registerTool(
    "slack_stats",
    {
      title: "Slack overview stats",
      description: `Get Slack integration statistics and overview for a workspace.
Returns: total messages, threads, active users, channel stats, activity trends, and sync status.
Use this to understand Slack activity and engagement patterns.`,
      inputSchema: z.object({
        workspace_id: z.string().uuid().optional(),
        days: z.number().optional().describe("Number of days to include in stats (default: 30)"),
      }),
    },
    async (input) => {
      const workspaceId = resolveWorkspaceId(input.workspace_id);
      if (!workspaceId) {
        return errorResult(
          "Error: workspace_id is required. Please call session_init first or provide workspace_id explicitly."
        );
      }

      const result = await client.slackStats({ workspace_id: workspaceId, days: input.days });
      return {
        content: [{ type: "text" as const, text: formatContent(result) }],
        structuredContent: toStructured(result),
      };
    }
  );

  registerTool(
    "slack_channels",
    {
      title: "List Slack channels",
      description: `Get synced Slack channels with statistics for a workspace.
Returns: channel names, message counts, thread counts, and last activity timestamps.`,
      inputSchema: z.object({
        workspace_id: z.string().uuid().optional(),
      }),
    },
    async (input) => {
      const workspaceId = resolveWorkspaceId(input.workspace_id);
      if (!workspaceId) {
        return errorResult(
          "Error: workspace_id is required. Please call session_init first or provide workspace_id explicitly."
        );
      }

      const result = await client.slackChannels({ workspace_id: workspaceId });
      return {
        content: [{ type: "text" as const, text: formatContent(result) }],
        structuredContent: toStructured(result),
      };
    }
  );

  registerTool(
    "slack_contributors",
    {
      title: "Slack top contributors",
      description: `Get top Slack contributors for a workspace.
Returns: user profiles with message counts, sorted by activity level.`,
      inputSchema: z.object({
        workspace_id: z.string().uuid().optional(),
        limit: z.number().optional().describe("Maximum contributors to return (default: 20)"),
      }),
    },
    async (input) => {
      const workspaceId = resolveWorkspaceId(input.workspace_id);
      if (!workspaceId) {
        return errorResult(
          "Error: workspace_id is required. Please call session_init first or provide workspace_id explicitly."
        );
      }

      const result = await client.slackContributors({
        workspace_id: workspaceId,
        limit: input.limit,
      });
      return {
        content: [{ type: "text" as const, text: formatContent(result) }],
        structuredContent: toStructured(result),
      };
    }
  );

  registerTool(
    "slack_activity",
    {
      title: "Slack activity feed",
      description: `Get recent Slack activity feed for a workspace.
Returns: messages with user info, reactions, replies, and timestamps.
Can filter by channel.`,
      inputSchema: z.object({
        workspace_id: z.string().uuid().optional(),
        limit: z.number().optional().describe("Maximum messages to return (default: 50)"),
        offset: z.number().optional().describe("Pagination offset"),
        channel_id: z.string().optional().describe("Filter by specific channel ID"),
      }),
    },
    async (input) => {
      const workspaceId = resolveWorkspaceId(input.workspace_id);
      if (!workspaceId) {
        return errorResult(
          "Error: workspace_id is required. Please call session_init first or provide workspace_id explicitly."
        );
      }

      const result = await client.slackActivity({
        workspace_id: workspaceId,
        limit: input.limit,
        offset: input.offset,
        channel_id: input.channel_id,
      });
      return {
        content: [{ type: "text" as const, text: formatContent(result) }],
        structuredContent: toStructured(result),
      };
    }
  );

  registerTool(
    "slack_discussions",
    {
      title: "Slack key discussions",
      description: `Get high-engagement Slack discussions/threads for a workspace.
Returns: threads with high reply/reaction counts, sorted by engagement.
Useful for finding important conversations and decisions.`,
      inputSchema: z.object({
        workspace_id: z.string().uuid().optional(),
        limit: z.number().optional().describe("Maximum discussions to return (default: 20)"),
      }),
    },
    async (input) => {
      const workspaceId = resolveWorkspaceId(input.workspace_id);
      if (!workspaceId) {
        return errorResult(
          "Error: workspace_id is required. Please call session_init first or provide workspace_id explicitly."
        );
      }

      const result = await client.slackDiscussions({
        workspace_id: workspaceId,
        limit: input.limit,
      });
      return {
        content: [{ type: "text" as const, text: formatContent(result) }],
        structuredContent: toStructured(result),
      };
    }
  );

  registerTool(
    "slack_search",
    {
      title: "Search Slack messages",
      description: `Search Slack messages for a workspace.
Returns: matching messages with channel, user, and engagement info.
Use this to find specific conversations or topics.`,
      inputSchema: z.object({
        workspace_id: z.string().uuid().optional(),
        q: z.string().describe("Search query"),
        limit: z.number().optional().describe("Maximum results (default: 50)"),
      }),
    },
    async (input) => {
      const workspaceId = resolveWorkspaceId(input.workspace_id);
      if (!workspaceId) {
        return errorResult(
          "Error: workspace_id is required. Please call session_init first or provide workspace_id explicitly."
        );
      }

      const result = await client.slackSearch({
        workspace_id: workspaceId,
        q: input.q,
        limit: input.limit,
      });
      return {
        content: [{ type: "text" as const, text: formatContent(result) }],
        structuredContent: toStructured(result),
      };
    }
  );

  registerTool(
    "slack_sync_users",
    {
      title: "Sync Slack users",
      description: `Trigger a sync of Slack user profiles for a workspace.
This fetches the latest user info from Slack and updates local profiles.
Also auto-maps Slack users to ContextStream users by email.`,
      inputSchema: z.object({
        workspace_id: z.string().uuid().optional(),
      }),
    },
    async (input) => {
      const workspaceId = resolveWorkspaceId(input.workspace_id);
      if (!workspaceId) {
        return errorResult(
          "Error: workspace_id is required. Please call session_init first or provide workspace_id explicitly."
        );
      }

      const result = await client.slackSyncUsers({ workspace_id: workspaceId });
      return {
        content: [
          {
            type: "text" as const,
            text: `✅ Synced ${result.synced_users} Slack users, auto-mapped ${result.auto_mapped} by email.`,
          },
        ],
        structuredContent: toStructured(result),
      };
    }
  );

  // ============================================
  // GitHub Integration Tools
  // ============================================

  registerTool(
    "github_stats",
    {
      title: "GitHub overview stats",
      description: `Get GitHub integration statistics and overview for a workspace.
Returns: total issues, PRs, releases, comments, repository stats, activity trends, and sync status.
Use this to understand GitHub activity and engagement patterns across synced repositories.`,
      inputSchema: z.object({
        workspace_id: z.string().uuid().optional(),
      }),
    },
    async (input) => {
      const workspaceId = resolveWorkspaceId(input.workspace_id);
      if (!workspaceId) {
        return errorResult(
          "Error: workspace_id is required. Please call session_init first or provide workspace_id explicitly."
        );
      }

      const result = await client.githubStats({ workspace_id: workspaceId });
      return {
        content: [{ type: "text" as const, text: formatContent(result) }],
        structuredContent: toStructured(result),
      };
    }
  );

  registerTool(
    "github_repos",
    {
      title: "List GitHub repositories",
      description: `Get synced GitHub repositories with statistics for a workspace.
Returns: repository names with issue, PR, release, and comment counts, plus last activity timestamps.`,
      inputSchema: z.object({
        workspace_id: z.string().uuid().optional(),
      }),
    },
    async (input) => {
      const workspaceId = resolveWorkspaceId(input.workspace_id);
      if (!workspaceId) {
        return errorResult(
          "Error: workspace_id is required. Please call session_init first or provide workspace_id explicitly."
        );
      }

      const result = await client.githubRepos({ workspace_id: workspaceId });
      return {
        content: [{ type: "text" as const, text: formatContent(result) }],
        structuredContent: toStructured(result),
      };
    }
  );

  registerTool(
    "github_contributors",
    {
      title: "GitHub top contributors",
      description: `Get top GitHub contributors for a workspace.
Returns: usernames with contribution counts, sorted by activity level.`,
      inputSchema: z.object({
        workspace_id: z.string().uuid().optional(),
        limit: z.number().optional().describe("Maximum contributors to return (default: 20)"),
      }),
    },
    async (input) => {
      const workspaceId = resolveWorkspaceId(input.workspace_id);
      if (!workspaceId) {
        return errorResult(
          "Error: workspace_id is required. Please call session_init first or provide workspace_id explicitly."
        );
      }

      const result = await client.githubContributors({
        workspace_id: workspaceId,
        limit: input.limit,
      });
      return {
        content: [{ type: "text" as const, text: formatContent(result) }],
        structuredContent: toStructured(result),
      };
    }
  );

  registerTool(
    "github_activity",
    {
      title: "GitHub activity feed",
      description: `Get recent GitHub activity feed for a workspace.
Returns: issues, PRs, releases, and comments with details like state, author, labels.
Can filter by repository or type (issue, pull_request, release, comment).`,
      inputSchema: z.object({
        workspace_id: z.string().uuid().optional(),
        limit: z.number().optional().describe("Maximum items to return (default: 50)"),
        offset: z.number().optional().describe("Pagination offset"),
        repo: z.string().optional().describe("Filter by repository name"),
        type: z
          .enum(["issue", "pull_request", "release", "comment"])
          .optional()
          .describe("Filter by item type"),
      }),
    },
    async (input) => {
      const workspaceId = resolveWorkspaceId(input.workspace_id);
      if (!workspaceId) {
        return errorResult(
          "Error: workspace_id is required. Please call session_init first or provide workspace_id explicitly."
        );
      }

      const result = await client.githubActivity({
        workspace_id: workspaceId,
        limit: input.limit,
        offset: input.offset,
        repo: input.repo,
        type: input.type,
      });
      return {
        content: [{ type: "text" as const, text: formatContent(result) }],
        structuredContent: toStructured(result),
      };
    }
  );

  registerTool(
    "github_issues",
    {
      title: "GitHub issues and PRs",
      description: `Get GitHub issues and pull requests for a workspace.
Returns: issues/PRs with title, state, author, labels, comment count.
Can filter by state (open/closed) or repository.`,
      inputSchema: z.object({
        workspace_id: z.string().uuid().optional(),
        limit: z.number().optional().describe("Maximum items to return (default: 50)"),
        offset: z.number().optional().describe("Pagination offset"),
        state: z.enum(["open", "closed"]).optional().describe("Filter by state"),
        repo: z.string().optional().describe("Filter by repository name"),
      }),
    },
    async (input) => {
      const workspaceId = resolveWorkspaceId(input.workspace_id);
      if (!workspaceId) {
        return errorResult(
          "Error: workspace_id is required. Please call session_init first or provide workspace_id explicitly."
        );
      }

      const result = await client.githubIssues({
        workspace_id: workspaceId,
        limit: input.limit,
        offset: input.offset,
        state: input.state,
        repo: input.repo,
      });
      return {
        content: [{ type: "text" as const, text: formatContent(result) }],
        structuredContent: toStructured(result),
      };
    }
  );

  registerTool(
    "github_search",
    {
      title: "Search GitHub content",
      description: `Search GitHub issues, PRs, and comments for a workspace.
Returns: matching items with repository, title, state, and content preview.
Use this to find specific issues, PRs, or discussions.`,
      inputSchema: z.object({
        workspace_id: z.string().uuid().optional(),
        q: z.string().describe("Search query"),
        limit: z.number().optional().describe("Maximum results (default: 50)"),
      }),
    },
    async (input) => {
      const workspaceId = resolveWorkspaceId(input.workspace_id);
      if (!workspaceId) {
        return errorResult(
          "Error: workspace_id is required. Please call session_init first or provide workspace_id explicitly."
        );
      }

      const result = await client.githubSearch({
        workspace_id: workspaceId,
        q: input.q,
        limit: input.limit,
      });
      return {
        content: [{ type: "text" as const, text: formatContent(result) }],
        structuredContent: toStructured(result),
      };
    }
  );

  registerTool(
    "github_knowledge",
    {
      title: "GitHub extracted knowledge",
      description: `Get knowledge extracted from GitHub issues and PRs.
Returns: decisions, lessons, and insights automatically distilled from GitHub conversations.
This surfaces key decisions and learnings from your repository discussions.

Example queries:
- "What decisions were made about authentication?"
- "What lessons learned from production incidents?"
- "Show recent architectural decisions"`,
      inputSchema: z.object({
        workspace_id: z.string().uuid().optional(),
        limit: z.number().optional().describe("Maximum items to return (default: 20)"),
        node_type: z
          .enum(["decision", "lesson", "fact", "insight"])
          .optional()
          .describe("Filter by knowledge type"),
      }),
    },
    async (input) => {
      const workspaceId = resolveWorkspaceId(input.workspace_id);
      if (!workspaceId) {
        return errorResult(
          "Error: workspace_id is required. Please call session_init first or provide workspace_id explicitly."
        );
      }

      const result = await client.githubKnowledge({
        workspace_id: workspaceId,
        limit: input.limit,
        node_type: input.node_type,
      });
      if (result.length === 0) {
        return {
          content: [
            {
              type: "text" as const,
              text: "No knowledge extracted from GitHub yet. Knowledge is distilled from issues/PRs after sync.",
            },
          ],
        };
      }
      return {
        content: [{ type: "text" as const, text: formatContent(result) }],
        structuredContent: toStructured(result),
      };
    }
  );

  registerTool(
    "slack_knowledge",
    {
      title: "Slack extracted knowledge",
      description: `Get knowledge extracted from Slack conversations.
Returns: decisions, lessons, and insights automatically distilled from Slack discussions.
This surfaces key decisions and learnings from your team conversations.

Example queries:
- "What decisions were made in #engineering this week?"
- "Show lessons learned from outages"
- "What architectural insights came from Slack?"`,
      inputSchema: z.object({
        workspace_id: z.string().uuid().optional(),
        limit: z.number().optional().describe("Maximum items to return (default: 20)"),
        node_type: z
          .enum(["decision", "lesson", "fact", "insight"])
          .optional()
          .describe("Filter by knowledge type"),
      }),
    },
    async (input) => {
      const workspaceId = resolveWorkspaceId(input.workspace_id);
      if (!workspaceId) {
        return errorResult(
          "Error: workspace_id is required. Please call session_init first or provide workspace_id explicitly."
        );
      }

      const result = await client.slackKnowledge({
        workspace_id: workspaceId,
        limit: input.limit,
        node_type: input.node_type,
      });
      if (result.length === 0) {
        return {
          content: [
            {
              type: "text" as const,
              text: "No knowledge extracted from Slack yet. Knowledge is distilled from high-engagement threads after sync.",
            },
          ],
        };
      }
      return {
        content: [{ type: "text" as const, text: formatContent(result) }],
        structuredContent: toStructured(result),
      };
    }
  );

  registerTool(
    "github_summary",
    {
      title: "GitHub activity summary",
      description: `Get a high-level summary of GitHub activity for a workspace.
Returns: overview of issues, PRs, commits, releases, and highlights for the specified period.
Use this for weekly/monthly reports or to get a quick overview of repository activity.

Example prompts:
- "Give me a weekly GitHub summary"
- "What happened in GitHub last month?"
- "Show me the GitHub summary for repo X"`,
      inputSchema: z.object({
        workspace_id: z.string().uuid().optional(),
        days: z.number().optional().describe("Number of days to summarize (default: 7)"),
        repo: z.string().optional().describe("Filter by repository name"),
      }),
    },
    async (input) => {
      const workspaceId = resolveWorkspaceId(input.workspace_id);
      if (!workspaceId) {
        return errorResult(
          "Error: workspace_id is required. Please call session_init first or provide workspace_id explicitly."
        );
      }

      const result = await client.githubSummary({
        workspace_id: workspaceId,
        days: input.days,
        repo: input.repo,
      });
      return {
        content: [{ type: "text" as const, text: formatContent(result) }],
        structuredContent: toStructured(result),
      };
    }
  );

  registerTool(
    "slack_summary",
    {
      title: "Slack activity summary",
      description: `Get a high-level summary of Slack activity for a workspace.
Returns: overview of messages, threads, top channels, and highlights for the specified period.
Use this for weekly/monthly reports or to get a quick overview of team discussions.

Example prompts:
- "Give me a weekly Slack summary"
- "What was discussed in Slack last month?"
- "Show me the Slack summary for #engineering"`,
      inputSchema: z.object({
        workspace_id: z.string().uuid().optional(),
        days: z.number().optional().describe("Number of days to summarize (default: 7)"),
        channel: z.string().optional().describe("Filter by channel name"),
      }),
    },
    async (input) => {
      const workspaceId = resolveWorkspaceId(input.workspace_id);
      if (!workspaceId) {
        return errorResult(
          "Error: workspace_id is required. Please call session_init first or provide workspace_id explicitly."
        );
      }

      const result = await client.slackSummary({
        workspace_id: workspaceId,
        days: input.days,
        channel: input.channel,
      });
      return {
        content: [{ type: "text" as const, text: formatContent(result) }],
        structuredContent: toStructured(result),
      };
    }
  );

  // ============================================
  // Notion Integration Tools
  // ============================================

  registerTool(
    "notion_create_page",
    {
      title: "Create Notion page",
      description: `Create a new page in a connected Notion workspace.
Returns: the created page ID, URL, title, and timestamps.
Use this to save notes, documentation, or any content to Notion.
Supports Markdown content which is automatically converted to Notion blocks.

Example prompts:
- "Create a Notion page with today's meeting notes"
- "Save this documentation to Notion"
- "Create a new page in my Notion workspace"`,
      inputSchema: z.object({
        workspace_id: z.string().uuid().optional().describe("Workspace ID (uses session default if not provided)"),
        project_id: z.string().uuid().optional().describe("Project ID (uses session default if not provided). If provided, the memory event will be scoped to this project."),
        title: z.string().describe("Page title"),
        content: z.string().optional().describe("Page content in Markdown format"),
        parent_database_id: z.string().optional().describe("Parent database ID to create page in"),
        parent_page_id: z.string().optional().describe("Parent page ID to create page under"),
      }),
    },
    async (input) => {
      const workspaceId = resolveWorkspaceId(input.workspace_id);
      const projectId = resolveProjectId(input.project_id);
      if (!workspaceId) {
        return errorResult(
          "Error: workspace_id is required. Please call session_init first or provide workspace_id explicitly."
        );
      }

      const result = await client.createNotionPage({
        workspace_id: workspaceId,
        project_id: projectId,
        title: input.title,
        content: input.content,
        parent_database_id: input.parent_database_id,
        parent_page_id: input.parent_page_id,
      });
      return {
        content: [
          {
            type: "text" as const,
            text: `Page created successfully!\n\nTitle: ${result.title}\nURL: ${result.url}\nID: ${result.id}\nCreated: ${result.created_time}`,
          },
        ],
        structuredContent: toStructured(result),
      };
    }
  );

  // ============================================
  // Cross-Integration Tools
  // ============================================

  registerTool(
    "integrations_search",
    {
      title: "Cross-source search",
      description: `Search across all connected integrations (GitHub, Slack, etc.) with a single query.
Returns: unified results from all sources, ranked by relevance or recency.
Use this to find related discussions, issues, and content across all your tools.

Example prompts:
- "Search all integrations for database migration discussions"
- "Find mentions of authentication across GitHub and Slack"
- "Search for API changes in the last 30 days"`,
      inputSchema: z.object({
        workspace_id: z.string().uuid().optional(),
        query: z.string().describe("Search query"),
        limit: z.number().optional().describe("Maximum results (default: 20)"),
        sources: z.array(z.string()).optional().describe("Filter by source: github, slack"),
        days: z.number().optional().describe("Filter to results within N days"),
        sort_by: z
          .enum(["relevance", "recent", "engagement"])
          .optional()
          .describe("Sort by: relevance, recent, or engagement"),
      }),
    },
    async (input) => {
      const workspaceId = resolveWorkspaceId(input.workspace_id);
      if (!workspaceId) {
        return errorResult(
          "Error: workspace_id is required. Please call session_init first or provide workspace_id explicitly."
        );
      }

      const result = await client.integrationsSearch({
        workspace_id: workspaceId,
        query: input.query,
        limit: input.limit,
        sources: input.sources,
        days: input.days,
        sort_by: input.sort_by,
      });
      return {
        content: [{ type: "text" as const, text: formatContent(result) }],
        structuredContent: toStructured(result),
      };
    }
  );

  registerTool(
    "integrations_summary",
    {
      title: "Cross-source activity summary",
      description: `Get a unified summary of activity across all connected integrations.
Returns: combined overview of GitHub and Slack activity, key highlights, and trends.
Use this for weekly team summaries or to understand overall activity across all tools.

Example prompts:
- "Give me a weekly team summary across all sources"
- "What happened across GitHub and Slack last week?"
- "Show me a unified activity overview"`,
      inputSchema: z.object({
        workspace_id: z.string().uuid().optional(),
        days: z.number().optional().describe("Number of days to summarize (default: 7)"),
      }),
    },
    async (input) => {
      const workspaceId = resolveWorkspaceId(input.workspace_id);
      if (!workspaceId) {
        return errorResult(
          "Error: workspace_id is required. Please call session_init first or provide workspace_id explicitly."
        );
      }

      const result = await client.integrationsSummary({
        workspace_id: workspaceId,
        days: input.days,
      });
      return {
        content: [{ type: "text" as const, text: formatContent(result) }],
        structuredContent: toStructured(result),
      };
    }
  );

  registerTool(
    "integrations_knowledge",
    {
      title: "Cross-source knowledge",
      description: `Get knowledge extracted from all connected integrations (GitHub, Slack, etc.).
Returns: decisions, lessons, and insights distilled from all sources.
Use this to find key decisions and learnings from across your team's conversations.

Example prompts:
- "What decisions were made across all sources about authentication?"
- "Show me lessons learned from all integrations"
- "What insights have we gathered from GitHub and Slack?"`,
      inputSchema: z.object({
        workspace_id: z.string().uuid().optional(),
        knowledge_type: z
          .enum(["decision", "lesson", "fact", "insight"])
          .optional()
          .describe("Filter by knowledge type"),
        query: z.string().optional().describe("Optional search query to filter knowledge"),
        sources: z.array(z.string()).optional().describe("Filter by source: github, slack"),
        limit: z.number().optional().describe("Maximum items to return (default: 20)"),
      }),
    },
    async (input) => {
      const workspaceId = resolveWorkspaceId(input.workspace_id);
      if (!workspaceId) {
        return errorResult(
          "Error: workspace_id is required. Please call session_init first or provide workspace_id explicitly."
        );
      }

      const result = await client.integrationsKnowledge({
        workspace_id: workspaceId,
        knowledge_type: input.knowledge_type,
        query: input.query,
        sources: input.sources,
        limit: input.limit,
      });
      return {
        content: [{ type: "text" as const, text: formatContent(result) }],
        structuredContent: toStructured(result),
      };
    }
  );

  registerTool(
    "integrations_status",
    {
      title: "Integration health status",
      description: `Check the status of all integrations (GitHub, Slack, etc.) for a workspace.
Returns: connection status, last sync time, next sync time, and any errors.
Use this to verify integrations are healthy and syncing properly.`,
      inputSchema: z.object({
        workspace_id: z.string().uuid().optional(),
      }),
    },
    async (input) => {
      const workspaceId = resolveWorkspaceId(input.workspace_id);
      if (!workspaceId) {
        return errorResult(
          "Error: workspace_id is required. Please call session_init first or provide workspace_id explicitly."
        );
      }

      const result = await client.integrationsStatus({ workspace_id: workspaceId });

      // Update integration status tracking (Strategy 2)
      if (AUTO_HIDE_INTEGRATIONS) {
        const slackConnected =
          result?.some(
            (s: { provider: string; status: string }) =>
              s.provider === "slack" && s.status === "connected"
          ) ?? false;
        const githubConnected =
          result?.some(
            (s: { provider: string; status: string }) =>
              s.provider === "github" && s.status === "connected"
          ) ?? false;
        const notionConnected =
          result?.some(
            (s: { provider: string; status: string }) =>
              s.provider === "notion" && s.status === "connected"
          ) ?? false;
        updateIntegrationStatus({ slack: slackConnected, github: githubConnected, notion: notionConnected }, workspaceId);
      }

      if (result.length === 0) {
        return {
          content: [
            { type: "text" as const, text: "No integrations configured for this workspace." },
          ],
        };
      }

      const formatted = result
        .map((i) => {
          const status = i.status === "connected" ? "✅" : i.status === "error" ? "❌" : "⏳";
          const lastSync = i.last_sync_at ? new Date(i.last_sync_at).toLocaleString() : "Never";
          const error = i.error_message ? ` (Error: ${i.error_message})` : "";
          return `${status} ${i.provider}: ${i.status} | Last sync: ${lastSync} | Resources: ${i.resources_synced}${error}`;
        })
        .join("\n");

      return {
        content: [{ type: "text" as const, text: formatted }],
        structuredContent: toStructured(result),
      };
    }
  );

  // ============================================
  // Reminder Tools
  // ============================================

  registerTool(
    "reminders_list",
    {
      title: "List reminders",
      description: `List all reminders for the current user.
Returns: reminders with title, content, remind_at, priority, status, and keywords.
Can filter by status (pending, completed, dismissed, snoozed) and priority (low, normal, high, urgent).

Use this to see what reminders you have set.`,
      inputSchema: z.object({
        workspace_id: z.string().uuid().optional(),
        project_id: z.string().uuid().optional(),
        status: z
          .enum(["pending", "completed", "dismissed", "snoozed"])
          .optional()
          .describe("Filter by status"),
        priority: z
          .enum(["low", "normal", "high", "urgent"])
          .optional()
          .describe("Filter by priority"),
        limit: z.number().optional().describe("Maximum reminders to return (default: 20)"),
      }),
    },
    async (input) => {
      const result = await client.remindersList({
        workspace_id: input.workspace_id,
        project_id: input.project_id,
        status: input.status,
        priority: input.priority,
        limit: input.limit,
      });
      if (!result.reminders || result.reminders.length === 0) {
        return { content: [{ type: "text" as const, text: "No reminders found." }] };
      }
      return {
        content: [{ type: "text" as const, text: formatContent(result) }],
        structuredContent: toStructured(result),
      };
    }
  );

  registerTool(
    "reminders_active",
    {
      title: "Get active reminders",
      description: `Get active reminders that are pending, overdue, or due soon.
Returns: reminders with urgency levels (overdue, due_soon, today, upcoming).
Optionally provide context (e.g., current task description) to get contextually relevant reminders.

Use this to see what reminders need attention now.`,
      inputSchema: z.object({
        workspace_id: z.string().uuid().optional(),
        project_id: z.string().uuid().optional(),
        context: z
          .string()
          .optional()
          .describe("Optional context to match relevant reminders (e.g., current task)"),
        limit: z.number().optional().describe("Maximum reminders to return (default: 10)"),
      }),
    },
    async (input) => {
      const result = await client.remindersActive({
        workspace_id: input.workspace_id,
        project_id: input.project_id,
        context: input.context,
        limit: input.limit,
      });

      if (!result.reminders || result.reminders.length === 0) {
        return { content: [{ type: "text" as const, text: "No active reminders." }] };
      }

      // Format with urgency indicators
      const formatted = result.reminders
        .map((r) => {
          const icon =
            r.urgency === "overdue"
              ? "🔴"
              : r.urgency === "due_soon"
                ? "🟠"
                : r.urgency === "today"
                  ? "🟡"
                  : "🔵";
          const priority = r.priority !== "normal" ? ` [${r.priority}]` : "";
          return `${icon} ${r.title}${priority}\n   Due: ${new Date(r.remind_at).toLocaleString()}\n   ${r.content_preview}`;
        })
        .join("\n\n");

      const header =
        result.overdue_count > 0 ? `⚠️ ${result.overdue_count} overdue reminder(s)\n\n` : "";

      return {
        content: [{ type: "text" as const, text: header + formatted }],
        structuredContent: toStructured(result),
      };
    }
  );

  registerTool(
    "reminders_create",
    {
      title: "Create a reminder",
      description: `Create a new reminder for a specific date/time.
Set reminders to be notified about tasks, follow-ups, or important dates.

Priority levels: low, normal, high, urgent
Recurrence: daily, weekly, monthly (optional)

Example: Create a reminder to "Review PR #123" for tomorrow at 10am with high priority.`,
      inputSchema: z.object({
        workspace_id: z.string().uuid().optional(),
        project_id: z.string().uuid().optional(),
        title: z.string().describe("Reminder title (brief, descriptive)"),
        content: z.string().describe("Reminder details/description"),
        remind_at: z
          .string()
          .describe('When to remind (ISO 8601 datetime, e.g., "2025-01-15T10:00:00Z")'),
        priority: z
          .enum(["low", "normal", "high", "urgent"])
          .optional()
          .describe("Priority level (default: normal)"),
        keywords: z.array(z.string()).optional().describe("Keywords for contextual surfacing"),
        recurrence: z
          .enum(["daily", "weekly", "monthly"])
          .optional()
          .describe("Recurrence pattern"),
      }),
    },
    async (input) => {
      const result = await client.remindersCreate({
        workspace_id: input.workspace_id,
        project_id: input.project_id,
        title: input.title,
        content: input.content,
        remind_at: input.remind_at,
        priority: input.priority,
        keywords: input.keywords,
        recurrence: input.recurrence,
      });

      const due = new Date(result.remind_at).toLocaleString();
      return {
        content: [
          {
            type: "text" as const,
            text: `✅ Reminder created: "${result.title}"\nDue: ${due}\nPriority: ${result.priority}\nID: ${result.id}`,
          },
        ],
        structuredContent: toStructured(result),
      };
    }
  );

  registerTool(
    "reminders_snooze",
    {
      title: "Snooze a reminder",
      description: `Snooze a reminder until a later time.
Use this to postpone a reminder without dismissing it.

Common snooze durations:
- 1 hour: add 1 hour to current time
- 4 hours: add 4 hours
- Tomorrow: next day at 9am
- Next week: 7 days from now`,
      inputSchema: z.object({
        reminder_id: z.string().uuid().describe("ID of the reminder to snooze"),
        until: z.string().describe("When to resurface the reminder (ISO 8601 datetime)"),
      }),
    },
    async (input) => {
      const result = await client.remindersSnooze({
        reminder_id: input.reminder_id,
        until: input.until,
      });

      const snoozedUntil = new Date(result.snoozed_until).toLocaleString();
      return {
        content: [{ type: "text" as const, text: `😴 Reminder snoozed until ${snoozedUntil}` }],
        structuredContent: toStructured(result),
      };
    }
  );

  registerTool(
    "reminders_complete",
    {
      title: "Complete a reminder",
      description: `Mark a reminder as completed.
Use this when the task or action associated with the reminder is done.`,
      inputSchema: z.object({
        reminder_id: z.string().uuid().describe("ID of the reminder to complete"),
      }),
    },
    async (input) => {
      const result = await client.remindersComplete({
        reminder_id: input.reminder_id,
      });

      return {
        content: [{ type: "text" as const, text: `✅ Reminder completed!` }],
        structuredContent: toStructured(result),
      };
    }
  );

  registerTool(
    "reminders_dismiss",
    {
      title: "Dismiss a reminder",
      description: `Dismiss a reminder without completing it.
Use this to remove a reminder that is no longer relevant.`,
      inputSchema: z.object({
        reminder_id: z.string().uuid().describe("ID of the reminder to dismiss"),
      }),
    },
    async (input) => {
      const result = await client.remindersDismiss({
        reminder_id: input.reminder_id,
      });

      return {
        content: [{ type: "text" as const, text: `🗑️ Reminder dismissed.` }],
        structuredContent: toStructured(result),
      };
    }
  );

  // =============================================================================
  // CONSOLIDATED DOMAIN TOOLS (Strategy 8)
  // =============================================================================
  // These tools are only registered when CONSOLIDATED_MODE is enabled (default in v0.4.x)
  // They consolidate ~58 individual tools into ~11 domain tools with action dispatch

  if (CONSOLIDATED_MODE) {
    // -------------------------------------------------------------------------
    // search - Consolidates search_semantic, search_hybrid, search_keyword, search_pattern
    // -------------------------------------------------------------------------
    registerTool(
      "search",
      {
        title: "Search",
        description: `Search workspace memory and knowledge. Modes: semantic (meaning-based), hybrid (semantic + keyword), keyword (exact match), pattern (regex), exhaustive (all matches like grep), refactor (word-boundary matching for symbol renaming).

Output formats: full (default, includes content), paths (file paths only - 80% token savings), minimal (compact - 60% savings), count (match counts only - 90% savings).`,
        inputSchema: z.object({
          mode: z
            .enum(["semantic", "hybrid", "keyword", "pattern", "exhaustive", "refactor"])
            .describe("Search mode"),
          query: z.string().describe("Search query"),
          workspace_id: z.string().uuid().optional(),
          project_id: z.string().uuid().optional(),
          limit: z.number().optional().describe("Max results to return (default: 3)"),
          offset: z.number().optional().describe("Offset for pagination"),
          content_max_chars: z
            .number()
            .optional()
            .describe("Max chars per result content (default: 400)"),
          context_lines: z
            .number()
            .min(0)
            .max(10)
            .optional()
            .describe("Lines of context around matches (like grep -C)"),
          exact_match_boost: z
            .number()
            .min(1)
            .max(10)
            .optional()
            .describe("Boost factor for exact matches (default: 2.0)"),
          output_format: z
            .enum(["full", "paths", "minimal", "count"])
            .optional()
            .describe(
              "Response format: full (default), paths (80% savings), minimal (60% savings), count (90% savings)"
            ),
        }),
      },
      async (input) => {
        const params = normalizeSearchParams(input);

        let result;
        let toolType: TokenSavingsToolType;
        switch (input.mode) {
          case "semantic":
            result = await client.searchSemantic(params);
            toolType = "search_semantic";
            break;
          case "hybrid":
            result = await client.searchHybrid(params);
            toolType = "search_hybrid";
            break;
          case "keyword":
            result = await client.searchKeyword(params);
            toolType = "search_keyword";
            break;
          case "pattern":
            result = await client.searchPattern(params);
            toolType = "search_pattern";
            break;
          case "exhaustive":
            result = await client.searchExhaustive(params);
            toolType = "search_exhaustive";
            break;
          case "refactor":
            result = await client.searchRefactor(params);
            toolType = "search_refactor";
            break;
          default:
            toolType = "search_hybrid";
        }

        const outputText = formatContent(result);

        // Track token savings (fire-and-forget)
        trackToolTokenSavings(client, toolType, outputText, {
          workspace_id: params.workspace_id,
          project_id: params.project_id,
        });

        return {
          content: [{ type: "text" as const, text: outputText }],
          structuredContent: toStructured(result),
        };
      }
    );

    // -------------------------------------------------------------------------
    // session - Consolidates session management tools
    // -------------------------------------------------------------------------
    registerTool(
      "session",
      {
        title: "Session",
        description: `Session management operations. Actions: capture (save decision/insight), capture_lesson (save lesson from mistake), get_lessons (retrieve lessons), recall (natural language recall), remember (quick save), user_context (get preferences), summary (workspace summary), compress (compress chat), delta (changes since timestamp), smart_search (context-enriched search), decision_trace (trace decision provenance). Plan actions: capture_plan (save implementation plan), get_plan (retrieve plan with tasks), update_plan (modify plan), list_plans (list all plans).`,
        inputSchema: z.object({
          action: z
            .enum([
              "capture",
              "capture_lesson",
              "get_lessons",
              "recall",
              "remember",
              "user_context",
              "summary",
              "compress",
              "delta",
              "smart_search",
              "decision_trace",
              // Plan actions
              "capture_plan",
              "get_plan",
              "update_plan",
              "list_plans",
            ])
            .describe("Action to perform"),
          workspace_id: z.string().uuid().optional(),
          project_id: z.string().uuid().optional(),
          // Content params
          query: z.string().optional().describe("Query for recall/search/lessons/decision_trace"),
          content: z.string().optional().describe("Content for capture/remember/compress"),
          title: z.string().optional().describe("Title for capture/capture_lesson/capture_plan"),
          event_type: z
            .enum([
              "decision",
              "preference",
              "insight",
              "note",
              "implementation",
              "task",
              "bug",
              "feature",
              "plan",
              "correction",
              "lesson",
              "warning",
              "frustration",
              "conversation",
            ])
            .optional()
            .describe("Event type for capture"),
          importance: z.enum(["low", "medium", "high", "critical"]).optional(),
          tags: z.array(z.string()).optional(),
          // Lesson-specific
          category: z
            .enum(["workflow", "code_quality", "verification", "communication", "project_specific"])
            .optional(),
          trigger: z.string().optional().describe("What caused the problem"),
          impact: z.string().optional().describe("What went wrong"),
          prevention: z.string().optional().describe("How to prevent in future"),
          severity: z.enum(["low", "medium", "high", "critical"]).optional(),
          keywords: z.array(z.string()).optional(),
          // Other params
          since: z.string().optional().describe("ISO timestamp for delta"),
          limit: z.number().optional(),
          max_tokens: z.number().optional().describe("Max tokens for summary"),
          include_decisions: z.boolean().optional(),
          include_related: z.boolean().optional(),
          include_impact: z.boolean().optional(),
          session_id: z.string().optional(),
          code_refs: z
            .array(
              z.object({
                file_path: z.string(),
                symbol_id: z.string().optional(),
                symbol_name: z.string().optional(),
              })
            )
            .optional(),
          provenance: z
            .object({
              repo: z.string().optional(),
              branch: z.string().optional(),
              commit_sha: z.string().optional(),
              pr_url: z.string().url().optional(),
              issue_url: z.string().url().optional(),
              slack_thread_url: z.string().url().optional(),
            })
            .optional(),
          // Plan-specific params
          plan_id: z.string().uuid().optional().describe("Plan ID for get_plan/update_plan"),
          description: z.string().optional().describe("Description for capture_plan"),
          goals: z.array(z.string()).optional().describe("Goals for capture_plan"),
          steps: z
            .array(
              z.object({
                id: z.string(),
                title: z.string(),
                description: z.string().optional(),
                order: z.number(),
                estimated_effort: z.enum(["small", "medium", "large"]).optional(),
              })
            )
            .optional()
            .describe("Implementation steps for capture_plan"),
          status: z
            .enum(["draft", "active", "completed", "archived", "abandoned"])
            .optional()
            .describe("Plan status"),
          due_at: z.string().optional().describe("Due date for plan (ISO timestamp)"),
          source_tool: z.string().optional().describe("Tool that generated this plan"),
          include_tasks: z.boolean().optional().describe("Include tasks when getting plan"),
        }),
      },
      async (input) => {
        const workspaceId = resolveWorkspaceId(input.workspace_id);
        const projectId = resolveProjectId(input.project_id);

        switch (input.action) {
          case "capture": {
            if (!input.event_type || !input.title || !input.content) {
              return errorResult("capture requires: event_type, title, content");
            }
            const result = await client.captureContext({
              workspace_id: workspaceId,
              project_id: projectId,
              event_type: input.event_type,
              title: input.title,
              content: input.content,
              importance: input.importance,
              tags: input.tags,
              session_id: input.session_id,
              code_refs: input.code_refs,
              provenance: input.provenance,
            });
            return {
              content: [{ type: "text" as const, text: formatContent(result) }],
              structuredContent: toStructured(result),
            };
          }

          case "capture_lesson": {
            if (!input.title || !input.trigger || !input.impact || !input.prevention) {
              return errorResult("capture_lesson requires: title, trigger, impact, prevention");
            }
            const lessonContent = [
              `## ${input.title}`,
              `**Severity:** ${input.severity || "medium"}`,
              input.category ? `**Category:** ${input.category}` : "",
              `### Trigger`,
              input.trigger,
              `### Impact`,
              input.impact,
              `### Prevention`,
              input.prevention,
            ]
              .filter(Boolean)
              .join("\n");

            const lessonInput = {
              title: input.title,
              category: input.category,
              trigger: input.trigger,
              impact: input.impact,
              prevention: input.prevention,
              severity: input.severity || "medium",
              keywords: input.keywords,
              workspace_id: workspaceId,
              project_id: projectId,
            };
            const signature = buildLessonSignature(
              lessonInput as any,
              workspaceId || "global",
              projectId
            );
            if (isDuplicateLessonCapture(signature)) {
              return {
                content: [
                  {
                    type: "text" as const,
                    text: formatContent({
                      deduplicated: true,
                      message: "Lesson already captured recently",
                    }),
                  },
                ],
              };
            }
            const result = await client.captureContext({
              workspace_id: workspaceId,
              project_id: projectId,
              event_type: "lesson",
              title: input.title,
              content: lessonContent,
              importance:
                input.severity === "critical"
                  ? "critical"
                  : input.severity === "high"
                    ? "high"
                    : "medium",
              tags: input.keywords || [],
            });
            return {
              content: [{ type: "text" as const, text: formatContent(result) }],
              structuredContent: toStructured(result),
            };
          }

          case "get_lessons": {
            if (!workspaceId) {
              return errorResult("get_lessons requires workspace_id. Call session_init first.");
            }
            const result = await client.getHighPriorityLessons({
              workspace_id: workspaceId,
              project_id: projectId,
              context_hint: input.query,
              limit: input.limit,
            });
            return {
              content: [{ type: "text" as const, text: formatContent(result) }],
              structuredContent: toStructured(result),
            };
          }

          case "recall": {
            if (!input.query) {
              return errorResult("recall requires: query");
            }
            const result = await client.smartSearch({
              workspace_id: workspaceId,
              project_id: projectId,
              query: input.query,
              include_related: input.include_related,
              include_decisions: input.include_decisions,
            });
            const outputText = formatContent(result);
            // Track token savings
            trackToolTokenSavings(client, "session_recall", outputText, {
              workspace_id: workspaceId,
              project_id: projectId,
            });
            return {
              content: [{ type: "text" as const, text: outputText }],
              structuredContent: toStructured(result),
            };
          }

          case "remember": {
            if (!input.content) {
              return errorResult("remember requires: content");
            }
            // Map "critical" to "high" for the client API
            const importance =
              input.importance === "critical" ? "high" : (input.importance as "low" | "medium" | "high" | undefined);
            const result = await client.sessionRemember({
              workspace_id: workspaceId,
              project_id: projectId,
              content: input.content,
              importance,
            });
            return {
              content: [{ type: "text" as const, text: formatContent(result) }],
              structuredContent: toStructured(result),
            };
          }

          case "user_context": {
            const result = await client.getUserContext({ workspace_id: workspaceId });
            const outputText = formatContent(result);
            // Track token savings
            trackToolTokenSavings(client, "session_user_context", outputText, {
              workspace_id: workspaceId,
            });
            return {
              content: [{ type: "text" as const, text: outputText }],
              structuredContent: toStructured(result),
            };
          }

          case "summary": {
            const result = await client.getContextSummary({
              workspace_id: workspaceId,
              project_id: projectId,
              max_tokens: input.max_tokens,
            });
            const outputText = formatContent(result);
            // Track token savings
            trackToolTokenSavings(client, "session_summary", outputText, {
              workspace_id: workspaceId,
              project_id: projectId,
              max_tokens: input.max_tokens,
            });
            return {
              content: [{ type: "text" as const, text: outputText }],
              structuredContent: toStructured(result),
            };
          }

          case "compress": {
            if (!input.content) {
              return errorResult("compress requires: content (the chat history to compress)");
            }
            const result = await client.compressChat({
              workspace_id: workspaceId,
              project_id: projectId,
              chat_history: input.content,
            });
            return {
              content: [{ type: "text" as const, text: formatContent(result) }],
              structuredContent: toStructured(result),
            };
          }

          case "delta": {
            if (!input.since) {
              return errorResult("delta requires: since (ISO timestamp)");
            }
            const result = await client.getContextDelta({
              workspace_id: workspaceId,
              project_id: projectId,
              since: input.since,
              limit: input.limit,
            });
            return {
              content: [{ type: "text" as const, text: formatContent(result) }],
              structuredContent: toStructured(result),
            };
          }

          case "smart_search": {
            if (!input.query) {
              return errorResult("smart_search requires: query");
            }
            const result = await client.smartSearch({
              workspace_id: workspaceId,
              project_id: projectId,
              query: input.query,
              include_decisions: input.include_decisions,
              include_related: input.include_related,
            });
            const outputText = formatContent(result);
            // Track token savings
            trackToolTokenSavings(client, "session_smart_search", outputText, {
              workspace_id: workspaceId,
              project_id: projectId,
            });
            return {
              content: [{ type: "text" as const, text: outputText }],
              structuredContent: toStructured(result),
            };
          }

          case "decision_trace": {
            if (!input.query) {
              return errorResult("decision_trace requires: query");
            }
            const result = await client.decisionTrace({
              workspace_id: workspaceId,
              project_id: projectId,
              query: input.query,
              include_impact: input.include_impact,
              limit: input.limit,
            });
            return {
              content: [{ type: "text" as const, text: formatContent(result) }],
              structuredContent: toStructured(result),
            };
          }

          // Plan actions
          case "capture_plan": {
            if (!input.title) {
              return errorResult("capture_plan requires: title");
            }
            if (!workspaceId) {
              return errorResult("capture_plan requires workspace_id. Call session_init first.");
            }
            const result = await client.createPlan({
              workspace_id: workspaceId,
              project_id: projectId,
              title: input.title,
              content: input.content,
              description: input.description,
              goals: input.goals,
              steps: input.steps,
              status: input.status || "draft",
              tags: input.tags,
              due_at: input.due_at,
              source_tool: input.source_tool || "mcp",
            });
            return {
              content: [{ type: "text" as const, text: formatContent(result) }],
              structuredContent: toStructured(result),
            };
          }

          case "get_plan": {
            if (!input.plan_id) {
              return errorResult("get_plan requires: plan_id");
            }
            const result = await client.getPlan({
              plan_id: input.plan_id,
              include_tasks: input.include_tasks !== false,
            });
            return {
              content: [{ type: "text" as const, text: formatContent(result) }],
              structuredContent: toStructured(result),
            };
          }

          case "update_plan": {
            if (!input.plan_id) {
              return errorResult("update_plan requires: plan_id");
            }
            const result = await client.updatePlan({
              plan_id: input.plan_id,
              title: input.title,
              content: input.content,
              description: input.description,
              goals: input.goals,
              steps: input.steps,
              status: input.status,
              tags: input.tags,
              due_at: input.due_at,
            });
            return {
              content: [{ type: "text" as const, text: formatContent(result) }],
              structuredContent: toStructured(result),
            };
          }

          case "list_plans": {
            if (!workspaceId) {
              return errorResult("list_plans requires workspace_id. Call session_init first.");
            }
            const result = await client.listPlans({
              workspace_id: workspaceId,
              project_id: projectId,
              status: input.status,
              limit: input.limit,
            });
            return {
              content: [{ type: "text" as const, text: formatContent(result) }],
              structuredContent: toStructured(result),
            };
          }

          default:
            return errorResult(`Unknown action: ${input.action}`);
        }
      }
    );

    // -------------------------------------------------------------------------
    // memory - Consolidates memory event and node operations
    // -------------------------------------------------------------------------
    registerTool(
      "memory",
      {
        title: "Memory",
        description: `Memory operations for events and nodes. Event actions: create_event, get_event, update_event, delete_event, list_events, distill_event. Node actions: create_node, get_node, update_node, delete_node, list_nodes, supersede_node. Query actions: search, decisions, timeline, summary. Task actions: create_task (create task, optionally linked to plan), get_task, update_task (can link/unlink task to plan via plan_id), delete_task, list_tasks, reorder_tasks.`,
        inputSchema: z.object({
          action: z
            .enum([
              "create_event",
              "get_event",
              "update_event",
              "delete_event",
              "list_events",
              "distill_event",
              "create_node",
              "get_node",
              "update_node",
              "delete_node",
              "list_nodes",
              "supersede_node",
              "search",
              "decisions",
              "timeline",
              "summary",
              // Task actions
              "create_task",
              "get_task",
              "update_task",
              "delete_task",
              "list_tasks",
              "reorder_tasks",
            ])
            .describe("Action to perform"),
          workspace_id: z.string().uuid().optional(),
          project_id: z.string().uuid().optional(),
          // ID params
          event_id: z.string().uuid().optional(),
          node_id: z.string().uuid().optional(),
          // Content params
          title: z.string().optional(),
          content: z.string().optional(),
          event_type: z.string().optional(),
          node_type: z.string().optional(),
          metadata: z.record(z.any()).optional(),
          // Query params
          query: z.string().optional(),
          category: z.string().optional(),
          limit: z.number().optional(),
          // Node relations
          relations: z
            .array(
              z.object({
                type: z.string(),
                target_id: z.string().uuid(),
              })
            )
            .optional(),
          new_content: z.string().optional().describe("For supersede_node: the new content to replace the node with"),
          reason: z.string().optional().describe("For supersede_node: reason for the supersede"),
          // Provenance
          provenance: z
            .object({
              repo: z.string().optional(),
              branch: z.string().optional(),
              commit_sha: z.string().optional(),
              pr_url: z.string().url().optional(),
              issue_url: z.string().url().optional(),
              slack_thread_url: z.string().url().optional(),
            })
            .optional(),
          code_refs: z
            .array(
              z.object({
                file_path: z.string(),
                symbol_id: z.string().optional(),
                symbol_name: z.string().optional(),
              })
            )
            .optional(),
          // Task-specific params
          task_id: z
            .string()
            .uuid()
            .optional()
            .describe("Task ID for get_task/update_task/delete_task"),
          plan_id: z
            .string()
            .uuid()
            .nullable()
            .optional()
            .describe(
              "Plan ID: for create_task (link to plan), update_task (set UUID to link, null to unlink), list_tasks (filter by plan)"
            ),
          plan_step_id: z.string().optional().describe("Which plan step this task implements"),
          description: z.string().optional().describe("Description for task"),
          task_status: z
            .enum(["pending", "in_progress", "completed", "blocked", "cancelled"])
            .optional()
            .describe("Task status"),
          priority: z
            .enum(["low", "medium", "high", "urgent"])
            .optional()
            .describe("Task priority"),
          order: z.number().optional().describe("Task order within plan"),
          task_ids: z.array(z.string().uuid()).optional().describe("Task IDs for reorder_tasks"),
          blocked_reason: z.string().optional().describe("Reason when task is blocked"),
          tags: z.array(z.string()).optional().describe("Tags for task"),
        }),
      },
      async (input) => {
        const workspaceId = resolveWorkspaceId(input.workspace_id);
        const projectId = resolveProjectId(input.project_id);

        switch (input.action) {
          case "create_event": {
            if (!input.event_type || !input.title || !input.content) {
              return errorResult("create_event requires: event_type, title, content");
            }
            const result = await client.createMemoryEvent({
              workspace_id: workspaceId,
              project_id: projectId,
              event_type: input.event_type,
              title: input.title,
              content: input.content,
              metadata: input.metadata,
              provenance: input.provenance,
              code_refs: input.code_refs,
            });
            return {
              content: [{ type: "text" as const, text: formatContent(result) }],
              structuredContent: toStructured(result),
            };
          }

          case "get_event": {
            if (!input.event_id) {
              return errorResult("get_event requires: event_id");
            }
            const result = await client.getMemoryEvent(input.event_id);
            return {
              content: [{ type: "text" as const, text: formatContent(result) }],
              structuredContent: toStructured(result),
            };
          }

          case "update_event": {
            if (!input.event_id) {
              return errorResult("update_event requires: event_id");
            }
            const result = await client.updateMemoryEvent(input.event_id, {
              title: input.title,
              content: input.content,
              metadata: input.metadata,
            });
            return {
              content: [{ type: "text" as const, text: formatContent(result) }],
              structuredContent: toStructured(result),
            };
          }

          case "delete_event": {
            if (!input.event_id) {
              return errorResult("delete_event requires: event_id");
            }
            const result = await client.deleteMemoryEvent(input.event_id);
            return {
              content: [{ type: "text" as const, text: formatContent(result) }],
              structuredContent: toStructured(result),
            };
          }

          case "list_events": {
            const result = await client.listMemoryEvents({
              workspace_id: workspaceId,
              project_id: projectId,
              limit: input.limit,
            });
            return {
              content: [{ type: "text" as const, text: formatContent(result) }],
              structuredContent: toStructured(result),
            };
          }

          case "distill_event": {
            if (!input.event_id) {
              return errorResult("distill_event requires: event_id");
            }
            const result = await client.distillMemoryEvent(input.event_id);
            return {
              content: [{ type: "text" as const, text: formatContent(result) }],
              structuredContent: toStructured(result),
            };
          }

          case "create_node": {
            if (!input.node_type || !input.title || !input.content) {
              return errorResult("create_node requires: node_type, title, content");
            }
            const result = await client.createKnowledgeNode({
              workspace_id: workspaceId,
              project_id: projectId,
              node_type: input.node_type,
              title: input.title,
              content: input.content,
              relations: input.relations,
            });
            return {
              content: [{ type: "text" as const, text: formatContent(result) }],
              structuredContent: toStructured(result),
            };
          }

          case "get_node": {
            if (!input.node_id) {
              return errorResult("get_node requires: node_id");
            }
            const result = await client.getKnowledgeNode(input.node_id);
            return {
              content: [{ type: "text" as const, text: formatContent(result) }],
              structuredContent: toStructured(result),
            };
          }

          case "update_node": {
            if (!input.node_id) {
              return errorResult("update_node requires: node_id");
            }
            const result = await client.updateKnowledgeNode(input.node_id, {
              title: input.title,
              content: input.content,
            });
            return {
              content: [{ type: "text" as const, text: formatContent(result) }],
              structuredContent: toStructured(result),
            };
          }

          case "delete_node": {
            if (!input.node_id) {
              return errorResult("delete_node requires: node_id");
            }
            const result = await client.deleteKnowledgeNode(input.node_id);
            return {
              content: [{ type: "text" as const, text: formatContent(result) }],
              structuredContent: toStructured(result),
            };
          }

          case "list_nodes": {
            const result = await client.listKnowledgeNodes({
              workspace_id: workspaceId,
              project_id: projectId,
              limit: input.limit,
            });
            return {
              content: [{ type: "text" as const, text: formatContent(result) }],
              structuredContent: toStructured(result),
            };
          }

          case "supersede_node": {
            if (!input.node_id || !input.new_content) {
              return errorResult("supersede_node requires: node_id, new_content");
            }
            const result = await client.supersedeKnowledgeNode(input.node_id, {
              new_content: input.new_content,
              reason: input.reason,
            });
            return {
              content: [{ type: "text" as const, text: formatContent(result) }],
              structuredContent: toStructured(result),
            };
          }

          case "search": {
            if (!input.query) {
              return errorResult("search requires: query");
            }
            const result = await client.memorySearch({
              workspace_id: workspaceId,
              project_id: projectId,
              query: input.query,
              limit: input.limit,
            });
            const outputText = formatContent(result);
            // Track token savings
            trackToolTokenSavings(client, "memory_search", outputText, {
              workspace_id: workspaceId,
              project_id: projectId,
            });
            return {
              content: [{ type: "text" as const, text: outputText }],
              structuredContent: toStructured(result),
            };
          }

          case "decisions": {
            const result = await client.memoryDecisions({
              workspace_id: workspaceId,
              project_id: projectId,
              category: input.category,
              limit: input.limit,
            });
            const outputText = formatContent(result);
            // Track token savings
            trackToolTokenSavings(client, "memory_decisions", outputText, {
              workspace_id: workspaceId,
              project_id: projectId,
            });
            return {
              content: [{ type: "text" as const, text: outputText }],
              structuredContent: toStructured(result),
            };
          }

          case "timeline": {
            if (!workspaceId) {
              return errorResult("timeline requires workspace_id. Call session_init first.");
            }
            const result = await client.memoryTimeline(workspaceId);
            const outputText = formatContent(result);
            // Track token savings
            trackToolTokenSavings(client, "memory_timeline", outputText, {
              workspace_id: workspaceId,
            });
            return {
              content: [{ type: "text" as const, text: outputText }],
              structuredContent: toStructured(result),
            };
          }

          case "summary": {
            if (!workspaceId) {
              return errorResult("summary requires workspace_id. Call session_init first.");
            }
            const result = await client.memorySummary(workspaceId);
            const outputText = formatContent(result);
            // Track token savings
            trackToolTokenSavings(client, "memory_summary", outputText, {
              workspace_id: workspaceId,
            });
            return {
              content: [{ type: "text" as const, text: outputText }],
              structuredContent: toStructured(result),
            };
          }

          // Task actions
          case "create_task": {
            if (!input.title) {
              return errorResult("create_task requires: title");
            }
            if (!workspaceId) {
              return errorResult("create_task requires workspace_id. Call session_init first.");
            }
            const result = await client.createTask({
              workspace_id: workspaceId,
              project_id: projectId,
              title: input.title,
              content: input.content,
              description: input.description,
              plan_id: input.plan_id ?? undefined,
              plan_step_id: input.plan_step_id,
              status: input.task_status,
              priority: input.priority,
              order: input.order,
              code_refs: input.code_refs,
              tags: input.tags,
            });
            return {
              content: [{ type: "text" as const, text: formatContent(result) }],
              structuredContent: toStructured(result),
            };
          }

          case "get_task": {
            if (!input.task_id) {
              return errorResult("get_task requires: task_id");
            }
            const result = await client.getTask({
              task_id: input.task_id,
            });
            return {
              content: [{ type: "text" as const, text: formatContent(result) }],
              structuredContent: toStructured(result),
            };
          }

          case "update_task": {
            if (!input.task_id) {
              return errorResult("update_task requires: task_id");
            }
            const result = await client.updateTask({
              task_id: input.task_id,
              title: input.title,
              content: input.content,
              description: input.description,
              status: input.task_status,
              priority: input.priority,
              order: input.order,
              plan_id: input.plan_id,
              plan_step_id: input.plan_step_id,
              code_refs: input.code_refs,
              tags: input.tags,
              blocked_reason: input.blocked_reason,
            });
            return {
              content: [{ type: "text" as const, text: formatContent(result) }],
              structuredContent: toStructured(result),
            };
          }

          case "delete_task": {
            if (!input.task_id) {
              return errorResult("delete_task requires: task_id");
            }
            const result = await client.deleteTask({
              task_id: input.task_id,
            });
            return {
              content: [{ type: "text" as const, text: formatContent(result) }],
              structuredContent: toStructured(result),
            };
          }

          case "list_tasks": {
            if (!workspaceId) {
              return errorResult("list_tasks requires workspace_id. Call session_init first.");
            }
            const result = await client.listTasks({
              workspace_id: workspaceId,
              project_id: projectId,
              plan_id: input.plan_id ?? undefined,
              status: input.task_status,
              priority: input.priority,
              limit: input.limit,
            });
            return {
              content: [{ type: "text" as const, text: formatContent(result) }],
              structuredContent: toStructured(result),
            };
          }

          case "reorder_tasks": {
            if (!input.plan_id) {
              return errorResult("reorder_tasks requires: plan_id");
            }
            if (!input.task_ids || input.task_ids.length === 0) {
              return errorResult(
                "reorder_tasks requires: task_ids (array of task IDs in new order)"
              );
            }
            const result = await client.reorderPlanTasks({
              plan_id: input.plan_id,
              task_ids: input.task_ids,
            });
            return {
              content: [{ type: "text" as const, text: formatContent(result) }],
              structuredContent: toStructured(result),
            };
          }

          default:
            return errorResult(`Unknown action: ${input.action}`);
        }
      }
    );

    // -------------------------------------------------------------------------
    // graph - Consolidates code graph analysis tools
    // -------------------------------------------------------------------------
    registerTool(
      "graph",
      {
        title: "Graph",
        description: `Code graph analysis. Actions: dependencies (module deps), impact (change impact), call_path (function call path), related (related nodes), path (path between nodes), decisions (decision history), ingest (build graph), circular_dependencies, unused_code, contradictions.`,
        inputSchema: z.object({
          action: z
            .enum([
              "dependencies",
              "impact",
              "call_path",
              "related",
              "path",
              "decisions",
              "ingest",
              "circular_dependencies",
              "unused_code",
              "contradictions",
            ])
            .describe("Action to perform"),
          workspace_id: z.string().uuid().optional(),
          project_id: z.string().uuid().optional(),
          // ID params
          node_id: z.string().uuid().optional().describe("For related/contradictions"),
          source_id: z.string().uuid().optional().describe("For path"),
          target_id: z.string().uuid().optional().describe("For path"),
          // Target specification
          target: z
            .object({
              type: z.string().describe("module|function|type|variable"),
              id: z.string().describe("Element identifier"),
            })
            .optional()
            .describe("For dependencies/impact"),
          source: z
            .object({
              type: z.string().describe("function"),
              id: z.string().describe("Function identifier"),
            })
            .optional()
            .describe("For call_path"),
          // Options
          max_depth: z.number().optional(),
          include_transitive: z.boolean().optional(),
          limit: z.number().optional(),
          wait: z.boolean().optional().describe("For ingest: wait for completion"),
        }),
      },
      async (input) => {
        const workspaceId = resolveWorkspaceId(input.workspace_id);
        const projectId = resolveProjectId(input.project_id);

        // Check graph tier for gated tools
        const gatedActions = [
          "related",
          "path",
          "decisions",
          "call_path",
          "circular_dependencies",
          "unused_code",
          "ingest",
          "contradictions",
        ];
        if (gatedActions.includes(input.action)) {
          const gate = await gateIfGraphTool(`graph_${input.action}`, input);
          if (gate) return gate;
        }

        switch (input.action) {
          case "dependencies": {
            if (!input.target) {
              return errorResult("dependencies requires: target { type, id }");
            }
            const result = await client.graphDependencies({
              target: input.target,
              max_depth: input.max_depth,
              include_transitive: input.include_transitive,
            });
            const outputText = formatContent(result);
            // Track token savings
            trackToolTokenSavings(client, "graph_dependencies", outputText, {
              workspace_id: workspaceId,
              project_id: projectId,
            });
            return {
              content: [{ type: "text" as const, text: outputText }],
              structuredContent: toStructured(result),
            };
          }

          case "impact": {
            if (!input.target) {
              return errorResult("impact requires: target { type, id }");
            }
            const result = await client.graphImpact({
              target: input.target,
              max_depth: input.max_depth,
            });
            const outputText = formatContent(result);
            // Track token savings
            trackToolTokenSavings(client, "graph_impact", outputText, {
              workspace_id: workspaceId,
              project_id: projectId,
            });
            return {
              content: [{ type: "text" as const, text: outputText }],
              structuredContent: toStructured(result),
            };
          }

          case "call_path": {
            if (!input.source || !input.target) {
              return errorResult("call_path requires: source { type, id }, target { type, id }");
            }
            const result = await client.graphCallPath({
              source: input.source,
              target: input.target,
              max_depth: input.max_depth,
            });
            const outputText = formatContent(result);
            // Track token savings
            trackToolTokenSavings(client, "graph_call_path", outputText, {
              workspace_id: workspaceId,
              project_id: projectId,
            });
            return {
              content: [{ type: "text" as const, text: outputText }],
              structuredContent: toStructured(result),
            };
          }

          case "related": {
            if (!input.node_id) {
              return errorResult("related requires: node_id");
            }
            const result = await client.graphRelated({
              node_id: input.node_id,
              workspace_id: workspaceId,
              project_id: projectId,
              limit: input.limit,
            });
            const outputText = formatContent(result);
            // Track token savings
            trackToolTokenSavings(client, "graph_related", outputText, {
              workspace_id: workspaceId,
              project_id: projectId,
            });
            return {
              content: [{ type: "text" as const, text: outputText }],
              structuredContent: toStructured(result),
            };
          }

          case "path": {
            if (!input.source_id || !input.target_id) {
              return errorResult("path requires: source_id, target_id");
            }
            const result = await client.graphPath({
              source_id: input.source_id,
              target_id: input.target_id,
              workspace_id: workspaceId,
              project_id: projectId,
            });
            return {
              content: [{ type: "text" as const, text: formatContent(result) }],
              structuredContent: toStructured(result),
            };
          }

          case "decisions": {
            const result = await client.graphDecisions({
              workspace_id: workspaceId,
              project_id: projectId,
              limit: input.limit,
            });
            return {
              content: [{ type: "text" as const, text: formatContent(result) }],
              structuredContent: toStructured(result),
            };
          }

          case "ingest": {
            if (!projectId) {
              return errorResult("ingest requires: project_id");
            }
            const result = await client.graphIngest({
              project_id: projectId,
              wait: input.wait,
            });
            return {
              content: [{ type: "text" as const, text: formatContent(result) }],
              structuredContent: toStructured(result),
            };
          }

          case "circular_dependencies": {
            if (!projectId) {
              return errorResult("circular_dependencies requires: project_id");
            }
            const result = await client.findCircularDependencies(projectId);
            return {
              content: [{ type: "text" as const, text: formatContent(result) }],
              structuredContent: toStructured(result),
            };
          }

          case "unused_code": {
            if (!projectId) {
              return errorResult("unused_code requires: project_id");
            }
            const result = await client.findUnusedCode(projectId);
            return {
              content: [{ type: "text" as const, text: formatContent(result) }],
              structuredContent: toStructured(result),
            };
          }

          case "contradictions": {
            if (!input.node_id) {
              return errorResult("contradictions requires: node_id");
            }
            const result = await client.findContradictions(input.node_id);
            return {
              content: [{ type: "text" as const, text: formatContent(result) }],
              structuredContent: toStructured(result),
            };
          }

          default:
            return errorResult(`Unknown action: ${input.action}`);
        }
      }
    );

    // -------------------------------------------------------------------------
    // project - Consolidates project management tools
    // -------------------------------------------------------------------------
    registerTool(
      "project",
      {
        title: "Project",
        description: `Project management. Actions: list, get, create, update, index (trigger indexing), overview, statistics, files, index_status, ingest_local (index local folder).`,
        inputSchema: z.object({
          action: z
            .enum([
              "list",
              "get",
              "create",
              "update",
              "index",
              "overview",
              "statistics",
              "files",
              "index_status",
              "ingest_local",
            ])
            .describe("Action to perform"),
          workspace_id: z.string().uuid().optional(),
          project_id: z.string().uuid().optional(),
          // Create/update params
          name: z.string().optional(),
          description: z.string().optional(),
          folder_path: z.string().optional(),
          generate_editor_rules: z.boolean().optional(),
          // Ingest params
          path: z.string().optional().describe("Local path to ingest"),
          overwrite: z.boolean().optional(),
          write_to_disk: z.boolean().optional(),
          // Pagination
          page: z.number().optional(),
          page_size: z.number().optional(),
        }),
      },
      async (input) => {
        const workspaceId = resolveWorkspaceId(input.workspace_id);
        const projectId = resolveProjectId(input.project_id);

        switch (input.action) {
          case "list": {
            const result = await client.listProjects({
              workspace_id: workspaceId,
              page: input.page,
              page_size: input.page_size,
            });
            return {
              content: [{ type: "text" as const, text: formatContent(result) }],
              structuredContent: toStructured(result),
            };
          }

          case "get": {
            if (!projectId) {
              return errorResult("get requires: project_id");
            }
            const result = await client.getProject(projectId);
            return {
              content: [{ type: "text" as const, text: formatContent(result) }],
              structuredContent: toStructured(result),
            };
          }

          case "create": {
            if (!input.name) {
              return errorResult("create requires: name");
            }
            const result = await client.createProject({
              workspace_id: workspaceId,
              name: input.name,
              description: input.description,
            });
            return {
              content: [{ type: "text" as const, text: formatContent(result) }],
              structuredContent: toStructured(result),
            };
          }

          case "update": {
            if (!projectId) {
              return errorResult("update requires: project_id");
            }
            const result = await client.updateProject(projectId, {
              name: input.name,
              description: input.description,
            });
            return {
              content: [{ type: "text" as const, text: formatContent(result) }],
              structuredContent: toStructured(result),
            };
          }

          case "index": {
            if (!projectId) {
              return errorResult("index requires: project_id");
            }
            const result = await client.indexProject(projectId);
            return {
              content: [{ type: "text" as const, text: formatContent(result) }],
              structuredContent: toStructured(result),
            };
          }

          case "overview": {
            if (!projectId) {
              return errorResult("overview requires: project_id");
            }
            const result = await client.projectOverview(projectId);
            return {
              content: [{ type: "text" as const, text: formatContent(result) }],
              structuredContent: toStructured(result),
            };
          }

          case "statistics": {
            if (!projectId) {
              return errorResult("statistics requires: project_id");
            }
            const result = await client.projectStatistics(projectId);
            return {
              content: [{ type: "text" as const, text: formatContent(result) }],
              structuredContent: toStructured(result),
            };
          }

          case "files": {
            if (!projectId) {
              return errorResult("files requires: project_id");
            }
            const result = await client.projectFiles(projectId);
            return {
              content: [{ type: "text" as const, text: formatContent(result) }],
              structuredContent: toStructured(result),
            };
          }

          case "index_status": {
            if (!projectId) {
              return errorResult("index_status requires: project_id");
            }
            const result = await client.projectIndexStatus(projectId);
            return {
              content: [{ type: "text" as const, text: formatContent(result) }],
              structuredContent: toStructured(result),
            };
          }

          case "ingest_local": {
            if (!input.path) {
              return errorResult("ingest_local requires: path");
            }
            if (!projectId) {
              return errorResult("ingest_local requires: project_id");
            }
            const validPath = await validateReadableDirectory(input.path);
            if (!validPath.ok) {
              return errorResult(validPath.error);
            }
            const ingestOptions = {
              ...(input.write_to_disk !== undefined && { write_to_disk: input.write_to_disk }),
              ...(input.overwrite !== undefined && { overwrite: input.overwrite }),
            };
            startBackgroundIngest(projectId, validPath.resolvedPath, ingestOptions);
            const result = {
              status: "started",
              message: "Ingestion running in background",
              project_id: projectId,
              path: validPath.resolvedPath,
              ...(input.write_to_disk !== undefined && { write_to_disk: input.write_to_disk }),
              ...(input.overwrite !== undefined && { overwrite: input.overwrite }),
              note: "Use 'project' with action 'index_status' to monitor progress.",
            };
            return {
              content: [
                {
                  type: "text" as const,
                  text: `Ingestion started in background for directory: ${validPath.resolvedPath}. Use 'project' with action 'index_status' to monitor progress.`,
                },
              ],
              structuredContent: toStructured(result),
            };
          }

          default:
            return errorResult(`Unknown action: ${input.action}`);
        }
      }
    );

    // -------------------------------------------------------------------------
    // workspace - Consolidates workspace management tools
    // -------------------------------------------------------------------------
    registerTool(
      "workspace",
      {
        title: "Workspace",
        description: `Workspace management. Actions: list, get, associate (link folder to workspace), bootstrap (create workspace and initialize).`,
        inputSchema: z.object({
          action: z.enum(["list", "get", "associate", "bootstrap"]).describe("Action to perform"),
          workspace_id: z.string().uuid().optional(),
          // Associate/bootstrap params
          folder_path: z.string().optional(),
          workspace_name: z.string().optional(),
          create_parent_mapping: z.boolean().optional(),
          generate_editor_rules: z.boolean().optional(),
          // Bootstrap-specific
          description: z.string().optional(),
          visibility: z.enum(["private", "public"]).optional(),
          auto_index: z.boolean().optional(),
          context_hint: z.string().optional(),
          // Pagination
          page: z.number().optional(),
          page_size: z.number().optional(),
        }),
      },
      async (input) => {
        switch (input.action) {
          case "list": {
            const result = await client.listWorkspaces({
              page: input.page,
              page_size: input.page_size,
            });
            return {
              content: [{ type: "text" as const, text: formatContent(result) }],
              structuredContent: toStructured(result),
            };
          }

          case "get": {
            if (!input.workspace_id) {
              return errorResult("get requires: workspace_id");
            }
            const result = await client.getWorkspace(input.workspace_id);
            return {
              content: [{ type: "text" as const, text: formatContent(result) }],
              structuredContent: toStructured(result),
            };
          }

          case "associate": {
            if (!input.folder_path || !input.workspace_id) {
              return errorResult("associate requires: folder_path, workspace_id");
            }
            const result = await client.associateWorkspace({
              folder_path: input.folder_path,
              workspace_id: input.workspace_id,
              workspace_name: input.workspace_name,
              create_parent_mapping: input.create_parent_mapping,
            });
            return {
              content: [{ type: "text" as const, text: formatContent(result) }],
              structuredContent: toStructured(result),
            };
          }

          case "bootstrap": {
            if (!input.workspace_name) {
              return errorResult("bootstrap requires: workspace_name");
            }
            // Bootstrap creates a new workspace and optionally associates it with a folder
            const wsResult = await client.createWorkspace({
              name: input.workspace_name,
              description: input.description,
              visibility: input.visibility,
            });
            const newWorkspaceId = (wsResult as { id?: string })?.id;
            if (!newWorkspaceId) {
              return errorResult("Failed to create workspace during bootstrap");
            }
            // If folder_path provided, associate the workspace with it
            if (input.folder_path) {
              await client.associateWorkspace({
                folder_path: input.folder_path,
                workspace_id: newWorkspaceId,
                workspace_name: input.workspace_name,
                create_parent_mapping: input.create_parent_mapping,
              });
            }
            return {
              content: [{ type: "text" as const, text: formatContent(wsResult) }],
              structuredContent: toStructured(wsResult),
            };
          }

          default:
            return errorResult(`Unknown action: ${input.action}`);
        }
      }
    );

    // -------------------------------------------------------------------------
    // reminder - Consolidates reminder management tools
    // -------------------------------------------------------------------------
    registerTool(
      "reminder",
      {
        title: "Reminder",
        description: `Reminder management. Actions: list, active (pending/overdue), create, snooze, complete, dismiss.`,
        inputSchema: z.object({
          action: z
            .enum(["list", "active", "create", "snooze", "complete", "dismiss"])
            .describe("Action to perform"),
          workspace_id: z.string().uuid().optional(),
          project_id: z.string().uuid().optional(),
          reminder_id: z.string().uuid().optional(),
          // Create params
          title: z.string().optional(),
          content: z.string().optional(),
          remind_at: z.string().optional().describe("ISO 8601 datetime"),
          priority: z.enum(["low", "normal", "high", "urgent"]).optional(),
          recurrence: z.enum(["daily", "weekly", "monthly"]).optional(),
          keywords: z.array(z.string()).optional(),
          // Snooze params
          until: z.string().optional().describe("ISO 8601 datetime"),
          // Filter params
          status: z.enum(["pending", "completed", "dismissed", "snoozed"]).optional(),
          context: z.string().optional(),
          limit: z.number().optional(),
        }),
      },
      async (input) => {
        const workspaceId = resolveWorkspaceId(input.workspace_id);
        const projectId = resolveProjectId(input.project_id);

        switch (input.action) {
          case "list": {
            const result = await client.remindersList({
              workspace_id: workspaceId,
              project_id: projectId,
              status: input.status,
              priority: input.priority,
              limit: input.limit,
            });
            return {
              content: [{ type: "text" as const, text: formatContent(result) }],
              structuredContent: toStructured(result),
            };
          }

          case "active": {
            const result = await client.remindersActive({
              workspace_id: workspaceId,
              project_id: projectId,
              context: input.context,
              limit: input.limit,
            });
            return {
              content: [{ type: "text" as const, text: formatContent(result) }],
              structuredContent: toStructured(result),
            };
          }

          case "create": {
            if (!input.title || !input.content || !input.remind_at) {
              return errorResult("create requires: title, content, remind_at");
            }
            const result = await client.remindersCreate({
              workspace_id: workspaceId,
              project_id: projectId,
              title: input.title,
              content: input.content,
              remind_at: input.remind_at,
              priority: input.priority,
              recurrence: input.recurrence,
              keywords: input.keywords,
            });
            return {
              content: [{ type: "text" as const, text: formatContent(result) }],
              structuredContent: toStructured(result),
            };
          }

          case "snooze": {
            if (!input.reminder_id || !input.until) {
              return errorResult("snooze requires: reminder_id, until");
            }
            const result = await client.remindersSnooze({
              reminder_id: input.reminder_id,
              until: input.until,
            });
            return {
              content: [{ type: "text" as const, text: formatContent(result) }],
              structuredContent: toStructured(result),
            };
          }

          case "complete": {
            if (!input.reminder_id) {
              return errorResult("complete requires: reminder_id");
            }
            const result = await client.remindersComplete({
              reminder_id: input.reminder_id,
            });
            return {
              content: [{ type: "text" as const, text: formatContent(result) }],
              structuredContent: toStructured(result),
            };
          }

          case "dismiss": {
            if (!input.reminder_id) {
              return errorResult("dismiss requires: reminder_id");
            }
            const result = await client.remindersDismiss({
              reminder_id: input.reminder_id,
            });
            return {
              content: [{ type: "text" as const, text: formatContent(result) }],
              structuredContent: toStructured(result),
            };
          }

          default:
            return errorResult(`Unknown action: ${input.action}`);
        }
      }
    );

    // -------------------------------------------------------------------------
    // integration - Consolidates Slack/GitHub/Notion/cross-integration tools
    // -------------------------------------------------------------------------
    registerTool(
      "integration",
      {
        title: "Integration",
        description: `Integration operations for Slack, GitHub, and Notion. Provider: slack, github, notion, all. Actions: status, search, stats, activity, contributors, knowledge, summary, channels (slack), discussions (slack), repos (github), issues (github), create_page (notion), create_database (notion), list_databases (notion), search_pages (notion with smart type detection - filter by event_type, status, priority, has_due_date, tags), get_page (notion), query_database (notion), update_page (notion).`,
        inputSchema: z.object({
          provider: z.enum(["slack", "github", "notion", "all"]).describe("Integration provider"),
          action: z
            .enum([
              "status",
              "search",
              "stats",
              "activity",
              "contributors",
              "knowledge",
              "summary",
              "channels",
              "discussions",
              "sync_users",
              "repos",
              "issues",
              // Notion-specific actions
              "create_page",
              "create_database",
              "list_databases",
              "search_pages",
              "get_page",
              "query_database",
              "update_page",
            ])
            .describe("Action to perform"),
          workspace_id: z.string().uuid().optional(),
          project_id: z.string().uuid().optional(),
          query: z.string().optional(),
          limit: z.number().optional(),
          since: z.string().optional(),
          until: z.string().optional(),
          // Notion-specific parameters
          title: z.string().optional().describe("Page/database title (for Notion create_page/update_page/create_database)"),
          content: z.string().optional().describe("Page content in Markdown (for Notion create_page/update_page)"),
          description: z.string().optional().describe("Database description (for Notion create_database)"),
          parent_database_id: z.string().optional().describe("Parent database ID (for Notion create_page)"),
          parent_page_id: z.string().optional().describe("Parent page ID (for Notion create_page/create_database)"),
          page_id: z.string().optional().describe("Page ID (for Notion get_page/update_page)"),
          database_id: z.string().optional().describe("Database ID (for Notion query_database/search_pages/activity)"),
          days: z.number().optional().describe("Number of days for stats/summary (default: 7)"),
          node_type: z.string().optional().describe("Filter knowledge by type (for Notion knowledge)"),
          filter: z.record(z.unknown()).optional().describe("Query filter (for Notion query_database)"),
          sorts: z.array(z.object({
            property: z.string(),
            direction: z.enum(["ascending", "descending"]),
          })).optional().describe("Sort order (for Notion query_database)"),
          properties: z.record(z.unknown()).optional().describe("Page properties (for Notion update_page)"),
          // Smart type detection filters (for Notion search_pages)
          event_type: z.enum(["NotionTask", "NotionMeeting", "NotionWiki", "NotionBugReport", "NotionFeature", "NotionJournal", "NotionPage"]).optional().describe("Filter by detected content type (for Notion search_pages)"),
          status: z.string().optional().describe("Filter by status property, e.g. 'Done', 'In Progress' (for Notion search_pages)"),
          priority: z.string().optional().describe("Filter by priority property, e.g. 'High', 'Medium', 'Low' (for Notion search_pages)"),
          has_due_date: z.boolean().optional().describe("Filter to pages with or without due dates (for Notion search_pages)"),
          tags: z.string().optional().describe("Filter by tags, comma-separated (for Notion search_pages)"),
        }),
      },
      async (input) => {
        const workspaceId = resolveWorkspaceId(input.workspace_id);
        const projectId = resolveProjectId(input.project_id);

        // Check integration gating
        const integrationGated = await gateIfIntegrationTool(
          input.provider === "slack"
            ? "slack_search"
            : input.provider === "github"
              ? "github_search"
              : input.provider === "notion"
                ? "notion_create_page"
                : "integrations_status"
        );
        if (integrationGated) return integrationGated;

        const params = {
          workspace_id: workspaceId,
          project_id: projectId,
          query: input.query,
          limit: input.limit,
          since: input.since,
          until: input.until,
        };

        switch (input.action) {
          case "status": {
            const result = await client.integrationsStatus(params);
            return {
              content: [{ type: "text" as const, text: formatContent(result) }],
              structuredContent: toStructured(result),
            };
          }

          case "search": {
            if (!input.query) {
              return errorResult("search requires: query");
            }
            if (input.provider === "slack") {
              const result = await client.slackSearch({
                workspace_id: workspaceId,
                q: input.query,
                limit: input.limit,
              });
              return {
                content: [{ type: "text" as const, text: formatContent(result) }],
                structuredContent: toStructured(result),
              };
            } else if (input.provider === "github") {
              const result = await client.githubSearch({
                workspace_id: workspaceId,
                q: input.query,
                limit: input.limit,
              });
              return {
                content: [{ type: "text" as const, text: formatContent(result) }],
                structuredContent: toStructured(result),
              };
            } else {
              const result = await client.integrationsSearch({
                workspace_id: workspaceId,
                query: input.query,
                limit: input.limit,
              });
              return {
                content: [{ type: "text" as const, text: formatContent(result) }],
                structuredContent: toStructured(result),
              };
            }
          }

          case "stats": {
            if (input.provider === "slack") {
              const result = await client.slackStats(params);
              return {
                content: [{ type: "text" as const, text: formatContent(result) }],
                structuredContent: toStructured(result),
              };
            } else if (input.provider === "github") {
              const result = await client.githubStats(params);
              return {
                content: [{ type: "text" as const, text: formatContent(result) }],
                structuredContent: toStructured(result),
              };
            } else if (input.provider === "notion") {
              const result = await client.notionStats({
                workspace_id: workspaceId,
                days: input.days,
              });
              return {
                content: [{ type: "text" as const, text: formatContent(result) }],
                structuredContent: toStructured(result),
              };
            }
            return errorResult("stats requires provider: slack, github, or notion");
          }

          case "activity": {
            if (input.provider === "slack") {
              const result = await client.slackActivity(params);
              return {
                content: [{ type: "text" as const, text: formatContent(result) }],
                structuredContent: toStructured(result),
              };
            } else if (input.provider === "github") {
              const result = await client.githubActivity(params);
              return {
                content: [{ type: "text" as const, text: formatContent(result) }],
                structuredContent: toStructured(result),
              };
            } else if (input.provider === "notion") {
              const result = await client.notionActivity({
                workspace_id: workspaceId,
                limit: input.limit,
                database_id: input.database_id,
              });
              return {
                content: [{ type: "text" as const, text: formatContent(result) }],
                structuredContent: toStructured(result),
              };
            }
            return errorResult("activity requires provider: slack, github, or notion");
          }

          case "contributors": {
            if (input.provider === "slack") {
              const result = await client.slackContributors(params);
              return {
                content: [{ type: "text" as const, text: formatContent(result) }],
                structuredContent: toStructured(result),
              };
            } else if (input.provider === "github") {
              const result = await client.githubContributors(params);
              return {
                content: [{ type: "text" as const, text: formatContent(result) }],
                structuredContent: toStructured(result),
              };
            }
            return errorResult("contributors requires provider: slack or github");
          }

          case "knowledge": {
            if (input.provider === "slack") {
              const result = await client.slackKnowledge(params);
              return {
                content: [{ type: "text" as const, text: formatContent(result) }],
                structuredContent: toStructured(result),
              };
            } else if (input.provider === "github") {
              const result = await client.githubKnowledge(params);
              return {
                content: [{ type: "text" as const, text: formatContent(result) }],
                structuredContent: toStructured(result),
              };
            } else if (input.provider === "notion") {
              const result = await client.notionKnowledge({
                workspace_id: workspaceId,
                limit: input.limit,
                node_type: input.node_type,
              });
              return {
                content: [{ type: "text" as const, text: formatContent(result) }],
                structuredContent: toStructured(result),
              };
            } else {
              const result = await client.integrationsKnowledge(params);
              return {
                content: [{ type: "text" as const, text: formatContent(result) }],
                structuredContent: toStructured(result),
              };
            }
          }

          case "summary": {
            if (input.provider === "slack") {
              const result = await client.slackSummary(params);
              return {
                content: [{ type: "text" as const, text: formatContent(result) }],
                structuredContent: toStructured(result),
              };
            } else if (input.provider === "github") {
              const result = await client.githubSummary(params);
              return {
                content: [{ type: "text" as const, text: formatContent(result) }],
                structuredContent: toStructured(result),
              };
            } else if (input.provider === "notion") {
              const result = await client.notionSummary({
                workspace_id: workspaceId,
                days: input.days,
                database_id: input.database_id,
              });
              return {
                content: [{ type: "text" as const, text: formatContent(result) }],
                structuredContent: toStructured(result),
              };
            } else {
              const result = await client.integrationsSummary(params);
              return {
                content: [{ type: "text" as const, text: formatContent(result) }],
                structuredContent: toStructured(result),
              };
            }
          }

          case "channels": {
            if (input.provider !== "slack") {
              return errorResult("channels is only available for slack provider");
            }
            const result = await client.slackChannels(params);
            return {
              content: [{ type: "text" as const, text: formatContent(result) }],
              structuredContent: toStructured(result),
            };
          }

          case "discussions": {
            if (input.provider !== "slack") {
              return errorResult("discussions is only available for slack provider");
            }
            const result = await client.slackDiscussions(params);
            return {
              content: [{ type: "text" as const, text: formatContent(result) }],
              structuredContent: toStructured(result),
            };
          }

          case "sync_users": {
            if (input.provider !== "slack") {
              return errorResult("sync_users is only available for slack provider");
            }
            const result = await client.slackSyncUsers(params);
            return {
              content: [{ type: "text" as const, text: formatContent(result) }],
              structuredContent: toStructured(result),
            };
          }

          case "repos": {
            if (input.provider !== "github") {
              return errorResult("repos is only available for github provider");
            }
            const result = await client.githubRepos(params);
            return {
              content: [{ type: "text" as const, text: formatContent(result) }],
              structuredContent: toStructured(result),
            };
          }

          case "issues": {
            if (input.provider !== "github") {
              return errorResult("issues is only available for github provider");
            }
            const result = await client.githubIssues(params);
            return {
              content: [{ type: "text" as const, text: formatContent(result) }],
              structuredContent: toStructured(result),
            };
          }

          case "create_page": {
            if (input.provider !== "notion") {
              return errorResult("create_page is only available for notion provider");
            }
            if (!input.title) {
              return errorResult("title is required for create_page action");
            }
            if (!workspaceId) {
              return errorResult(
                "Error: workspace_id is required. Please call session_init first or provide workspace_id explicitly."
              );
            }
            const result = await client.createNotionPage({
              workspace_id: workspaceId,
              project_id: projectId,
              title: input.title,
              content: input.content,
              parent_database_id: input.parent_database_id,
              parent_page_id: input.parent_page_id,
            });
            return {
              content: [
                {
                  type: "text" as const,
                  text: `Page created successfully!\n\nTitle: ${result.title}\nURL: ${result.url}\nID: ${result.id}\nCreated: ${result.created_time}`,
                },
              ],
              structuredContent: toStructured(result),
            };
          }

          case "create_database": {
            if (input.provider !== "notion") {
              return errorResult("create_database is only available for notion provider");
            }
            if (!input.title) {
              return errorResult("title is required for create_database action");
            }
            if (!input.parent_page_id) {
              return errorResult("parent_page_id is required for create_database action");
            }
            if (!workspaceId) {
              return errorResult(
                "Error: workspace_id is required. Please call session_init first or provide workspace_id explicitly."
              );
            }
            const newDatabase = await client.notionCreateDatabase({
              workspace_id: workspaceId,
              title: input.title,
              parent_page_id: input.parent_page_id,
              description: input.description,
            });
            return {
              content: [
                {
                  type: "text" as const,
                  text: `Created database "${newDatabase.title}"\nID: ${newDatabase.id}\nURL: ${newDatabase.url}`,
                },
              ],
              structuredContent: toStructured(newDatabase),
            };
          }

          case "list_databases": {
            if (input.provider !== "notion") {
              return errorResult("list_databases is only available for notion provider");
            }
            if (!workspaceId) {
              return errorResult(
                "Error: workspace_id is required. Please call session_init first or provide workspace_id explicitly."
              );
            }
            const databases = await client.notionListDatabases({
              workspace_id: workspaceId,
            });
            return {
              content: [{ type: "text" as const, text: formatContent(databases) }],
              structuredContent: toStructured(databases),
            };
          }

          case "search_pages": {
            if (input.provider !== "notion") {
              return errorResult("search_pages is only available for notion provider");
            }
            if (!workspaceId) {
              return errorResult(
                "Error: workspace_id is required. Please call session_init first or provide workspace_id explicitly."
              );
            }
            const pages = await client.notionSearchPages({
              workspace_id: workspaceId,
              query: input.query,
              database_id: input.database_id,
              limit: input.limit,
              event_type: input.event_type,
              status: input.status,
              priority: input.priority,
              has_due_date: input.has_due_date,
              tags: input.tags,
            });
            return {
              content: [{ type: "text" as const, text: formatContent(pages) }],
              structuredContent: toStructured(pages),
            };
          }

          case "get_page": {
            if (input.provider !== "notion") {
              return errorResult("get_page is only available for notion provider");
            }
            if (!input.page_id) {
              return errorResult("page_id is required for get_page action");
            }
            if (!workspaceId) {
              return errorResult(
                "Error: workspace_id is required. Please call session_init first or provide workspace_id explicitly."
              );
            }
            const page = await client.notionGetPage({
              workspace_id: workspaceId,
              page_id: input.page_id,
            });
            return {
              content: [
                {
                  type: "text" as const,
                  text: `# ${page.title}\n\nID: ${page.id}\nURL: ${page.url}\nCreated: ${page.created_time}\nLast edited: ${page.last_edited_time}\n\n---\n\n${page.content}`,
                },
              ],
              structuredContent: toStructured(page),
            };
          }

          case "query_database": {
            if (input.provider !== "notion") {
              return errorResult("query_database is only available for notion provider");
            }
            if (!input.database_id) {
              return errorResult("database_id is required for query_database action");
            }
            if (!workspaceId) {
              return errorResult(
                "Error: workspace_id is required. Please call session_init first or provide workspace_id explicitly."
              );
            }
            const queryResult = await client.notionQueryDatabase({
              workspace_id: workspaceId,
              database_id: input.database_id,
              filter: input.filter,
              sorts: input.sorts,
              limit: input.limit,
            });
            return {
              content: [{ type: "text" as const, text: formatContent(queryResult) }],
              structuredContent: toStructured(queryResult),
            };
          }

          case "update_page": {
            if (input.provider !== "notion") {
              return errorResult("update_page is only available for notion provider");
            }
            if (!input.page_id) {
              return errorResult("page_id is required for update_page action");
            }
            if (!workspaceId) {
              return errorResult(
                "Error: workspace_id is required. Please call session_init first or provide workspace_id explicitly."
              );
            }
            const updatedPage = await client.notionUpdatePage({
              workspace_id: workspaceId,
              page_id: input.page_id,
              title: input.title,
              content: input.content,
              properties: input.properties,
            });
            return {
              content: [
                {
                  type: "text" as const,
                  text: `Page updated successfully!\n\nTitle: ${updatedPage.title}\nURL: ${updatedPage.url}\nID: ${updatedPage.id}\nLast edited: ${updatedPage.last_edited_time}`,
                },
              ],
              structuredContent: toStructured(updatedPage),
            };
          }

          default:
            return errorResult(`Unknown action: ${input.action}`);
        }
      }
    );

    // -------------------------------------------------------------------------
    // help - Consolidates utility and help tools
    // -------------------------------------------------------------------------
    registerTool(
      "help",
      {
        title: "Help",
        description: `Utility and help. Actions: tools (list available tools), auth (current user), version (server version), editor_rules (generate AI editor rules), enable_bundle (enable tool bundle in progressive mode).`,
        inputSchema: z.object({
          action: z
            .enum(["tools", "auth", "version", "editor_rules", "enable_bundle"])
            .describe("Action to perform"),
          // For tools
          format: z.enum(["grouped", "minimal", "full"]).optional(),
          category: z.string().optional(),
          // For editor_rules
          folder_path: z.string().optional(),
          editors: z.array(z.string()).optional(),
          mode: z.enum(["minimal", "full"]).optional(),
          dry_run: z.boolean().optional(),
          workspace_id: z.string().uuid().optional(),
          workspace_name: z.string().optional(),
          project_name: z.string().optional(),
          additional_rules: z.string().optional(),
          // For enable_bundle
          bundle: z
            .enum([
              "session",
              "memory",
              "search",
              "graph",
              "workspace",
              "project",
              "reminders",
              "integrations",
            ])
            .optional(),
          list_bundles: z.boolean().optional(),
        }),
      },
      async (input) => {
        switch (input.action) {
          case "tools": {
            const format = (input.format || "grouped") as CatalogFormat;
            const catalog = generateToolCatalog(format, input.category);

            // In consolidated mode, also show domain tools info
            const consolidatedInfo = CONSOLIDATED_MODE
              ? `\n\n[Consolidated Mode]\nDomain tools: ${Array.from(CONSOLIDATED_TOOLS).join(", ")}\nEach domain tool has an 'action' parameter for specific operations.`
              : "";

            return {
              content: [{ type: "text" as const, text: catalog + consolidatedInfo }],
              structuredContent: {
                format,
                catalog,
                consolidated_mode: CONSOLIDATED_MODE,
                domain_tools: CONSOLIDATED_MODE ? Array.from(CONSOLIDATED_TOOLS) : undefined,
              },
            };
          }

          case "auth": {
            const result = await client.me();
            return {
              content: [{ type: "text" as const, text: formatContent(result) }],
              structuredContent: toStructured(result),
            };
          }

          case "version": {
            const result = { name: "contextstream-mcp", version: VERSION };
            return {
              content: [{ type: "text" as const, text: formatContent(result) }],
              structuredContent: toStructured(result),
            };
          }

          case "editor_rules": {
            // Generate rule files content for all supported editors
            const result = generateAllRuleFiles({
              workspaceId: input.workspace_id,
              workspaceName: input.workspace_name,
              projectName: input.project_name,
              additionalRules: input.additional_rules,
              mode: input.mode as "minimal" | "full" | undefined,
            });
            return {
              content: [{ type: "text" as const, text: formatContent(result) }],
              structuredContent: toStructured(result),
            };
          }

          case "enable_bundle": {
            if (input.list_bundles) {
              const bundles = getBundleInfo();
              const result = {
                progressive_mode: PROGRESSIVE_MODE,
                consolidated_mode: CONSOLIDATED_MODE,
                bundles,
                hint: CONSOLIDATED_MODE
                  ? "Consolidated mode is enabled. All operations are available via domain tools."
                  : PROGRESSIVE_MODE
                    ? "Progressive mode is enabled. Use enable_bundle to unlock additional tools."
                    : "Neither progressive nor consolidated mode is enabled.",
              };
              return {
                content: [{ type: "text" as const, text: formatContent(result) }],
                structuredContent: toStructured(result),
              };
            }

            if (!input.bundle) {
              return errorResult("enable_bundle requires: bundle (or use list_bundles: true)");
            }

            if (CONSOLIDATED_MODE) {
              return {
                content: [
                  {
                    type: "text" as const,
                    text: "Consolidated mode is enabled. All operations are available via domain tools (search, session, memory, graph, project, workspace, reminder, integration, help).",
                  },
                ],
              };
            }

            if (!PROGRESSIVE_MODE) {
              return {
                content: [
                  {
                    type: "text" as const,
                    text: "Progressive mode is not enabled. All tools from your toolset are already available.",
                  },
                ],
              };
            }

            const result = enableBundle(input.bundle);
            return {
              content: [{ type: "text" as const, text: formatContent(result) }],
              structuredContent: toStructured(result),
            };
          }

          default:
            return errorResult(`Unknown action: ${input.action}`);
        }
      }
    );

    console.error(
      `[ContextStream] Consolidated mode: Registered ${CONSOLIDATED_TOOLS.size} domain tools.`
    );
  }

  // =============================================================================
  // END CONSOLIDATED DOMAIN TOOLS
  // =============================================================================
}

/**
 * Register minimal tools for limited mode (no credentials).
 * This exposes only a setup help tool so users know how to configure the server.
 */
export function registerLimitedTools(server: McpServer): void {
  server.registerTool(
    "contextstream_setup",
    {
      title: "ContextStream Setup Required",
      description: "ContextStream is not configured. Call this tool for setup instructions.",
      inputSchema: { type: "object" as const, properties: {} },
    },
    async () => {
      return {
        content: [
          {
            type: "text" as const,
            text: `ContextStream: API key not configured.

To set up (creates key + configures your editor):
  npx -y @contextstream/mcp-server setup

This will:
- Start a 5-day Pro trial
- Auto-configure your editor's MCP settings
- Write rules files for better AI assistance

Preview first:
  npx -y @contextstream/mcp-server setup --dry-run

After setup, restart your editor to enable all ContextStream tools.`,
          },
        ],
      };
    }
  );

  console.error("[ContextStream] Limited mode: Registered setup helper tool only.");
}
