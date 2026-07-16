import { describe, expect, it } from "vitest";
import { buildMcpVersionInfo, extractTaskStatus, taskUpdateIsStatusOnly } from "./tools.js";

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
      version: "0.4.81",
      latest_version: "0.5.36",
      release_notes: ["Clear task no-op responses"],
      release_url: "https://github.com/contextstream/mcp-server/releases",
    });
  });
});
