import { describe, it, expect, vi } from "vitest";
import { trackToolTokenSavings } from "./token-savings.js";

describe("trackToolTokenSavings", () => {
  it("records context_chars and estimates candidate_chars with multiplier + overhead", () => {
    const client = {
      trackTokenSavings: vi.fn((_payload: any) => Promise.resolve({})),
    };

    trackToolTokenSavings(
      client,
      "search_hybrid",
      "hello",
      { workspace_id: "ws-123", project_id: "proj-456", max_tokens: 2000 },
      { output_format: "full" }
    );

    expect(client.trackTokenSavings).toHaveBeenCalledTimes(1);
    const payload = client.trackTokenSavings.mock.calls[0]?.[0] as any;
    expect(payload.tool).toBe("search_hybrid");
    expect(payload.context_chars).toBe(5);
    expect(payload.candidate_chars).toBe(520);
    expect(payload.workspace_id).toBe("ws-123");
    expect(payload.project_id).toBe("proj-456");
    expect(payload.max_tokens).toBe(2000);
    expect(payload.metadata).toMatchObject({
      method: "multiplier_estimate",
      source: "mcp-server",
      multiplier: 4.0,
      base_overhead_chars: 500,
      output_format: "full",
    });
  });

  it("does not add overhead when context is empty", () => {
    const client = {
      trackTokenSavings: vi.fn((_payload: any) => Promise.resolve({})),
    };

    trackToolTokenSavings(client, "search_hybrid", "");

    expect(client.trackTokenSavings).toHaveBeenCalledTimes(1);
    const payload = client.trackTokenSavings.mock.calls[0]?.[0] as any;
    expect(payload.context_chars).toBe(0);
    expect(payload.candidate_chars).toBe(0);
    expect(payload.metadata).toMatchObject({
      base_overhead_chars: 0,
    });
  });
});
