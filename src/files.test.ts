import { afterEach, describe, expect, it } from "vitest";
import * as fs from "node:fs/promises";
import * as os from "node:os";
import * as path from "node:path";
import {
  clearGitContextCache,
  countIndexableFiles,
  detectLanguage,
  readFilesFromDirectory,
} from "./files.js";

async function makeTempProject(): Promise<string> {
  return fs.mkdtemp(path.join(os.tmpdir(), "contextstream-files-test-"));
}

describe("files", () => {
  const tempDirs: string[] = [];

  afterEach(async () => {
    clearGitContextCache();
    await Promise.all(
      tempDirs.splice(0).map((dir) => fs.rm(dir, { recursive: true, force: true }))
    );
  });

  it("treats Dart files as indexable source", async () => {
    const root = await makeTempProject();
    tempDirs.push(root);

    await fs.mkdir(path.join(root, "lib"), { recursive: true });
    await fs.writeFile(path.join(root, "lib", "main.dart"), "void main() {}\n", "utf8");

    const count = await countIndexableFiles(root, { maxFiles: 10 });
    const files = await readFilesFromDirectory(root, { maxFiles: 10 });
    const dartFile = files.find((file) => file.path === path.join("lib", "main.dart"));

    expect(count.count).toBe(1);
    expect(files.map((file) => file.path)).toContain(path.join("lib", "main.dart"));
    expect(dartFile?.language).toBe("dart");
  });

  it("detects Dart language metadata", () => {
    expect(detectLanguage("lib/main.dart")).toBe("dart");
  });

  it("never reads agent state, nested worktrees, or credential files", async () => {
    const root = await makeTempProject();
    tempDirs.push(root);

    await fs.mkdir(path.join(root, ".claude", "worktrees", "stale"), { recursive: true });
    await fs.mkdir(path.join(root, ".codex"), { recursive: true });
    await fs.mkdir(path.join(root, ".aws"), { recursive: true });
    await fs.mkdir(path.join(root, "src"), { recursive: true });
    await fs.writeFile(path.join(root, ".claude", "worktrees", "stale", "app.ts"), "secret\n");
    await fs.writeFile(path.join(root, ".codex", "auth.json"), '{"token":"secret"}\n');
    await fs.writeFile(path.join(root, ".aws", "credentials.json"), '{"secret":true}\n');
    await fs.writeFile(path.join(root, "src", "credentials.json"), '{"secret":true}\n');
    await fs.writeFile(path.join(root, "src", "main.ts"), "export const ok = true;\n");

    const files = await readFilesFromDirectory(root, { maxFiles: 20 });
    expect(files.map((file) => file.path)).toEqual([path.join("src", "main.ts")]);
    expect((await countIndexableFiles(root, { maxFiles: 20 })).count).toBe(1);
  });

  it("rejects an ingest root that is itself inside agent state", async () => {
    const outer = await makeTempProject();
    tempDirs.push(outer);
    const nestedRoot = path.join(outer, ".claude", "worktrees", "demo");
    await fs.mkdir(path.join(nestedRoot, "src"), { recursive: true });
    await fs.writeFile(path.join(nestedRoot, "src", "main.ts"), "export const hidden = true;\n");

    expect(await readFilesFromDirectory(nestedRoot, { maxFiles: 20 })).toEqual([]);
    expect(await countIndexableFiles(nestedRoot, { maxFiles: 20 })).toEqual({
      count: 0,
      stopped: false,
    });
  });

  it("respects repository ignore files during full ingestion", async () => {
    const root = await makeTempProject();
    tempDirs.push(root);

    await fs.mkdir(path.join(root, "generated"), { recursive: true });
    await fs.mkdir(path.join(root, "src"), { recursive: true });
    await fs.writeFile(path.join(root, ".gitignore"), "generated/\n");
    await fs.writeFile(path.join(root, "generated", "bundle.ts"), "export const noise = true;\n");
    await fs.writeFile(path.join(root, "src", "main.ts"), "export const ok = true;\n");

    const files = await readFilesFromDirectory(root, { maxFiles: 20 });
    expect(files.map((file) => file.path)).toEqual([path.join("src", "main.ts")]);
  });
});
