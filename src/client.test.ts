import { afterEach, describe, expect, it, vi } from "vitest";
import { ContextStreamClient } from "./client.js";
import { HttpError } from "./http.js";

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

describe("ContextStreamClient.projectFiles", () => {
  afterEach(() => {
    vi.restoreAllMocks();
  });

  it("forwards pagination and filter query params", async () => {
    const client = new ContextStreamClient(baseConfig);
    const fetchMock = vi.spyOn(globalThis, "fetch" as any).mockResolvedValue({
      ok: true,
      status: 200,
      headers: { get: () => "application/json" },
      json: async () => ({ data: [] }),
    } as any);

    await client.projectFiles("11111111-1111-4111-8111-111111111111", {
      page: 3,
      page_size: 100,
      sort_by: "path",
      sort_order: "asc",
      path_pattern: "src/**",
    });

    const url = String(fetchMock.mock.calls[0]?.[0] ?? "");
    expect(url).toContain("/projects/11111111-1111-4111-8111-111111111111/files?");
    expect(url).toContain("page=3");
    expect(url).toContain("page_size=100");
    expect(url).toContain("sort_by=path");
    expect(url).toContain("sort_order=asc");
    expect(url).toContain("path_pattern=src%2F**");
  });
});

describe("ContextStreamClient.createSkill", () => {
  afterEach(() => {
    vi.restoreAllMocks();
  });

  it("defaults new skills to active status when none is provided", async () => {
    const client = new ContextStreamClient(baseConfig);
    const fetchMock = vi.spyOn(globalThis, "fetch" as any).mockResolvedValue({
      ok: true,
      status: 200,
      headers: { get: () => "application/json" },
      json: async () => ({ id: "skill-1" }),
    } as any);

    await client.createSkill({
      name: "deploy-checker",
      title: "Deploy Checker",
      instruction_body: "Run the deploy checks.",
    });

    expect(fetchMock).toHaveBeenCalledTimes(1);
    const init = fetchMock.mock.calls[0]?.[1] as RequestInit | undefined;
    const body = JSON.parse(String(init?.body ?? "{}"));
    expect(body.status).toBe("active");
  });

  it("honors an explicit draft status instead of defaulting to active", async () => {
    const client = new ContextStreamClient(baseConfig);
    const fetchMock = vi.spyOn(globalThis, "fetch" as any).mockResolvedValue({
      ok: true,
      status: 200,
      headers: { get: () => "application/json" },
      json: async () => ({ id: "skill-2" }),
    } as any);

    await client.createSkill({
      name: "draft-skill",
      title: "Draft Skill",
      instruction_body: "Not ready yet.",
      status: "draft",
    });

    expect(fetchMock).toHaveBeenCalledTimes(1);
    const init = fetchMock.mock.calls[0]?.[1] as RequestInit | undefined;
    const body = JSON.parse(String(init?.body ?? "{}"));
    expect(body.status).toBe("draft");
  });

  it("honors an explicit archived status instead of defaulting to active", async () => {
    const client = new ContextStreamClient(baseConfig);
    const fetchMock = vi.spyOn(globalThis, "fetch" as any).mockResolvedValue({
      ok: true,
      status: 200,
      headers: { get: () => "application/json" },
      json: async () => ({ id: "skill-archived" }),
    } as any);

    await client.createSkill({
      name: "archived-skill",
      title: "Archived Skill",
      instruction_body: "Retired skill.",
      status: "archived",
    });

    expect(fetchMock).toHaveBeenCalledTimes(1);
    const init = fetchMock.mock.calls[0]?.[1] as RequestInit | undefined;
    const body = JSON.parse(String(init?.body ?? "{}"));
    expect(body.status).toBe("archived");
  });

  it("defaults to active even when status is explicitly undefined (tool handler path)", async () => {
    // The skill(create) tool handler always forwards `status: input.status`,
    // which is `undefined` when the caller omits it. A naive
    // `{ status: "active", ...params }` spread lets that `undefined` override
    // the default, and JSON.stringify then drops the field entirely — so the
    // API never receives a status and falls back to "Draft". This test guards
    // the real MCP tool path.
    const client = new ContextStreamClient(baseConfig);
    const fetchMock = vi.spyOn(globalThis, "fetch" as any).mockResolvedValue({
      ok: true,
      status: 200,
      headers: { get: () => "application/json" },
      json: async () => ({ id: "skill-3" }),
    } as any);

    await client.createSkill({
      name: "active-by-default",
      title: "Active By Default",
      instruction_body: "Should be active.",
      status: undefined,
    });

    expect(fetchMock).toHaveBeenCalledTimes(1);
    const init = fetchMock.mock.calls[0]?.[1] as RequestInit | undefined;
    const body = JSON.parse(String(init?.body ?? "{}"));
    expect(body.status).toBe("active");
  });

  it("defaults to active when status is an empty or whitespace-only string", async () => {
    // The backend treats a missing status as "Draft", so an empty/whitespace
    // status must be normalized to "active" rather than forwarded as-is.
    const client = new ContextStreamClient(baseConfig);
    const fetchMock = vi.spyOn(globalThis, "fetch" as any).mockResolvedValue({
      ok: true,
      status: 200,
      headers: { get: () => "application/json" },
      json: async () => ({ id: "skill-empty" }),
    } as any);

    await client.createSkill({
      name: "empty-status",
      title: "Empty Status",
      instruction_body: "Should be active.",
      status: "   ",
    });

    expect(fetchMock).toHaveBeenCalledTimes(1);
    const init = fetchMock.mock.calls[0]?.[1] as RequestInit | undefined;
    const body = JSON.parse(String(init?.body ?? "{}"));
    expect(body.status).toBe("active");
  });
});

describe("ContextStreamClient ingest/session fallbacks", () => {
  afterEach(() => {
    vi.restoreAllMocks();
  });

  it("splits ingest batches on 413 and indexes smaller batches", async () => {
    const client = new ContextStreamClient(baseConfig);
    const ingestSpy = vi
      .spyOn(client, "ingestFiles")
      .mockImplementation(async (_projectId, batch) => {
        if (batch.length > 1) {
          throw new HttpError(413, "payload too large");
        }
        return { data: { files_indexed: 1, files_skipped: 0, status: "completed" } } as any;
      });

    const result = await client.ingestFilesAdaptive("11111111-1111-4111-8111-111111111111", [
      { path: "a.ts", content: "export const a = 1;" },
      { path: "b.ts", content: "export const b = 2;" },
    ]);

    expect(ingestSpy).toHaveBeenCalledTimes(3);
    expect(result.data.files_indexed).toBe(2);
  });

  it("falls back to listMemoryEvents when memorySearch errors for lessons", async () => {
    const client = new ContextStreamClient(baseConfig);
    vi.spyOn(client, "memorySearch").mockRejectedValue(new Error("search unavailable"));
    vi.spyOn(client, "listMemoryEvents").mockResolvedValue({
      items: [
        {
          title: "Retry with fallback",
          content: "### Prevention\nAlways run fallback.",
          metadata: { tags: ["lesson"] },
        },
      ],
    } as any);

    const lessons = await client.getHighPriorityLessons({
      workspace_id: "11111111-1111-4111-8111-111111111111",
      limit: 3,
    });

    expect(lessons).toHaveLength(1);
    expect(lessons[0]?.title).toBe("Retry with fallback");
  });
});
