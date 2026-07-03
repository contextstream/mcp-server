import { describe, it, expect } from "vitest";
import { normalizeGitRemote } from "./project-index-utils.js";

describe("normalizeGitRemote", () => {
  it("normalizes ssh and https forms of the same repo to one identity", () => {
    expect(normalizeGitRemote("git@github.com:org/repo.git")).toBe("github.com/org/repo");
    expect(normalizeGitRemote("https://github.com/org/repo")).toBe("github.com/org/repo");
    expect(normalizeGitRemote("https://github.com/Org/Repo.git")).toBe("github.com/org/repo");
    expect(normalizeGitRemote("ssh://git@github.com/org/repo.git/")).toBe("github.com/org/repo");
  });

  it("strips embedded credentials", () => {
    expect(normalizeGitRemote("https://user:token@github.com/org/repo.git")).toBe(
      "github.com/org/repo"
    );
  });

  it("distinguishes different repos that share a name", () => {
    const a = normalizeGitRemote("git@github.com:contextstream/mcp.git");
    const b = normalizeGitRemote("git@github.com:contextstream/mcp-server.git");
    expect(a).not.toBe(b);
  });

  it("returns null for empty input", () => {
    expect(normalizeGitRemote("")).toBeNull();
    expect(normalizeGitRemote("   ")).toBeNull();
  });
});
