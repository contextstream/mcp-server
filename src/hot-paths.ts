import * as fs from "node:fs";
import * as path from "node:path";
import { homedir } from "node:os";

type SignalType = "search_result" | "activity_read" | "activity_edit" | "activity_focus";

interface ScopeProfile {
  paths: Record<string, { score: number; last_seen: number; hits: number }>;
  updated_at: number;
}

interface StoreData {
  version: 1;
  scopes: Record<string, ScopeProfile>;
}

export interface HotPathHintEntry {
  path: string;
  score: number;
  source: "history" | "active";
}

export interface HotPathsHint {
  entries: HotPathHintEntry[];
  confidence: number;
  generated_at: string;
  profile_version: number;
}

const STORE_VERSION = 1 as const;
const STORE_DIR = path.join(homedir(), ".contextstream");
const STORE_FILE = path.join(STORE_DIR, "hot-paths.json");
const MAX_PATHS_PER_SCOPE = 400;
const HALF_LIFE_MS = 3 * 24 * 60 * 60 * 1000;

function clamp(value: number, min: number, max: number): number {
  return Math.max(min, Math.min(max, value));
}

function normalizePathKey(input: string): string {
  return input.replace(/\\/g, "/").trim();
}

function toScopeKey(input: { workspace_id?: string; project_id?: string }): string {
  const workspace = input.workspace_id || "none";
  const project = input.project_id || "none";
  return `${workspace}:${project}`;
}

function signalWeight(signal: SignalType): number {
  switch (signal) {
    case "search_result":
      return 1.8;
    case "activity_edit":
      return 1.4;
    case "activity_focus":
      return 1.2;
    case "activity_read":
    default:
      return 1.0;
  }
}

function looksBroadQuery(query: string): boolean {
  const q = query.trim().toLowerCase();
  if (!q) return true;
  if (q.length <= 2) return true;
  if (/^\*+$/.test(q) || q === "all files" || q === "everything") return true;
  const tokens = q.split(/\s+/).filter(Boolean);
  return tokens.length > 12;
}

export class HotPathStore {
  private data: StoreData = { version: STORE_VERSION, scopes: {} };

  constructor() {
    this.load();
  }

  recordPaths(
    scope: { workspace_id?: string; project_id?: string },
    paths: string[],
    signal: SignalType
  ): void {
    if (paths.length === 0) return;
    const now = Date.now();
    const scopeKey = toScopeKey(scope);
    const profile = this.ensureScope(scopeKey);
    const weight = signalWeight(signal);

    for (const raw of paths) {
      const pathKey = normalizePathKey(raw);
      if (!pathKey) continue;
      const current = profile.paths[pathKey] || { score: 0, last_seen: now, hits: 0 };
      const decayed = this.decayedScore(current.score, current.last_seen, now);
      profile.paths[pathKey] = {
        score: decayed + weight,
        last_seen: now,
        hits: current.hits + 1,
      };
    }

    profile.updated_at = now;
    this.pruneScope(profile);
    this.persist();
  }

  buildHint(input: {
    workspace_id?: string;
    project_id?: string;
    query: string;
    active_paths?: string[];
    limit?: number;
  }): HotPathsHint | undefined {
    const scopeKey = toScopeKey(input);
    const profile = this.data.scopes[scopeKey];
    if (!profile) return undefined;

    const now = Date.now();
    const baseEntries = Object.entries(profile.paths)
      .map(([filePath, entry]) => ({
        path: filePath,
        score: this.decayedScore(entry.score, entry.last_seen, now),
      }))
      .filter((entry) => entry.score > 0.05)
      .sort((a, b) => b.score - a.score);

    const active = (input.active_paths || []).map(normalizePathKey).filter(Boolean);
    const merged = new Map<string, HotPathHintEntry>();
    for (const entry of baseEntries.slice(0, Math.max(12, input.limit || 8))) {
      merged.set(entry.path, {
        path: entry.path,
        score: Number(entry.score.toFixed(4)),
        source: "history",
      });
    }

    for (const activePath of active) {
      const existing = merged.get(activePath);
      if (existing) {
        existing.score = Number((existing.score + 0.9).toFixed(4));
      } else {
        merged.set(activePath, { path: activePath, score: 0.9, source: "active" });
      }
    }

    const limit = clamp(input.limit ?? 8, 1, 12);
    const entries = [...merged.values()].sort((a, b) => b.score - a.score).slice(0, limit);
    if (entries.length === 0) return undefined;

    const scoreSum = entries.reduce((sum, item) => sum + item.score, 0);
    const normalized = clamp(scoreSum / (limit * 2.5), 0, 1);
    const confidencePenalty = looksBroadQuery(input.query) ? 0.55 : 1.0;
    const confidence = Number((normalized * confidencePenalty).toFixed(3));

    return {
      entries,
      confidence,
      generated_at: new Date(now).toISOString(),
      profile_version: STORE_VERSION,
    };
  }

  private decayedScore(score: number, lastSeenMs: number, nowMs: number): number {
    const elapsed = Math.max(0, nowMs - lastSeenMs);
    const decay = Math.pow(0.5, elapsed / HALF_LIFE_MS);
    return score * decay;
  }

  private ensureScope(scopeKey: string): ScopeProfile {
    if (!this.data.scopes[scopeKey]) {
      this.data.scopes[scopeKey] = { paths: {}, updated_at: Date.now() };
    }
    return this.data.scopes[scopeKey];
  }

  private pruneScope(profile: ScopeProfile): void {
    const entries = Object.entries(profile.paths);
    if (entries.length <= MAX_PATHS_PER_SCOPE) return;
    entries
      .sort((a, b) => b[1].score - a[1].score)
      .slice(MAX_PATHS_PER_SCOPE)
      .forEach(([key]) => delete profile.paths[key]);
  }

  private load(): void {
    try {
      if (!fs.existsSync(STORE_FILE)) return;
      const parsed = JSON.parse(fs.readFileSync(STORE_FILE, "utf-8")) as StoreData;
      if (parsed?.version !== STORE_VERSION || !parsed.scopes) return;
      this.data = parsed;
    } catch {
      // Ignore store load errors to keep search path resilient.
    }
  }

  private persist(): void {
    try {
      fs.mkdirSync(STORE_DIR, { recursive: true });
      fs.writeFileSync(STORE_FILE, JSON.stringify(this.data));
    } catch {
      // Ignore persistence failures.
    }
  }
}

export const globalHotPathStore = new HotPathStore();
