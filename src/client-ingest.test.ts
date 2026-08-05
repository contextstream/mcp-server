import * as fs from "node:fs/promises";
import * as os from "node:os";
import * as path from "node:path";
import { randomUUID } from "node:crypto";
import { afterEach, describe, expect, it, vi } from "vitest";
import { ContextStreamClient } from "./client.js";
import { deleteHashManifest, readHashManifest } from "./files.js";

describe("local ingest failure reporting", () => {
  const tempDirs: string[] = [];
  const projectIds: string[] = [];

  afterEach(async () => {
    vi.restoreAllMocks();
    for (const projectId of projectIds.splice(0)) deleteHashManifest(projectId);
    await Promise.all(
      tempDirs.splice(0).map((dir) => fs.rm(dir, { recursive: true, force: true }))
    );
  });

  it("reports a failed batch and leaves it eligible for retry", async () => {
    const root = await fs.mkdtemp(path.join(os.tmpdir(), "contextstream-ingest-test-"));
    const projectId = randomUUID();
    tempDirs.push(root);
    projectIds.push(projectId);
    await fs.writeFile(path.join(root, "main.ts"), "export const value = 1;\n");

    const client = new ContextStreamClient({
      apiUrl: "https://api.contextstream.io",
      apiKey: "test-key",
      userAgent: "test",
      contextPackEnabled: true,
      showTiming: false,
      toolSurfaceProfile: "default",
    });
    vi.spyOn(client, "ingestFilesAdaptive").mockRejectedValue(new Error("network unavailable"));

    const result = await client.ingestLocal({ projectId, rootPath: root, force: true });
    expect(result).toMatchObject({
      status: "error",
      filesIndexed: 0,
      failedBatches: 1,
      failedFiles: 1,
      lastError: "network unavailable",
    });
    expect(readHashManifest(projectId).has("main.ts")).toBe(false);
  });
});
