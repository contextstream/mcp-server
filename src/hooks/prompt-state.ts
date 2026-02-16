import * as fs from "node:fs";
import * as path from "node:path";
import { homedir } from "node:os";

type PromptStateEntry = {
  require_context: boolean;
  require_init?: boolean;
  last_context_at?: string;
  last_state_change_at?: string;
  updated_at: string;
};

type PromptStateFile = {
  workspaces: Record<string, PromptStateEntry>;
};

const STATE_PATH = path.join(homedir(), ".contextstream", "prompt-state.json");

function defaultState(): PromptStateFile {
  return { workspaces: {} };
}

function nowIso(): string {
  return new Date().toISOString();
}

function ensureStateDir(): void {
  try {
    fs.mkdirSync(path.dirname(STATE_PATH), { recursive: true });
  } catch {
    // best effort
  }
}

function normalizePath(input: string): string {
  try {
    return path.resolve(input);
  } catch {
    return input;
  }
}

function workspacePathsMatch(a: string, b: string): boolean {
  const left = normalizePath(a);
  const right = normalizePath(b);
  return (
    left === right ||
    left.startsWith(`${right}${path.sep}`) ||
    right.startsWith(`${left}${path.sep}`)
  );
}

function readState(): PromptStateFile {
  try {
    const content = fs.readFileSync(STATE_PATH, "utf8");
    const parsed = JSON.parse(content) as PromptStateFile;
    if (!parsed || typeof parsed !== "object" || !parsed.workspaces) {
      return defaultState();
    }
    return parsed;
  } catch {
    return defaultState();
  }
}

function writeState(state: PromptStateFile): void {
  try {
    ensureStateDir();
    fs.writeFileSync(STATE_PATH, JSON.stringify(state, null, 2), "utf8");
  } catch {
    // best effort
  }
}

function getOrCreateEntry(
  state: PromptStateFile,
  cwd: string
): { key: string; entry: PromptStateEntry } | null {
  if (!cwd.trim()) return null;

  const exact = state.workspaces[cwd];
  if (exact) return { key: cwd, entry: exact };

  for (const [trackedCwd, trackedEntry] of Object.entries(state.workspaces)) {
    if (workspacePathsMatch(trackedCwd, cwd)) {
      return { key: trackedCwd, entry: trackedEntry };
    }
  }

  const created: PromptStateEntry = {
    require_context: false,
    require_init: false,
    last_context_at: undefined,
    last_state_change_at: undefined,
    updated_at: nowIso(),
  };
  state.workspaces[cwd] = created;
  return { key: cwd, entry: created };
}

export function cleanupStale(maxAgeSeconds: number): void {
  const state = readState();
  const now = Date.now();
  let changed = false;

  for (const [cwd, entry] of Object.entries(state.workspaces)) {
    const updated = new Date(entry.updated_at);
    if (Number.isNaN(updated.getTime())) continue;
    const ageSeconds = (now - updated.getTime()) / 1000;
    if (ageSeconds > maxAgeSeconds) {
      delete state.workspaces[cwd];
      changed = true;
    }
  }

  if (changed) {
    writeState(state);
  }
}

export function markContextRequired(cwd: string): void {
  if (!cwd.trim()) return;
  const state = readState();
  const target = getOrCreateEntry(state, cwd);
  if (!target) return;
  target.entry.require_context = true;
  target.entry.updated_at = nowIso();
  writeState(state);
}

export function clearContextRequired(cwd: string): void {
  if (!cwd.trim()) return;
  const state = readState();
  const target = getOrCreateEntry(state, cwd);
  if (!target) return;
  target.entry.require_context = false;
  target.entry.last_context_at = nowIso();
  target.entry.updated_at = nowIso();
  writeState(state);
}

export function isContextRequired(cwd: string): boolean {
  if (!cwd.trim()) return false;
  const state = readState();
  const target = getOrCreateEntry(state, cwd);
  return Boolean(target?.entry.require_context);
}

export function markInitRequired(cwd: string): void {
  if (!cwd.trim()) return;
  const state = readState();
  const target = getOrCreateEntry(state, cwd);
  if (!target) return;
  target.entry.require_init = true;
  target.entry.updated_at = nowIso();
  writeState(state);
}

export function clearInitRequired(cwd: string): void {
  if (!cwd.trim()) return;
  const state = readState();
  const target = getOrCreateEntry(state, cwd);
  if (!target) return;
  target.entry.require_init = false;
  target.entry.updated_at = nowIso();
  writeState(state);
}

export function isInitRequired(cwd: string): boolean {
  if (!cwd.trim()) return false;
  const state = readState();
  const target = getOrCreateEntry(state, cwd);
  return Boolean(target?.entry.require_init);
}

export function markStateChanged(cwd: string): void {
  if (!cwd.trim()) return;
  const state = readState();
  const target = getOrCreateEntry(state, cwd);
  if (!target) return;
  target.entry.last_state_change_at = nowIso();
  target.entry.updated_at = nowIso();
  writeState(state);
}

export function isContextFreshAndClean(cwd: string, maxAgeSeconds: number): boolean {
  if (!cwd.trim()) return false;
  const state = readState();
  const target = getOrCreateEntry(state, cwd);
  const entry = target?.entry;
  if (!entry?.last_context_at) return false;

  const contextAt = new Date(entry.last_context_at);
  if (Number.isNaN(contextAt.getTime())) return false;

  const ageSeconds = (Date.now() - contextAt.getTime()) / 1000;
  if (ageSeconds < 0 || ageSeconds > maxAgeSeconds) return false;

  if (entry.last_state_change_at) {
    const changedAt = new Date(entry.last_state_change_at);
    if (!Number.isNaN(changedAt.getTime()) && changedAt.getTime() > contextAt.getTime()) {
      return false;
    }
  }

  return true;
}
