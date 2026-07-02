/**
 * Central workspace/project scope resolution for write paths.
 *
 * A session can drift into an inconsistent {workspace, project} pair — for
 * example a workspace that was adopted implicitly sitting next to a project
 * that belongs to a different workspace. Writes then fail on some endpoints
 * and silently mis-scope on others. This module resolves a consistent pair
 * before a write, keeps the caller's explicit intent authoritative,
 * self-heals implicit ("soft") drift, and recovers once from stale-scope
 * errors.
 */
import { HttpError } from "./http.js";
import { getAuthOverride } from "./auth-context.js";
import { resolveWorkspace } from "./workspace-config.js";

const UUID_RE = /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/i;

export function normalizeScopeUuid(value: unknown): string | undefined {
  if (typeof value !== "string") return undefined;
  const trimmed = value.trim();
  return UUID_RE.test(trimmed) ? trimmed.toLowerCase() : undefined;
}

/** Minimal structural view of ContextStreamClient used by scope resolution. */
export interface ScopeClient {
  listProjects(params?: {
    workspace_id?: string;
    page?: number;
    page_size?: number;
  }): Promise<unknown> | unknown;
  getProject(projectId: string): Promise<unknown>;
  getWorkspace(workspaceId: string): Promise<unknown>;
  setDefaults(input: { workspace_id?: string; project_id?: string }): void;
  clearDefaults(input?: { workspace?: boolean; project?: boolean }): void;
}

/** Minimal structural view of SessionManager used by scope resolution. */
export interface ScopeSession {
  getContext(): Record<string, unknown> | null;
  getFolderPath(): string | null;
  updateScope(input: { workspace_id?: string; project_id?: string; folder_path?: string }): void;
  replaceScope(input: {
    workspace_id?: string | null;
    project_id?: string | null;
    folder_path?: string;
  }): void;
}

export interface ResolvedWriteScope {
  workspaceId?: string;
  projectId?: string;
  /** The project the caller explicitly asked for, when it differs from the outcome. */
  requestedProjectId?: string;
  /** A candidate project that turned out to be deleted/inaccessible. */
  staleProjectId?: string;
  /** True when resolution changed the session's scope to self-heal. */
  recovered: boolean;
  /** One-line, user-facing explanation of any adjustment made. */
  note?: string;
  /** Set when resolution cannot proceed (e.g. explicit project unusable). */
  error?: string;
}

function unwrapPayload(value: unknown): Record<string, unknown> | null {
  if (!value || typeof value !== "object") return null;
  const record = value as Record<string, unknown>;
  if (record.data && typeof record.data === "object" && !Array.isArray(record.data)) {
    return record.data as Record<string, unknown>;
  }
  return record;
}

function collectProjectList(value: unknown): Array<Record<string, unknown>> {
  if (Array.isArray(value)) return value as Array<Record<string, unknown>>;
  if (!value || typeof value !== "object") return [];
  const record = value as Record<string, unknown>;
  for (const key of ["items", "projects", "results", "data"]) {
    const nested = record[key];
    if (Array.isArray(nested)) return nested as Array<Record<string, unknown>>;
    if (nested && typeof nested === "object") {
      const deeper = (nested as Record<string, unknown>).items;
      if (Array.isArray(deeper)) return deeper as Array<Record<string, unknown>>;
    }
  }
  return [];
}

function scopeErrorStatus(error: unknown): number | undefined {
  return error instanceof HttpError ? error.status : undefined;
}

export function isScopeNotFoundError(error: unknown): boolean {
  return scopeErrorStatus(error) === 404;
}

export function isScopeAccessError(error: unknown): boolean {
  const status = scopeErrorStatus(error);
  return status === 401 || status === 403;
}

/** True for errors that indicate the workspace/project scope itself is bad. */
export function isScopeError(error: unknown): boolean {
  if (isScopeNotFoundError(error) || isScopeAccessError(error)) return true;
  if (error instanceof HttpError) {
    const code = String(error.code || "").toLowerCase();
    return code === "project_access_denied" || code === "scope_invalid";
  }
  return false;
}

function normalizeNameKey(value: string): string {
  return value.toLowerCase().replace(/[ _-]/g, "");
}

/**
 * Resolve a project reference that may be a UUID or a project *name* —
 * agents commonly pass a name like "my-server" instead of looking up the
 * UUID. Names are matched case-insensitively, ignoring spaces/underscores/
 * hyphens, against the resolved workspace's projects.
 */
