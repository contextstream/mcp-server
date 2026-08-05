import { describe, expect, it } from "vitest";
import {
  buildMcpVersionInfo,
  extractTaskStatus,
  formatDailyRecaps,
  taskUpdateIsStatusOnly,
} from "./tools.js";

describe("task update idempotency", () => {
  it("recognizes a status-only update", () => {
    expect(taskUpdateIsStatusOnly({ status: "completed" })).toBe(true);
  });

  it("requires a write when another field changes", () => {
    expect(
      taskUpdateIsStatusOnly({
        status: "completed",
        title: "Updated title",
      })
    ).toBe(false);
  });

  it("reads direct and wrapped task statuses", () => {
    expect(extractTaskStatus({ status: "completed" })).toBe("completed");
    expect(extractTaskStatus({ data: { task: { status: "blocked" } } })).toBe("blocked");
  });
});

describe("MCP version information", () => {
  it("preserves release notes and uses the public release URL", () => {
    expect(
      buildMcpVersionInfo("0.4.81", {
        data: {
          latest_version: "0.5.36",
          release_notes: ["Clear task no-op responses"],
        },
      })
    ).toEqual({
      name: "contextstream-mcp",
      runtime_type: "legacy-typescript-mcp",
      version: "0.4.81",
      latest_version: "0.5.36",
      release_notes: ["Clear task no-op responses"],
      release_url: "https://github.com/contextstream/mcp-server/releases",
    });
  });
});

describe("Daily Recap formatting", () => {
  it("shows recap dates, generation timestamps, and headlines", () => {
    expect(
      formatDailyRecaps({
        data: [
          {
            recap_date: "2026-08-04",
            generated_at: "2026-08-05T06:00:00Z",
            headline: "Shipped MCP support fixes",
          },
        ],
      })
    ).toContain(
      "2026-08-04 — generated 2026-08-05T06:00:00Z — Shipped MCP support fixes"
    );
  });

  it("explains the daily schedule and manual trigger when history is empty", () => {
    const output = formatDailyRecaps([]);
    expect(output).toContain("around 23:00");
    expect(output).toContain('session(action="trigger_recap")');
  });
});
