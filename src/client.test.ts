import { afterEach, describe, expect, it, vi } from "vitest";
import { ContextStreamClient } from "./client.js";

const baseConfig = {
  apiUrl: "https://api.contextstream.io",
  apiKey: "test-key",
  userAgent: "contextstream-mcp/test",
  contextPackEnabled: true,
  showTiming: false,
  toolSurfaceProfile: "default" as const,
};

describe("ContextStreamClient.captureContext", () => {
  afterEach(() => {
    vi.restoreAllMocks();
  });

  it("preserves decision event types for decision queries", async () => {
    const client = new ContextStreamClient(baseConfig);
    const createMemoryEvent = vi
      .spyOn(client, "createMemoryEvent")
      .mockResolvedValue({ success: true } as any);

    await client.captureContext({
      workspace_id: "11111111-1111-4111-8111-111111111111",
      event_type: "decision",
      title: "Use connection pooling",
      content: "Pooling improves throughput for repeated database work.",
      tags: ["category:testing"],
    });

    expect(createMemoryEvent).toHaveBeenCalledWith(
      expect.objectContaining({
        event_type: "decision",
        tags: expect.arrayContaining(["category:testing", "decision"]),
        metadata: expect.objectContaining({
          original_type: "decision",
          tags: expect.arrayContaining(["category:testing", "decision"]),
        }),
      })
    );
  });

  it("keeps lesson-system events stored as manual notes with lesson tags", async () => {
    const client = new ContextStreamClient(baseConfig);
    const createMemoryEvent = vi
      .spyOn(client, "createMemoryEvent")
      .mockResolvedValue({ success: true } as any);

    await client.captureContext({
      workspace_id: "11111111-1111-4111-8111-111111111111",
      event_type: "lesson",
      title: "Check pagination first",
      content: "The API paginates by default.",
    });

    expect(createMemoryEvent).toHaveBeenCalledWith(
      expect.objectContaining({
        event_type: "manual_note",
        tags: expect.arrayContaining(["lesson", "lesson_system"]),
        metadata: expect.objectContaining({
          original_type: "lesson",
          tags: expect.arrayContaining(["lesson", "lesson_system"]),
        }),
      })
    );
  });
});