export async function resolveProjectScopeId(
  client: ScopeClient,
  workspaceId: string | undefined,
  raw: string | undefined,
  fallback?: string
): Promise<{ id?: string; error?: string }> {
  const value = raw?.trim();
  if (!value) return { id: fallback };

  const asUuid = normalizeScopeUuid(value);
  if (asUuid) return { id: asUuid };

  if (!workspaceId) {
    return {
      error: `Invalid project_id UUID: ${value}. A workspace must be resolved before a project name can be looked up — run init first or pass workspace_id.`,
    };
  }

  let projects: Array<Record<string, unknown>>;
  try {
    projects = collectProjectList(
      await client.listProjects({ workspace_id: workspaceId, page: 1, page_size: 200 })
    );
  } catch {
    return {
      error: `Invalid project_id UUID: ${value}. Could not list workspace projects to try a name match — pass a UUID instead.`,
    };
  }

  const nameKey = normalizeNameKey(value);
  for (const project of projects) {
    const name = String(project?.name ?? "").trim();
    if (!name) continue;
    if (name.toLowerCase() === value.toLowerCase() || normalizeNameKey(name) === nameKey) {
      const id = normalizeScopeUuid(project?.id);
      if (id) return { id };
    }
  }

  const sample = projects
    .slice(0, 5)
    .map((p) => String(p?.name ?? "").trim())
    .filter(Boolean);
  const more = projects.length > 5 ? `, …+${projects.length - 5} more` : "";
  return {
    error: `Invalid project_id UUID: ${value}. No project in this workspace matches that name either. Known projects: ${sample.join(", ")}${more}.`,
  };
}

async function workspaceIsUsable(client: ScopeClient, workspaceId: string): Promise<boolean> {
  try {
    await client.getWorkspace(workspaceId);
    return true;
  } catch (error) {
    if (isScopeNotFoundError(error) || isScopeAccessError(error)) return false;
    // Transient failures must not clear a possibly-valid scope.
    return true;
  }
}

export interface WriteScopeInput {
  workspaceId?: string;
  projectId?: string;
}

export interface ScopeResolutionDeps {
  /** Folder → saved scope lookup; injectable for tests. */
  resolveFolderScope?: (folderPath: string) => {
    workspace_id?: string;
    project_id?: string;
  } | null;
}

function defaultResolveFolderScope(
  folderPath: string
): { workspace_id?: string; project_id?: string } | null {
  try {
    const resolved = resolveWorkspace(folderPath);
    if (!resolved.config) return null;
    return {
      workspace_id: resolved.config.workspace_id,
      project_id: resolved.config.project_id,
    };
  } catch {
    return null;
  }
}

/**
 * Resolve a consistent {workspace, project} pair for a write.
 *
 * Candidate ladder: explicit input → session active scope → workspace-only.
 * When the active workspace and the candidate project disagree, the
 * project's real workspace is adopted only if the active workspace is
 * "soft" — neither caller-provided, nor request-header-provided, nor backed
 * by the folder's saved config. Explicit workspaces stay authoritative and
 * the mismatched project is dropped with a note instead.
 */
