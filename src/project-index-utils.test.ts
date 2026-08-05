import { describe, expect, it } from "vitest";
import {
  apiResultIsIndexing,
  apiIndexResultIsSkipped,
  apiResultReportsIndexed,
  apiAllowsPathIngest,
  classifyRemoteIndexState,
  classifyGraphIngestIndexState,
  classifyIndexConfidence,
  classifyIndexFreshness,
  extractPendingFilePaths,
  extractIndexTimestamp,
  indexHistoryEntryCount,
} from "./project-index-utils.js";

describe("apiResultReportsIndexed", () => {
  it("detects indexed_files field", () => {
    const payload = {
      status: "completed",
      indexed_files: 12,
    };
    expect(apiResultReportsIndexed(payload)).toBe(true);
  });

  it("detects indexed_file_count field", () => {
    const payload = {
      project_index_state: "ready",
      indexed_file_count: 4,
    };
    expect(apiResultReportsIndexed(payload)).toBe(true);
  });

  it("respects explicit indexed=false", () => {
    const payload = {
      indexed: false,
      indexed_files: 99,
    };
    expect(apiResultReportsIndexed(payload)).toBe(false);
  });

  it("preserves canonical readiness when checkout readiness is unconfirmed", () => {
    expect(
      apiResultReportsIndexed({
        indexed: false,
        project_index_state: "ready",
        indexed_file_count: 886,
      })
    ).toBe(true);
  });

  it("does not infer indexed from indexing status alone", () => {
    const payload = {
      status: "indexing",
      total_files: 12,
      indexed_files: 0,
    };
    expect(apiResultReportsIndexed(payload)).toBe(false);
  });

  it("handles nested data payloads", () => {
    const payload = {
      data: {
        indexed_file_count: 8,
      },
    };
    expect(apiResultReportsIndexed(payload)).toBe(true);
  });
});

describe("remote index helpers", () => {
  it("classifies ready, indexing, and missing server state", () => {
    expect(classifyRemoteIndexState({ indexed_files: 2 })).toBe("server_index_ready");
    expect(classifyRemoteIndexState({ status: "processing" })).toBe("server_indexing");
    expect(classifyRemoteIndexState({ indexed_files: 0 })).toBe("requires_sync_bridge");
  });

  it("detects skipped index acknowledgements", () => {
    expect(apiIndexResultIsSkipped({ status: "skipped" })).toBe(true);
    expect(apiIndexResultIsSkipped({ data: { status: "completed" } })).toBe(false);
  });

  it("only delegates workstation paths to loopback APIs without opt-in", () => {
    expect(apiAllowsPathIngest("http://127.0.0.1:3000")).toBe(true);
    expect(apiAllowsPathIngest("http://localhost:8787")).toBe(true);
    expect(apiAllowsPathIngest("http://[::1]:8787")).toBe(true);
    expect(apiAllowsPathIngest("https://api.contextstream.io")).toBe(false);
    expect(apiAllowsPathIngest("https://api.contextstream.io", true)).toBe(true);
  });
});

describe("apiResultIsIndexing", () => {
  it("detects project_index_state indexing", () => {
    const payload = {
      project_index_state: "indexing",
      pending_files: 1,
    };
    expect(apiResultIsIndexing(payload)).toBe(true);
  });

  it("detects status processing", () => {
    const payload = {
      status: "processing",
      pending_files: 1,
    };
    expect(apiResultIsIndexing(payload)).toBe(true);
  });

  it("detects pending files even without status", () => {
    const payload = {
      pending_files: 3,
    };
    expect(apiResultIsIndexing(payload)).toBe(true);
  });
});

describe("extractPendingFilePaths", () => {
  it("reads pending paths from nested payload", () => {
    const payload = {
      data: {
        pending_file_paths: ["lib/a.dart", "lib/b.dart"],
      },
    };
    expect(extractPendingFilePaths(payload)).toEqual(["lib/a.dart", "lib/b.dart"]);
  });

  it("reads legacy pending paths aliases", () => {
    const payload = {
      pending_paths: ["src/main.ts"],
    };
    expect(extractPendingFilePaths(payload)).toEqual(["src/main.ts"]);
  });
});

describe("indexHistoryEntryCount", () => {
  it("reads entries array", () => {
    const payload = {
      entries: [{ file_path: "a.rs" }, { file_path: "b.ts" }],
    };
    expect(indexHistoryEntryCount(payload)).toBe(2);
  });

  it("reads history array", () => {
    const payload = {
      history: [{ file_path: "a.rs" }],
    };
    expect(indexHistoryEntryCount(payload)).toBe(1);
  });

  it("reads legacy array shape", () => {
    const payload = [{ event: 1 }, { event: 2 }, { event: 3 }];
    expect(indexHistoryEntryCount(payload)).toBe(3);
  });

  it("reads nested data entries", () => {
    const payload = {
      data: {
        entries: [{ file_path: "a.rs" }, { file_path: "b.rs" }, { file_path: "c.rs" }],
      },
    };
    expect(indexHistoryEntryCount(payload)).toBe(3);
  });
});

describe("index freshness helpers", () => {
  it("classifies freshness and confidence", () => {
    expect(classifyIndexFreshness(false, undefined)).toBe("missing");
    expect(classifyIndexFreshness(true, 2)).toBe("recent");
    const confidence = classifyIndexConfidence(true, true, false, "recent");
    expect(confidence.confidence).toBe("medium");
  });

  it("parses known timestamp keys", () => {
    const parsed = extractIndexTimestamp({ last_updated: "2026-02-20T18:00:00Z" });
    expect(parsed?.toISOString()).toBe("2026-02-20T18:00:00.000Z");
  });
});

describe("classifyGraphIngestIndexState", () => {
  it("returns ready for recent indexed payload", () => {
    const result = classifyGraphIngestIndexState({
      statusResult: {
        indexed_files: 10,
        last_updated: new Date(Date.now() - 20 * 60 * 1000).toISOString(),
        project_index_state: "ready",
      },
      locallyIndexed: false,
    });
    expect(result.state).toBe("ready");
  });

  it("returns stale when project_index_state is stale", () => {
    const result = classifyGraphIngestIndexState({
      statusResult: {
        indexed_files: 10,
        last_updated: new Date().toISOString(),
        project_index_state: "stale",
      },
      locallyIndexed: false,
    });
    expect(result.state).toBe("stale");
    expect(result.projectIndexState).toBe("stale");
  });

  it("returns missing when index cannot be confirmed", () => {
    const result = classifyGraphIngestIndexState({
      statusResult: {
        indexed_files: 0,
        status: "ready",
      },
      locallyIndexed: false,
    });
    expect(result.state).toBe("missing");
  });

  it("returns indexing when status indicates in-progress commit", () => {
    const result = classifyGraphIngestIndexState({
      statusResult: {
        project_index_state: "indexing",
        pending_files: 3,
      },
      locallyIndexed: true,
    });
    expect(result.state).toBe("indexing");
    expect(result.indexInProgress).toBe(true);
  });
});
