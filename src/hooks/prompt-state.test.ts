import { afterEach, beforeEach, describe, expect, it } from "vitest";
import * as fs from "node:fs";
import * as path from "node:path";
import { homedir } from "node:os";
import {
  clearContextRequired,
  clearInitRequired,
  cleanupStale,
  isContextFreshAndClean,
  isContextRequired,
  isInitRequired,
  markContextRequired,
  markInitRequired,
  markStateChanged,
} from "./prompt-state.js";

const STATE_PATH = path.join(homedir(), ".contextstream", "prompt-state.json");

describe("prompt-state", () => {
  let backup: string | null = null;

  beforeEach(() => {
    if (fs.existsSync(STATE_PATH)) {
      backup = fs.readFileSync(STATE_PATH, "utf8");
    } else {
      backup = null;
    }
    fs.mkdirSync(path.dirname(STATE_PATH), { recursive: true });
    fs.writeFileSync(STATE_PATH, JSON.stringify({ workspaces: {} }, null, 2), "utf8");
  });

  afterEach(() => {
    if (backup !== null) {
      fs.writeFileSync(STATE_PATH, backup, "utf8");
    } else if (fs.existsSync(STATE_PATH)) {
      fs.unlinkSync(STATE_PATH);
    }
  });

  it("tracks init requirement lifecycle", () => {
    const cwd = "/tmp/cs-prompt-state-init";
    markInitRequired(cwd);
    expect(isInitRequired(cwd)).toBe(true);

    clearInitRequired(cwd);
    expect(isInitRequired(cwd)).toBe(false);
  });

  it("tracks context requirement and freshness", () => {
    const cwd = "/tmp/cs-prompt-state-context";
    markContextRequired(cwd);
    expect(isContextRequired(cwd)).toBe(true);

    clearContextRequired(cwd);
    expect(isContextRequired(cwd)).toBe(false);
    expect(isContextFreshAndClean(cwd, 120)).toBe(true);
  });

  it("marks context stale when state changes after context", () => {
    const cwd = "/tmp/cs-prompt-state-changed";
    markContextRequired(cwd);
    clearContextRequired(cwd);
    expect(isContextFreshAndClean(cwd, 120)).toBe(true);

    markStateChanged(cwd);
    expect(isContextFreshAndClean(cwd, 120)).toBe(false);
  });

  it("supports path matching between parent and child directories", () => {
    const parent = "/tmp/cs-workspace";
    const child = "/tmp/cs-workspace/subdir";

    markContextRequired(parent);
    expect(isContextRequired(child)).toBe(true);

    clearContextRequired(child);
    expect(isContextRequired(parent)).toBe(false);
  });

  it("cleans up stale entries", () => {
    const stale = {
      workspaces: {
        "/tmp/cs-stale": {
          require_context: true,
          require_init: true,
          updated_at: new Date(Date.now() - 3600_000).toISOString(),
        },
      },
    };

    fs.writeFileSync(STATE_PATH, JSON.stringify(stale, null, 2), "utf8");
    cleanupStale(60);

    const parsed = JSON.parse(fs.readFileSync(STATE_PATH, "utf8"));
    expect(Object.keys(parsed.workspaces)).toHaveLength(0);
  });
});
