import { describe, it, expect } from "vitest";
import { degenerateTitleReason, collectDegenerateStepTitles } from "./tools.js";

describe("degenerateTitleReason", () => {
  it("accepts short action-oriented titles", () => {
    expect(degenerateTitleReason("Wire write paths through scope resolution")).toBeNull();
    expect(degenerateTitleReason("Fix src/tools.ts error handling")).toBeNull();
  });

  it("rejects empty and placeholder titles", () => {
    expect(degenerateTitleReason("")).toBe("empty");
    expect(degenerateTitleReason("   ")).toBe("empty");
    expect(degenerateTitleReason("--")).toBe("placeholder dashes");
    expect(degenerateTitleReason(undefined)).toBe("missing");
  });

  it("rejects multi-line and overlong titles", () => {
    expect(degenerateTitleReason("line one\nline two")).toBe("multi-line");
    expect(degenerateTitleReason("x".repeat(201))).toContain("too long");
  });

  it("rejects bare file paths", () => {
    expect(degenerateTitleReason("src/tools.ts")).toBe("bare file path");
    expect(degenerateTitleReason("crates/mcp-tools/src/domains/scope.rs")).toBe("bare file path");
    expect(degenerateTitleReason("index.test.ts")).toBe("bare file path");
  });
});

describe("collectDegenerateStepTitles", () => {
  it("returns null for valid steps or non-arrays", () => {
    expect(collectDegenerateStepTitles(undefined)).toBeNull();
    expect(
      collectDegenerateStepTitles([{ title: "Do the thing" }, { title: "Verify the thing" }])
    ).toBeNull();
  });

  it("lists every offending step with its reason", () => {
    const message = collectDegenerateStepTitles([
      { title: "Fine title" },
      { title: "--" },
      { title: "src/scope.ts" },
    ]);
    expect(message).toContain("step 2: placeholder dashes");
    expect(message).toContain("step 3: bare file path");
    expect(message).not.toContain("step 1");
  });
});
