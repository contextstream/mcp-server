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

    expect(count.count).toBe(1);
    expect(files.map((file) => file.path)).toContain(path.join("lib", "main.dart"));
  });

  it("detects Dart language metadata", () => {
    expect(detectLanguage("lib/main.dart")).toBe("dart");
  });
});
