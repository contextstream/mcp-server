/**
 * `contextstream-mcp doctor` — read-only diagnostics for every local
 * surface: auth, API reachability, folder scope binding, local index
 * health, editor rule files, and installed hooks. Prints ✓/✗ lines with a
 * next-step hint for each failure.
 */
import * as fsSync from "node:fs";
import * as path from "node:path";
import { loadConfig } from "./config.js";
import { ContextStreamClient } from "./client.js";
import { VERSION } from "./version.js";
import { resolveWorkspace } from "./workspace-config.js";
import { readIndexStatus } from "./hooks-config.js";
import {
  readGitHead,
  readGitRemoteIdentity,
  countDirtyFilesSince,
} from "./project-index-utils.js";
import { TEMPLATES } from "./rules-templates.js";

export async function runDoctor(): Promise<void> {
  const cwd = process.cwd();
  const lines: string[] = [`ContextStream MCP doctor v${VERSION}`, ""];
  let failures = 0;
  const ok = (message: string) => lines.push(`  ✓ ${message}`);
  const warn = (message: string, hint?: string) => {
    failures += 1;
    lines.push(`  ✗ ${message}`);
    if (hint) lines.push(`    → ${hint}`);
  };

  // 1. Auth & API reachability. loadConfig throws when no credentials are
  // present — diagnosing exactly that is this command's job, so continue
  // with the local checks either way.
  lines.push("Auth & API");
  let config: ReturnType<typeof loadConfig> | null = null;
  try {
    config = loadConfig();
  } catch {
    config = null;
  }
  if (!config || (!config.apiKey && !config.jwt)) {
    warn(
      "No credentials found (CONTEXTSTREAM_API_KEY / CONTEXTSTREAM_JWT unset).",
      "Set CONTEXTSTREAM_API_KEY, or run `contextstream-mcp setup`."
    );
  } else {
    ok(`Credentials present (${config.apiKey ? "API key" : "JWT"}).`);
    try {
      const client = new ContextStreamClient(config);
      const plan = await client.getPlanName();
      ok(`API reachable — plan: ${plan}.`);
    } catch (error) {
      warn(
        `API not reachable: ${error instanceof Error ? error.message : String(error)}`,
        `Check CONTEXTSTREAM_API_URL (${config.apiUrl}).`
      );
    }
  }

  // 2. Folder scope binding
  lines.push("", `Folder scope (${cwd})`);
  const resolved = resolveWorkspace(cwd);
  if (resolved.config?.workspace_id) {
    const scopeLabel = `${resolved.config.workspace_name || resolved.config.workspace_id}${
      resolved.config.project_name ? ` / ${resolved.config.project_name}` : ""
    }`;
    ok(`Workspace bound via ${resolved.source}: ${scopeLabel}.`);
  } else {
    warn(
      "No workspace binding for this folder.",
      'Run init(folder_path="...") from your MCP client, or `contextstream-mcp setup`.'
    );
  }

  // 3. Local index health
  lines.push("", "Local index");
  const status = await readIndexStatus();
  const entry = status.projects?.[path.resolve(cwd)];
  if (!entry) {
    warn(
      "No local index record for this folder.",
      'Search still works via the API; run project(action="ingest_local") to enable local freshness tracking.'
    );
  } else {
    const indexedAtMs = entry.indexed_at ? Date.parse(entry.indexed_at) : NaN;
    const ageHours = Number.isFinite(indexedAtMs)
      ? Math.floor((Date.now() - indexedAtMs) / 3_600_000)
      : null;
    ok(
      `Indexed ${ageHours !== null ? `${ageHours}h ago` : "(unknown age)"}${
        entry.project_id ? ` → project ${entry.project_id}` : ""
      }.`
    );
    if (entry.git_remote) {
      const currentRemote = await readGitRemoteIdentity(cwd);
      if (currentRemote && currentRemote !== entry.git_remote) {
        warn(
          "Git remote identity differs from the recorded binding.",
          'This folder will not bind to the recorded project (twin gate). Re-run project(action="ingest_local") here if intentional.'
        );
      } else {
        ok("Git remote identity matches the recorded binding.");
      }
    }
    if (entry.git_head) {
      const currentHead = await readGitHead(cwd);
      if (currentHead && currentHead !== entry.git_head) {
        const dirtyCount = await countDirtyFilesSince(
          cwd,
          Number.isFinite(indexedAtMs) ? indexedAtMs : 0
        );
        warn(
          `Git HEAD moved since last ingest${dirtyCount ? ` (+${dirtyCount} dirty file(s))` : ""}.`,
          "IndexKeeper re-ingests in the background; search output carries [INDEX_HEALTH] until it catches up."
        );
      } else {
        ok("Index HEAD matches the working tree.");
      }
    }
  }

  // 4. Editor rule files
  lines.push("", "Editor rules");
  let ruleFilesFound = 0;
  for (const [editor, template] of Object.entries(TEMPLATES)) {
    const filePath = path.join(cwd, template.filename);
    if (!fsSync.existsSync(filePath)) continue;
    ruleFilesFound += 1;
    const content = fsSync.readFileSync(filePath, "utf-8");
    const managed =
      content.includes("contextstream-rules-hash") ||
      content.includes("<contextstream>") ||
      content.includes("<!-- BEGIN ContextStream -->");
    if (managed) {
      ok(`${editor}: ${template.filename} (managed block present).`);
    } else {
      warn(
        `${editor}: ${template.filename} exists without a managed ContextStream block.`,
        "Run generate_editor_rules to install/refresh it."
      );
    }
  }
  if (ruleFilesFound === 0) {
    warn(
      "No editor rule files found in this folder.",
      "Run generate_editor_rules (or `contextstream-mcp setup`)."
    );
  }

  // 5. Hooks
  lines.push("", "Hooks");
  const hookTargets = [
    { name: "Claude Code (project)", file: path.join(cwd, ".claude", "settings.json") },
    { name: "Cursor (project)", file: path.join(cwd, ".cursor", "hooks.json") },
  ];
  let hookConfigsFound = 0;
  for (const target of hookTargets) {
    if (!fsSync.existsSync(target.file)) continue;
    const content = fsSync.readFileSync(target.file, "utf-8");
    if (content.includes("contextstream")) {
      ok(`${target.name}: ContextStream hooks installed.`);
      hookConfigsFound += 1;
    } else {
      warn(
        `${target.name}: config present without ContextStream hooks.`,
        "Re-run `contextstream-mcp setup` to install hooks."
      );
    }
  }
  if (hookConfigsFound === 0) {
    lines.push("  – No project-level hook configs found (hooks are optional; setup installs them).");
  }

  lines.push(
    "",
    failures === 0
      ? "All checks passed."
      : `${failures} issue(s) found — hints above show the next step for each.`
  );
  console.log(lines.join("\n"));
  if (failures > 0) process.exitCode = 1;
}
