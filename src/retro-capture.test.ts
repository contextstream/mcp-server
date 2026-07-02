import { describe, it, expect } from "vitest";
import {
  buildRetroCaptureContent,
  retroCaptureSourceFromItem,
  dedupeRetroCaptureSources,
} from "./tools.js";

describe("buildRetroCaptureContent", () => {
  it("keeps manual content and appends query + numbered evidence", () => {
    const text = buildRetroCaptureContent("We chose Postgres-style ids.", "id format decision", [
      {
        kind: "decision",
        id: "abc",
        title: "Use ULIDs",
        preview: "ULIDs sort by time",
        created_at: "2026-06-01",
      },
    ]);
    expect(text).toContain("We chose Postgres-style ids.");
    expect(text).toContain("Source query: id format decision");
    expect(text).toContain("1. [decision] Use ULIDs (abc) — 2026-06-01");
    expect(text).toContain("   ULIDs sort by time");
  });

  it("uses the default line when content is omitted", () => {
    const text = buildRetroCaptureContent(undefined, undefined, [
      { kind: "note", title: "A note" },
    ]);
    expect(text).toContain("Retroactive capture assembled from prior ContextStream sources.");
    expect(text).toContain("1. [note] A note");
  });
});

describe("retroCaptureSourceFromItem", () => {
  it("extracts a titled, previewed source with collapsed whitespace", () => {
    const source = retroCaptureSourceFromItem("lesson", {
      id: "e1",
      title: "Verify before deploy",
      content: "line one\n   line two",
      occurred_at: "2026-05-05T00:00:00Z",
      score: 0.91,
    });
    expect(source).toMatchObject({
      kind: "lesson",
      id: "e1",
      title: "Verify before deploy",
      preview: "line one line two",
      created_at: "2026-05-05T00:00:00Z",
      score: 0.91,
    });
  });
});

describe("dedupeRetroCaptureSources", () => {
  it("drops duplicate kind+id pairs, keeps distinct ones", () => {
    const deduped = dedupeRetroCaptureSources([
      { kind: "decision", id: "a", title: "One" },
      { kind: "decision", id: "a", title: "One again" },
      { kind: "decision", id: "b", title: "Two" },
      { kind: "note", title: "untitled-ish" },
      { kind: "note", title: "Untitled-ish" },
    ]);
    expect(deduped).toHaveLength(3);
  });
});
