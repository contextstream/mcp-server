type RecordValue = Record<string, unknown>;

export type IndexFreshness = "fresh" | "recent" | "aging" | "stale" | "missing" | "unknown";
export type IndexConfidence = "high" | "medium" | "low";

const INDEX_FRESH_HOURS = 1;
const INDEX_RECENT_HOURS = 24;
const INDEX_STALE_HOURS = 24 * 7;

function asRecord(value: unknown): RecordValue | undefined {
  return value && typeof value === "object" && !Array.isArray(value) ? (value as RecordValue) : undefined;
}

function candidateObjects(result: unknown): RecordValue[] {
  const root = asRecord(result);
  const data = asRecord(root?.data);
  if (data && root) return [data, root];
  if (data) return [data];
  if (root) return [root];
  return [];
}

function readBoolean(candidates: RecordValue[], key: string): boolean | undefined {
  for (const candidate of candidates) {
    const value = candidate[key];
    if (typeof value === "boolean") {
      return value;
    }
  }
  return undefined;
}

function readNumber(candidates: RecordValue[], keys: string[]): number | undefined {
  for (const candidate of candidates) {
    for (const key of keys) {
      const value = candidate[key];
      if (typeof value === "number" && Number.isFinite(value)) {
        return value;
      }
      if (typeof value === "string" && value.trim()) {
        const parsed = Number(value);
        if (Number.isFinite(parsed)) {
          return parsed;
        }
      }
    }
  }
  return undefined;
}

function readString(candidates: RecordValue[], key: string): string | undefined {
  for (const candidate of candidates) {
    const value = candidate[key];
    if (typeof value === "string" && value.trim()) {
      return value.trim();
    }
  }
  return undefined;
}

export function extractIndexTimestamp(result: unknown): Date | undefined {
  const candidates = candidateObjects(result);
  for (const key of ["last_updated", "indexed_at", "last_indexed"]) {
    const raw = readString(candidates, key);
    if (!raw) continue;
    const parsed = new Date(raw);
    if (!Number.isNaN(parsed.getTime())) {
      return parsed;
    }
  }
  return undefined;
}

export function apiResultReportsIndexed(result: unknown): boolean {
  const candidates = candidateObjects(result);

  const indexed = readBoolean(candidates, "indexed");
  if (indexed !== undefined) {
    return indexed;
  }

  const indexedFiles = readNumber(candidates, ["indexed_files", "indexed_file_count"]) ?? 0;
  if (indexedFiles > 0) {
    return true;
  }

  const totalFiles = readNumber(candidates, ["total_files"]) ?? 0;
  if (totalFiles > 0) {
    const status = readString(candidates, "status")?.toLowerCase();
    if (status === "completed" || status === "ready") {
      return true;
    }
  }

  return false;
}

export function apiResultIsIndexing(result: unknown): boolean {
  const candidates = candidateObjects(result);
  const projectIndexState = readString(candidates, "project_index_state")?.toLowerCase();
  if (projectIndexState === "indexing" || projectIndexState === "committing") {
    return true;
  }

  const status = readString(candidates, "status")?.toLowerCase();
  if (status === "indexing" || status === "processing") {
    return true;
  }

  const pendingFiles = readNumber(candidates, ["pending_files"]) ?? 0;
  return pendingFiles > 0;
}

function countFromObject(value: unknown): number | undefined {
  const obj = asRecord(value);
  if (!obj) return undefined;

  if (Array.isArray(obj.entries)) {
    return obj.entries.length;
  }
  if (Array.isArray(obj.history)) {
    return obj.history.length;
  }
  return undefined;
}

export function indexHistoryEntryCount(result: unknown): number {
  const rootCount = countFromObject(result);
  if (typeof rootCount === "number") {
    return rootCount;
  }

  const root = asRecord(result);
  const dataCount = countFromObject(root?.data);
  if (typeof dataCount === "number") {
    return dataCount;
  }

  if (Array.isArray(result)) {
    return result.length;
  }
  if (Array.isArray(root?.data)) {
    return root.data.length;
  }
  return 0;
}

export function classifyIndexFreshness(indexed: boolean, ageHours?: number): IndexFreshness {
  if (!indexed) {
    return "missing";
  }
  if (typeof ageHours !== "number" || Number.isNaN(ageHours)) {
    return "unknown";
  }
  if (ageHours <= INDEX_FRESH_HOURS) {
    return "fresh";
  }
  if (ageHours <= INDEX_RECENT_HOURS) {
    return "recent";
  }
  if (ageHours <= INDEX_STALE_HOURS) {
    return "aging";
  }
  return "stale";
}

export function classifyIndexConfidence(
  indexed: boolean,
  apiIndexed: boolean,
  locallyIndexed: boolean,
  freshness: IndexFreshness
): { confidence: IndexConfidence; reason: string } {
  if (!indexed) {
    return {
      confidence: "low",
      reason: "Neither API status nor local index metadata currently indicates a usable index.",
    };
  }

  if (apiIndexed && locallyIndexed) {
    const reason =
      freshness === "stale"
        ? "API and local metadata agree, but index age indicates stale coverage."
        : "API and local metadata agree for this project scope.";
    return { confidence: "high", reason };
  }

  if (apiIndexed || locallyIndexed) {
    return {
      confidence: "medium",
      reason: "Only one source reports index readiness (API vs local metadata).",
    };
  }

  return {
    confidence: "low",
    reason: "Index state is inferred but lacks corroborating API/local metadata.",
  };
}
