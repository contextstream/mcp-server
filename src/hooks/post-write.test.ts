import { execFileSync } from "node:child_process";
import * as fs from "node:fs/promises";
import * as os from "node:os";
import * as path from "node:path";
import { afterEach, describe, expect, it } from "vitest";
import { shouldIgnoreHookPath } from "./post-write.js";

describe("post-write ignore policy", () => {
  const tempDirs: string[] = [];

  afterEach(async () => {
    await Promise.all(
      tempDirs.splice(0).map((dir) => fs.rm(dir, { recursive: true, force: true }))
    );
  });

  it("always rejects agent state and credential directories", async () => {
    const root = await fs.mkdtemp(path.join(os.tmpdir(), "contextstream-post-write-"));
    tempDirs.push(root);

    expect(shouldIgnoreHookPath(root, path.join(root, ".claude", "worktrees", "app.ts"))).toBe(
      true
    );
    expect(shouldIgnoreHookPath(root, path.join(root, ".codex", "auth.json"))).toBe(true);
    expect(shouldIgnoreHookPath(root, path.join(root, ".ssh", "config"))).toBe(true);
  });

  it("rejects a configured project root nested inside agent state", async () => {
    const outer = await fs.mkdtemp(path.join(os.tmpdir(), "contextstream-post-write-"));
    tempDirs.push(outer);
    const root = path.join(outer, ".claude", "worktrees", "demo");
    const file = path.join(root, "src", "main.ts");
    await fs.mkdir(path.dirname(file), { recursive: true });
    await fs.writeFile(file, "export const hidden = true;\n");

    expect(shouldIgnoreHookPath(root, file)).toBe(true);
  });

  it("respects nested gitignore rules for hook-pushed files", async () => {
    const root = await fs.mkdtemp(path.join(os.tmpdir(), "contextstream-post-write-"));
    tempDirs.push(root);
    await fs.mkdir(path.join(root, "packages", "demo", "generated"), { recursive: true });
    await fs.writeFile(path.join(root, "packages", "demo", ".gitignore"), "generated/\n");
    await fs.writeFile(
      path.join(root, "packages", "demo", "generated", "bundle.ts"),
      "export const generated = true;\n"
    );
    execFileSync("git", ["init", "-q", root]);

    expect(
      shouldIgnoreHookPath(root, path.join(root, "packages", "demo", "generated", "bundle.ts"))
    ).toBe(true);
    expect(shouldIgnoreHookPath(root, path.join(root, "packages", "demo", "src", "main.ts"))).toBe(
      false
    );
  });

  it("rejects paths outside the configured project root", async () => {
    const root = await fs.mkdtemp(path.join(os.tmpdir(), "contextstream-post-write-"));
    tempDirs.push(root);
    expect(shouldIgnoreHookPath(root, path.join(path.dirname(root), "other", "main.ts"))).toBe(
      true
    );
  });
});
