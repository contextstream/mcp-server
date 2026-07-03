import { describe, it, expect } from "vitest";
import {
  displayTitle,
  isStaleOperationalGroundingItem,
  isLargeContextModel,
} from "./tools.js";

describe("displayTitle", () => {
  it("prefers a real title and never renders bare Untitled", () => {
    expect(displayTitle({ title: "Fix scope drift" })).toBe("Fix scope drift");
    expect(displayTitle({ title: "Untitled", summary: "Scope drift fix" })).toBe(
      "Scope drift fix"
    );
  });

  it("falls back to the first meaningful content line, clipped", () => {
    expect(displayTitle({ content: "\n\nAdopted workspace from project.\nmore" })).toBe(
      "Adopted workspace from project."
    );
    const long = "x".repeat(120);
    expect(displayTitle({ content: long }).length).toBeLessThanOrEqual(80);
  });

  it("falls back to a typed id, never Untitled", () => {
    expect(displayTitle({ event_type: "decision", id: "abcdef12-3456" })).toBe(
      "decision abcdef12"
    );
    expect(displayTitle({})).toBe("item");
  });
});

describe("isStaleOperationalGroundingItem", () => {
  it("only applies to operational kinds", () => {
    expect(isStaleOperationalGroundingItem({ event_type: "decision" })).toBe(false);
    expect(
      isStaleOperationalGroundingItem({ event_type: "decision", occurred_at: "2020-01-01" })
    ).toBe(false);
  });

  it("drops old or age-unknown operational telemetry, keeps fresh", () => {
    expect(isStaleOperationalGroundingItem({ event_type: "operation" })).toBe(true);
    expect(
      isStaleOperationalGroundingItem({
        event_type: "operation",
        occurred_at: "2020-01-01T00:00:00Z",
      })
    ).toBe(true);
    expect(
      isStaleOperationalGroundingItem({
        event_type: "operation",
        occurred_at: new Date().toISOString(),
      })
    ).toBe(false);
  });
});

describe("isLargeContextModel", () => {
  it("recognizes 1M-window model ids", () => {
    expect(isLargeContextModel("claude-opus-4-8")).toBe(true);
    expect(isLargeContextModel("claude-opus-4.8-thinking-high")).toBe(true);
    expect(isLargeContextModel("claude-fable-5")).toBe(true);
    expect(isLargeContextModel("claude-sonnet-5[1m]")).toBe(true);
  });

  it("keeps unknown and older models on the default", () => {
    expect(isLargeContextModel("claude-sonnet-5")).toBe(false);
    expect(isLargeContextModel("claude-haiku-4-5-20251001")).toBe(false);
    expect(isLargeContextModel("")).toBe(false);
  });
});
