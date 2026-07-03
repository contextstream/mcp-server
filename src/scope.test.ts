import { describe, it, expect, vi } from "vitest";
import { HttpError } from "./http.js";
import { runWithAuthOverride } from "./auth-context.js";
import {
  normalizeScopeUuid,
  resolveProjectScopeId,
  resolveWriteScope,
  recoverWriteScopeAfterProjectError,
  prependScopeNote,
  type ScopeClient,
  type ScopeSession,
} from "./scope.js";

const WS_A = "11111111-1111-4111-8111-111111111111";
const WS_B = "22222222-2222-4222-8222-222222222222";
const WS_C = "33333333-3333-4333-8333-333333333333";
const PROJ_1 = "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa";
const PROJ_2 = "bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb";

function makeClient(overrides: Partial<ScopeClient> = {}): ScopeClient {
  return {
    listProjects: vi.fn(async () => ({ items: [] })),
    getProject: vi.fn(async () => ({})),
    getWorkspace: vi.fn(async () => ({})),
    setDefaults: vi.fn(),
    clearDefaults: vi.fn(),
    ...overrides,
  };
}

function makeSession(
  context: Record<string, unknown> | null,
  folderPath: string | null = null
): ScopeSession & { context: Record<string, unknown> | null } {
  const state = { context };
  return {
    context: state.context,
    getContext: () => state.context,
    getFolderPath: () => folderPath,
    updateScope: vi.fn((input: Record<string, unknown>) => {
      state.context = { ...(state.context || {}), ...input };
    }),
    replaceScope: vi.fn((input: Record<string, unknown>) => {
      const next = { ...(state.context || {}) };
      if (input.workspace_id === null) delete next.workspace_id;
      if (input.project_id === null) delete next.project_id;
      state.context = next;
    }),
  };
}

describe("normalizeScopeUuid", () => {
  it("accepts UUIDs and rejects other strings", () => {
    expect(normalizeScopeUuid(WS_A)).toBe(WS_A);
    expect(normalizeScopeUuid(` ${WS_A.toUpperCase()} `)).toBe(WS_A);
    expect(normalizeScopeUuid("my-project")).toBeUndefined();
    expect(normalizeScopeUuid(42)).toBeUndefined();
  });
});

describe("resolveProjectScopeId", () => {
  it("passes UUIDs through without a lookup", async () => {
    const client = makeClient();
    const result = await resolveProjectScopeId(client, WS_A, PROJ_1);
    expect(result.id).toBe(PROJ_1);
    expect(client.listProjects).not.toHaveBeenCalled();
  });

  it("matches a project name ignoring case, spaces, underscores, and hyphens", async () => {
    const client = makeClient({
      listProjects: vi.fn(async () => ({
        items: [
          { id: PROJ_1, name: "My-Server" },
          { id: PROJ_2, name: "other" },
        ],
      })),
    });
    const result = await resolveProjectScopeId(client, WS_A, "my server");
    expect(result.id).toBe(PROJ_1);
  });

  it("errors with known project names when nothing matches", async () => {
    const client = makeClient({
      listProjects: vi.fn(async () => ({
        items: [{ id: PROJ_1, name: "alpha" }, { id: PROJ_2, name: "beta" }],
      })),
    });
    const result = await resolveProjectScopeId(client, WS_A, "gamma");
    expect(result.id).toBeUndefined();
    expect(result.error).toContain("alpha");
    expect(result.error).toContain("beta");
  });

  it("requires a resolved workspace for name lookups", async () => {
    const client = makeClient();
    const result = await resolveProjectScopeId(client, undefined, "alpha");
    expect(result.error).toContain("workspace");
  });

  it("returns the fallback when no value is provided", async () => {
    const client = makeClient();
    const result = await resolveProjectScopeId(client, WS_A, undefined, PROJ_2);
    expect(result.id).toBe(PROJ_2);
  });
});

