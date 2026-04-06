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

  it("preserves lesson event type with lesson tags", async () => {
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
        event_type: "lesson",
        tags: expect.arrayContaining(["lesson", "lesson_system"]),
        metadata: expect.objectContaining({
          original_type: "lesson",
          tags: expect.arrayContaining(["lesson", "lesson_system"]),
        }),
      })
    );
  });
});

describe("ContextStreamClient.docsList", () => {
  afterEach(() => {
    vi.restoreAllMocks();
  });

  it("forwards optional query parameter for server-side doc filtering", async () => {
    const client = new ContextStreamClient(baseConfig);
    const fetchMock = vi.spyOn(globalThis, "fetch" as any).mockResolvedValue({
      ok: true,
      status: 200,
      headers: {
        get: () => "application/json",
      },
      json: async () => ({ items: [] }),
    } as any);

    await client.docsList({
      workspace_id: "11111111-1111-4111-8111-111111111111",
      query: "migration playbook",
      per_page: 10,
    });

    expect(fetchMock).toHaveBeenCalledTimes(1);
    const url = String(fetchMock.mock.calls[0]?.[0] ?? "");
    expect(url).toContain("/docs?");
    expect(url).toContain("query=migration+playbook");
  });
});
