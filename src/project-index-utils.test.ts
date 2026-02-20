import { describe, expect, it } from "vitest";
import {
  apiResultIsIndexing,
  apiResultReportsIndexed,
  classifyIndexConfidence,
  classifyIndexFreshness,
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