describe("resolveWriteScope", () => {
  it("keeps a consistent session scope untouched", async () => {
    const client = makeClient({
      getProject: vi.fn(async () => ({ id: PROJ_1, workspace_id: WS_A })),
    });
    const session = makeSession({ workspace_id: WS_A, project_id: PROJ_1 });
    const scope = await resolveWriteScope(client, session, {});
    expect(scope).toMatchObject({ workspaceId: WS_A, projectId: PROJ_1, recovered: false });
    expect(session.updateScope).not.toHaveBeenCalled();
  });

  it("adopts the project's workspace when the active workspace is soft (drift heal)", async () => {
    const client = makeClient({
      getProject: vi.fn(async () => ({ id: PROJ_1, workspace_id: WS_B })),
    });
    const session = makeSession({ workspace_id: WS_A, project_id: PROJ_1 });
    const scope = await resolveWriteScope(client, session, {}, { resolveFolderScope: () => null });
    expect(scope).toMatchObject({ workspaceId: WS_B, projectId: PROJ_1, recovered: true });
    expect(scope.note).toBeTruthy();
    expect(session.updateScope).toHaveBeenCalledWith({ workspace_id: WS_B, project_id: PROJ_1 });
  });

  it("keeps an explicit workspace authoritative and drops the mismatched project", async () => {
    const client = makeClient({
      getProject: vi.fn(async () => ({ id: PROJ_1, workspace_id: WS_B })),
    });
    const session = makeSession({ workspace_id: WS_A, project_id: PROJ_1 });
    const scope = await resolveWriteScope(client, session, { workspaceId: WS_A });
    expect(scope.workspaceId).toBe(WS_A);
    expect(scope.projectId).toBeUndefined();
    expect(scope.recovered).toBe(false);
    expect(scope.note).toContain("different workspace");
    expect(session.updateScope).not.toHaveBeenCalled();
  });

  it("treats a header-provided workspace as authoritative", async () => {
    const client = makeClient({
      getProject: vi.fn(async () => ({ id: PROJ_1, workspace_id: WS_B })),
    });
    const session = makeSession({ project_id: PROJ_1 });
    const scope = await runWithAuthOverride({ workspaceId: WS_A }, () =>
      resolveWriteScope(client, session, {}, { resolveFolderScope: () => null })
    );
    expect(scope.workspaceId).toBe(WS_A);
    expect(scope.projectId).toBeUndefined();
    expect(session.updateScope).not.toHaveBeenCalled();
  });

  it("treats a folder-config-backed workspace as authoritative", async () => {
    const client = makeClient({
      getProject: vi.fn(async () => ({ id: PROJ_1, workspace_id: WS_B })),
    });
    const session = makeSession({ workspace_id: WS_A, project_id: PROJ_1 }, "/repo");
    const scope = await resolveWriteScope(client, session, {}, {
      resolveFolderScope: () => ({ workspace_id: WS_A }),
    });
    expect(scope.workspaceId).toBe(WS_A);
    expect(scope.projectId).toBeUndefined();
    expect(session.updateScope).not.toHaveBeenCalled();
  });

  it("errors when an explicitly requested project is inaccessible", async () => {
    const client = makeClient({
      getProject: vi.fn(async () => {
        throw new HttpError(404, "not found");
      }),
    });
    const session = makeSession({ workspace_id: WS_A });
    const scope = await resolveWriteScope(client, session, { projectId: PROJ_1 });
    expect(scope.error).toContain(PROJ_1);
  });

  it("silently drops a stale session project with a note", async () => {
    const client = makeClient({
      getProject: vi.fn(async () => {
        throw new HttpError(404, "not found");
      }),
    });
    const session = makeSession({ workspace_id: WS_A, project_id: PROJ_1 });
    const scope = await resolveWriteScope(client, session, {});
    expect(scope.error).toBeUndefined();
    expect(scope.workspaceId).toBe(WS_A);
    expect(scope.projectId).toBeUndefined();
    expect(scope.staleProjectId).toBe(PROJ_1);
    expect(scope.note).toContain("no longer accessible");
  });

  it("returns workspace-only scope when there are no project candidates", async () => {
    const client = makeClient();
    const session = makeSession({ workspace_id: WS_A });
    const scope = await resolveWriteScope(client, session, {});
    expect(scope).toMatchObject({ workspaceId: WS_A, recovered: false });
    expect(scope.projectId).toBeUndefined();
  });
});

describe("recoverWriteScopeAfterProjectError", () => {
  it("ignores non-scope errors", async () => {
    const client = makeClient();
    const session = makeSession({ workspace_id: WS_A });
    const recovered = await recoverWriteScopeAfterProjectError(
      client,
      session,
      { workspaceId: WS_A, projectId: PROJ_1, recovered: false },
      new Error("boom")
    );
    expect(recovered).toBeNull();
  });

  it("retries workspace-only when the workspace is still usable", async () => {
    const client = makeClient({ getWorkspace: vi.fn(async () => ({ id: WS_A })) });
    const session = makeSession({ workspace_id: WS_A, project_id: PROJ_1 });
    const recovered = await recoverWriteScopeAfterProjectError(
      client,
      session,
      { workspaceId: WS_A, projectId: PROJ_1, recovered: false },
      new HttpError(404, "project gone")
    );
    expect(recovered).toMatchObject({
      workspaceId: WS_A,
      projectId: undefined,
      staleProjectId: PROJ_1,
      recovered: true,
    });
  });

  it("clears a stale workspace and recovers from the folder's saved config", async () => {
    const getWorkspace = vi.fn(async (id: string) => {
      if (id === WS_A) throw new HttpError(404, "workspace gone");
      return { id };
    });
    const client = makeClient({ getWorkspace });
    const session = makeSession({ workspace_id: WS_A, project_id: PROJ_1 }, "/repo");
    const recovered = await recoverWriteScopeAfterProjectError(
      client,
      session,
      { workspaceId: WS_A, projectId: PROJ_1, recovered: false },
      new HttpError(403, "forbidden"),
      { resolveFolderScope: () => ({ workspace_id: WS_C, project_id: PROJ_2 }) }
    );
    expect(session.replaceScope).toHaveBeenCalled();
    expect(recovered).toMatchObject({
      workspaceId: WS_C,
      projectId: PROJ_2,
      recovered: true,
    });
  });

  it("clears the stale scope and points at init when the folder has no usable config", async () => {
    const client = makeClient({
      getWorkspace: vi.fn(async () => {
        throw new HttpError(404, "workspace gone");
      }),
    });
    const session = makeSession({ workspace_id: WS_A, project_id: PROJ_1 }, "/repo");
    const recovered = await recoverWriteScopeAfterProjectError(
      client,
      session,
      { workspaceId: WS_A, projectId: PROJ_1, recovered: false },
      new HttpError(404, "gone"),
      { resolveFolderScope: () => null }
    );
    expect(session.replaceScope).toHaveBeenCalled();
    expect(recovered?.workspaceId).toBeUndefined();
    expect(recovered?.note).toContain("init");
  });
});

describe("prependScopeNote", () => {
  it("prepends a [SCOPE] line only when a note exists", () => {
    const result = { content: [{ type: "text" as const, text: "done" }] };
    expect(prependScopeNote(result, undefined).content[0].text).toBe("done");
    expect(prependScopeNote(result, "healed").content[0].text).toBe("[SCOPE] healed\ndone");
  });
});
