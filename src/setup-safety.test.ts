import * as path from "node:path";
import { describe, expect, it } from "vitest";
import { isBroadProjectPath } from "./setup.js";

describe("setup project-root safety", () => {
  it("rejects filesystem roots and the user home", () => {
    const home = path.resolve("/tmp/contextstream-home");
    expect(isBroadProjectPath(path.parse(home).root, home)).toBe(true);
    expect(isBroadProjectPath(home, home)).toBe(true);
  });

  it("accepts ordinary repository and empty-folder paths", () => {
    const home = path.resolve("/tmp/contextstream-home");
    expect(isBroadProjectPath(path.join(home, "dev", "repo"), home)).toBe(false);
    expect(isBroadProjectPath(path.join(home, "Documents", "empty-project"), home)).toBe(false);
  });
});
