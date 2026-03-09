import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { ContextStreamClient } from "./client.js";
import type { Config } from "./config.js";

const TEST_TODO_ID = "11111111-1111-4111-8111-111111111111";

describe("ContextStreamClient todos completion", () => {
  let fetchSpy: ReturnType<typeof vi.spyOn>;
  let client: ContextStreamClient;

  beforeEach(() => {
    fetchSpy = vi.spyOn(globalThis, "fetch").mockResolvedValue(
      new Response(JSON.stringify({ success: true, data: { id: TEST_TODO_ID } }), {
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

  it("todosComplete sends completed=true and status=completed", async () => {
    await client.todosComplete({ todo_id: TEST_TODO_ID });

    expect(fetchSpy).toHaveBeenCalledTimes(1);
    const [url, options] = fetchSpy.mock.calls[0] as [RequestInfo | URL, RequestInit];

    expect(String(url)).toContain(`/api/v1/todos/${TEST_TODO_ID}`);
    expect(options.method).toBe("PATCH");
    expect(JSON.parse(String(options.body))).toEqual({
      completed: true,
      status: "completed",
    });
  });

  it("todosIncomplete sends completed=false and status=pending", async () => {
    await client.todosIncomplete({ todo_id: TEST_TODO_ID });

    expect(fetchSpy).toHaveBeenCalledTimes(1);
    const [, options] = fetchSpy.mock.calls[0] as [RequestInfo | URL, RequestInit];

    expect(options.method).toBe("PATCH");
    expect(JSON.parse(String(options.body))).toEqual({
      completed: false,
      status: "pending",
    });
  });
});
