import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { ContextStreamClient } from "./client.js";
import type { Config } from "./config.js";

const TEST_WORKSPACE_ID = "11111111-1111-4111-8111-111111111111";

describe("ContextStreamClient tag payloads", () => {
  let fetchSpy: ReturnType<typeof vi.spyOn>;
  let client: ContextStreamClient;

  beforeEach(() => {
    fetchSpy = vi.spyOn(globalThis, "fetch").mockResolvedValue(
      new Response(JSON.stringify({ success: true, data: { id: "evt_123" } }), {
        status: 200,
        headers: { "content-type": "application/json" },
      })
    );

    const config: Config = {
      apiUrl: "https://api.contextstream.io",
      apiKey: "test-api-key",
      userAgent: "contextstream-test",
      contextPackEnabled: true,
      showTiming: false,
      toolSurfaceProfile: "default",
    };
    client = new ContextStreamClient(config);
  });

  afterEach(() => {
    vi.restoreAllMocks();
  });

  it("createMemoryEvent sends top-level tags", async () => {
    await client.createMemoryEvent({
      workspace_id: TEST_WORKSPACE_ID,
      event_type: "insight",
      title: "Example event",
      content: "Some content",
      tags: ["category:testing", "priority:high"],
    });

    const [, options] = fetchSpy.mock.calls[0] as [RequestInfo | URL, RequestInit];
    expect(JSON.parse(String(options.body))).toEqual(
      expect.objectContaining({
        workspace_id: TEST_WORKSPACE_ID,
        tags: ["category:testing", "priority:high"],
      })
    );
  });

  it("sessionRemember sends tags as a JSON array", async () => {
    await client.sessionRemember({
      workspace_id: TEST_WORKSPACE_ID,
      content: "Something important",
      tags: ["high_priority", "always_surface"],
    });

    const [, options] = fetchSpy.mock.calls[0] as [RequestInfo | URL, RequestInit];
    expect(JSON.parse(String(options.body))).toEqual(
      expect.objectContaining({
        workspace_id: TEST_WORKSPACE_ID,
        content: "Something important",
        tags: ["high_priority", "always_surface"],
      })
    );
  });
});
