import { describe, it, expect } from "vitest";
import {
  allowBroadSearchFallbacks,
  applyPostRankFusion,
  exactQuotedLiteral,
  isExactIdentifierQuery,
  scoreConfidenceBand,
} from "./tools.js";
import { findTwinBindingByRemote } from "./project-index-utils.js";

describe("applyPostRankFusion", () => {
  it("promotes an exact-token match above a higher-scored similar hit", () => {
    const { results, reordered } = applyPostRankFusion(
      [
        { file_path: "src/other.ts", content: "something similar", score: 0.7 },
        { file_path: "src/scope.ts", content: "resolveWriteScope entry", score: 0.62 },
      ],
      "resolveWriteScope"
    );
    expect(reordered).toBe(true);
    expect(results[0].file_path).toBe("src/scope.ts");
  });

  it("is a no-op for short queries and small result sets", () => {
    const single = applyPostRankFusion([{ score: 0.9 }], "resolveWriteScope");
    expect(single.reordered).toBe(false);
    const noTokens = applyPostRankFusion(
      [{ score: 0.9 }, { score: 0.8 }],
      "a b"
    );
    expect(noTokens.reordered).toBe(false);
  });
});

describe("scoreConfidenceBand", () => {
  it("bands scores and handles unknowns", () => {
    expect(scoreConfidenceBand(0.9)).toBe("high");
    expect(scoreConfidenceBand(0.7)).toBe("medium");
    expect(scoreConfidenceBand(0.2)).toBe("low");
    expect(scoreConfidenceBand(undefined)).toBe("unknown");
  });
});

describe("bounded exact-search routing", () => {
  it("recognizes code identifiers and quoted literals", () => {
    expect(isExactIdentifierQuery("resolveWriteScope")).toBe(true);
    expect(isExactIdentifierQuery("SCOPE_INVALID")).toBe(true);
    expect(isExactIdentifierQuery("where is scope resolved")).toBe(false);
    expect(exactQuotedLiteral('"requires_sync_bridge"')).toBe("requires_sync_bridge");
  });

  it("only enables broad fallback for auto-selected non-exact queries", () => {
    expect(allowBroadSearchFallbacks(true, "how does project routing work")).toBe(true);
    expect(allowBroadSearchFallbacks(false, "how does project routing work")).toBe(false);
    expect(allowBroadSearchFallbacks(true, "resolveWriteScope")).toBe(false);
    expect(allowBroadSearchFallbacks(true, '"resolveWriteScope"')).toBe(false);
  });
});

describe("findTwinBindingByRemote", () => {
  const entries: Array<[string, { project_id?: string; git_remote?: string }]> = [
    [
      "/old/place/repo",
      { project_id: "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa", git_remote: "github.com/org/repo" },
    ],
    [
      "/other",
      { project_id: "bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb", git_remote: "github.com/org/other" },
    ],
    ["/no-identity", { project_id: "cccccccc-cccc-4ccc-8ccc-cccccccccccc" }],
  ];

  it("adopts a binding with the same remote identity at a different path", () => {
    const twin = findTwinBindingByRemote("github.com/org/repo", "/new/place/repo", entries);
    expect(twin).toMatchObject({ path: "/old/place/repo" });
  });

  it("never binds without identity or on mismatch", () => {
    expect(findTwinBindingByRemote(null, "/new", entries)).toBeNull();
    expect(findTwinBindingByRemote("github.com/org/unrelated", "/new", entries)).toBeNull();
  });

  it("skips the current folder itself", () => {
    expect(
      findTwinBindingByRemote("github.com/org/repo", "/old/place/repo", entries)
    ).toBeNull();
  });
});