export async function resolveWriteScope(
  client: ScopeClient,
  session: ScopeSession | null | undefined,
  input: WriteScopeInput,
  deps?: ScopeResolutionDeps
): Promise<ResolvedWriteScope> {
  const explicitWorkspace = normalizeScopeUuid(input.workspaceId);
  const explicitProject = normalizeScopeUuid(input.projectId);
  const ctx = session?.getContext() ?? null;
  const sessionWorkspace = normalizeScopeUuid(ctx?.workspace_id);
  const sessionProject = normalizeScopeUuid(ctx?.project_id);
  const headerWorkspace = normalizeScopeUuid(getAuthOverride()?.workspaceId);

  const activeWorkspace = explicitWorkspace || headerWorkspace || sessionWorkspace;

  const folderPath = session?.getFolderPath() ?? null;
  const resolveFolderScope = deps?.resolveFolderScope ?? defaultResolveFolderScope;
  let folderBacked = false;
  if (!explicitWorkspace && !headerWorkspace && activeWorkspace && folderPath) {
    const folderScope = resolveFolderScope(folderPath);
    folderBacked = normalizeScopeUuid(folderScope?.workspace_id) === activeWorkspace;
  }
  const workspaceIsSoft = !explicitWorkspace && !headerWorkspace && !folderBacked;

  const candidates: string[] = [];
  for (const candidate of [explicitProject, sessionProject]) {
    if (candidate && !candidates.includes(candidate)) candidates.push(candidate);
  }

  if (candidates.length === 0) {
    return { workspaceId: activeWorkspace, recovered: false };
  }

  let staleProjectId: string | undefined;
  const reachable: Array<{ id: string; workspaceId?: string }> = [];
  for (const candidate of candidates) {
    try {
      const record = unwrapPayload(await client.getProject(candidate));
      reachable.push({ id: candidate, workspaceId: normalizeScopeUuid(record?.workspace_id) });
    } catch (error) {
      if (isScopeError(error)) {
        staleProjectId = staleProjectId || candidate;
        continue;
      }
      // Transient failure: keep the candidate; its workspace is simply unknown.
      reachable.push({ id: candidate, workspaceId: undefined });
    }
  }

  if (explicitProject && staleProjectId === explicitProject) {
    return {
      workspaceId: activeWorkspace,
      requestedProjectId: explicitProject,
      staleProjectId,
      recovered: false,
      error: `project_id ${explicitProject} is not accessible (deleted or no access). Pass a current project_id, a project name, or run init(folder_path=...) to re-resolve scope.`,
    };
  }

  if (reachable.length === 0) {
    return {
      workspaceId: activeWorkspace,
      staleProjectId,
      recovered: Boolean(staleProjectId),
      note: staleProjectId
        ? `Project ${staleProjectId} is no longer accessible; continuing with workspace scope.`
        : undefined,
    };
  }

  // Pass 1: stay in the active workspace when any reachable candidate belongs
  // to it (candidates with unknown workspaces are assumed compatible).
  if (activeWorkspace) {
    const inWorkspace = reachable.find((c) => !c.workspaceId || c.workspaceId === activeWorkspace);
    if (inWorkspace) {
      return {
        workspaceId: activeWorkspace,
        projectId: inWorkspace.id,
        staleProjectId,
        recovered: false,
      };
    }
  } else {
    const first = reachable[0];
    if (first.workspaceId) {
      session?.updateScope({ workspace_id: first.workspaceId, project_id: first.id });
      return {
        workspaceId: first.workspaceId,
        projectId: first.id,
        staleProjectId,
        recovered: true,
        note: `Adopted workspace scope from project ${first.id}.`,
      };
    }
    return { projectId: first.id, staleProjectId, recovered: false };
  }

  // Pass 2: workspace/project mismatch. Adopt the project's real workspace
  // only when the active workspace is soft; then persist so the session heals.
  const best = reachable[0];
  if (workspaceIsSoft && best.workspaceId) {
    session?.updateScope({ workspace_id: best.workspaceId, project_id: best.id });
    return {
      workspaceId: best.workspaceId,
      projectId: best.id,
      requestedProjectId: explicitProject,
      staleProjectId,
      recovered: true,
      note: `Scope self-healed: adopted the workspace that project ${best.id} belongs to (the previous workspace scope was implicit and inconsistent).`,
    };
  }

  return {
    workspaceId: activeWorkspace,
    requestedProjectId: explicitProject ?? best.id,
    staleProjectId,
    recovered: false,
    note: `Project ${best.id} belongs to a different workspace; kept the authoritative workspace scope and wrote without a project. Pass matching workspace_id and project_id to target that project.`,
  };
}

/**
 * One-shot recovery after a write failed with a scope error. Returns the
 * scope to retry with, or null when the failure isn't scope-shaped.
 */
export async function recoverWriteScopeAfterProjectError(
  client: ScopeClient,
  session: ScopeSession | null | undefined,
  scope: ResolvedWriteScope,
  error: unknown,
  deps?: ScopeResolutionDeps
): Promise<ResolvedWriteScope | null> {
  if (!isScopeError(error)) return null;

  // The workspace still works: the project binding is what went stale.
  if (scope.projectId && scope.workspaceId && (await workspaceIsUsable(client, scope.workspaceId))) {
    return {
      ...scope,
      projectId: undefined,
      staleProjectId: scope.projectId,
      recovered: true,
      note: `Project scope ${scope.projectId} was rejected; retried with workspace scope only.`,
    };
  }

  // The workspace itself is stale: clear the implicit binding and re-resolve
  // from the folder's saved config.
  if (scope.workspaceId && !(await workspaceIsUsable(client, scope.workspaceId))) {
    const folderPath = session?.getFolderPath() ?? undefined;
    session?.replaceScope({ workspace_id: null, project_id: null, folder_path: folderPath });

    const resolveFolderScope = deps?.resolveFolderScope ?? defaultResolveFolderScope;
    const folderScope = folderPath ? resolveFolderScope(folderPath) : null;
    const folderWorkspace = normalizeScopeUuid(folderScope?.workspace_id);
    const folderProject = normalizeScopeUuid(folderScope?.project_id);

    if (folderWorkspace && (await workspaceIsUsable(client, folderWorkspace))) {
      session?.updateScope({
        workspace_id: folderWorkspace,
        project_id: folderProject,
        folder_path: folderPath,
      });
      return {
        workspaceId: folderWorkspace,
        projectId: folderProject,
        staleProjectId: scope.projectId,
        recovered: true,
        note: `Recovered scope from this folder's saved config after the previous workspace became inaccessible.`,
      };
    }

    return {
      recovered: true,
      staleProjectId: scope.projectId,
      note: `Cleared a stale scope (the previous workspace is no longer accessible). Run init(folder_path="...") to re-associate this folder.`,
    };
  }

  return null;
}

/** Prepend a scope note (if any) to a tool result's first text block. */
export function prependScopeNote<
  T extends { content: Array<{ type: "text"; text: string }> },
>(result: T, note: string | undefined): T {
  if (!note) return result;
  const first = result.content?.[0];
  if (first && first.type === "text") {
    first.text = `[SCOPE] ${note}\n${first.text}`;
  }
  return result;
}
