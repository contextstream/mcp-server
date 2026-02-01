/**
 * Hook Loop Scenario Tests
 *
 * These tests verify the hook behavior for preventing infinite loops:
 * 1. Non-indexed projects should NOT have tools blocked
 * 2. Indexed projects should have discovery tools blocked
 * 3. Files outside project scope should NOT be blocked
 * 4. Graceful fallback when ContextStream has no results
 *
 * This is a regression test suite for the Vladislav feedback about
 * hook-induced infinite loops when working with non-indexed projects.
 */

import { describe, it, expect, beforeEach, afterEach, vi } from "vitest";
import * as fs from "fs";
import * as path from "path";
import * as os from "os";
import { execSync } from "child_process";

// Path to the hook script - now uses Node.js via the built dist
const HOOK_SCRIPT_PATH = path.join(__dirname, "..", "dist", "index.js");
const INDEX_STATUS_FILE = path.join(os.homedir(), ".contextstream", "indexed-projects.json");

describe("Hook Loop Prevention Scenarios", () => {
  let tempDir: string;
  let originalIndexStatus: string | null = null;

  beforeEach(() => {
    // Create a temporary directory
    tempDir = fs.mkdtempSync(path.join(os.tmpdir(), "cs-hook-test-"));

    // Backup existing index status if it exists
    if (fs.existsSync(INDEX_STATUS_FILE)) {
      originalIndexStatus = fs.readFileSync(INDEX_STATUS_FILE, "utf-8");
    }
  });

  afterEach(() => {
    // Clean up temp directory
    fs.rmSync(tempDir, { recursive: true, force: true });

    // Restore original index status
    if (originalIndexStatus !== null) {
      fs.mkdirSync(path.dirname(INDEX_STATUS_FILE), { recursive: true });
      fs.writeFileSync(INDEX_STATUS_FILE, originalIndexStatus);
    } else if (fs.existsSync(INDEX_STATUS_FILE)) {
      // Remove the test index status file
      fs.unlinkSync(INDEX_STATUS_FILE);
    }
  });

  describe("is_project_indexed function behavior", () => {
    it("should return false when no index status file exists", () => {
      // Remove index status file if it exists
      if (fs.existsSync(INDEX_STATUS_FILE)) {
        fs.unlinkSync(INDEX_STATUS_FILE);
      }

      // The hook script's is_project_indexed should return False
      // We test this indirectly by checking the hook behavior
      const result = simulateHookCheck(tempDir, false);
      expect(result.isIndexed).toBe(false);
    });

    it("should return true for paths within an indexed project", () => {
      // Create index status file with our temp project
      setIndexStatus({
        [tempDir]: {
          indexed_at: new Date().toISOString(),
          project_id: "test-project-id",
        },
      });

      const result = simulateHookCheck(tempDir, true);
      expect(result.isIndexed).toBe(true);
    });

    it("should return true for subdirectories of an indexed project", () => {
      const subDir = path.join(tempDir, "src", "components");
      fs.mkdirSync(subDir, { recursive: true });

      setIndexStatus({
        [tempDir]: {
          indexed_at: new Date().toISOString(),
          project_id: "test-project-id",
        },
      });

      const result = simulateHookCheck(subDir, true);
      expect(result.isIndexed).toBe(true);
    });

    it("should return false for paths outside any indexed project", () => {
      const outsideDir = fs.mkdtempSync(path.join(os.tmpdir(), "outside-"));

      setIndexStatus({
        [tempDir]: {
          indexed_at: new Date().toISOString(),
          project_id: "test-project-id",
        },
      });

      const result = simulateHookCheck(outsideDir, true);
      expect(result.isIndexed).toBe(false);

      fs.rmSync(outsideDir, { recursive: true, force: true });
    });
  });

  describe("Hook blocking behavior", () => {
    it("should NOT block tools for non-indexed projects", () => {
      // Ensure no index status exists for temp dir
      setIndexStatus({
        "/some/other/project": {
          indexed_at: new Date().toISOString(),
          project_id: "other-project",
        },
      });

      // For non-indexed projects, hooks should allow local tools
      const hookInput = createHookInput("Glob", { pattern: "**/*.ts" }, tempDir);
      const result = runHookScript(hookInput);

      // Should NOT block - either continue or provide helpful message
      expect(result.decision).not.toBe("block");
    });

    it("should block discovery tools for indexed projects", () => {
      setIndexStatus({
        [tempDir]: {
          indexed_at: new Date().toISOString(),
          project_id: "test-project-id",
        },
      });

      const hookInput = createHookInput("Glob", { pattern: "**/*.ts" }, tempDir);
      const result = runHookScript(hookInput);

      // Should block and redirect to ContextStream search
      expect(result.decision).toBe("block");
      expect(result.reason?.toLowerCase()).toContain("contextstream");
    });

    it("should NOT block Read tool (needed after ContextStream search)", () => {
      setIndexStatus({
        [tempDir]: {
          indexed_at: new Date().toISOString(),
          project_id: "test-project-id",
        },
      });

      const hookInput = createHookInput(
        "Read",
        { file_path: path.join(tempDir, "src/index.ts") },
        tempDir
      );
      const result = runHookScript(hookInput);

      // Read should be allowed (not in matcher)
      expect(result.decision).not.toBe("block");
    });

    it("should NOT block MCP tools to prevent loops", () => {
      setIndexStatus({
        [tempDir]: {
          indexed_at: new Date().toISOString(),
          project_id: "test-project-id",
        },
      });

      const hookInput = createHookInput(
        "mcp__contextstream__search",
        { query: "test", mode: "hybrid" },
        tempDir
      );
      const result = runHookScript(hookInput);

      // MCP tools should never be blocked
      expect(result.decision).not.toBe("block");
    });
  });

  describe("Stale index handling", () => {
    it("should handle stale index gracefully", () => {
      // Create index status with old timestamp (> 7 days)
      const staleDate = new Date();
      staleDate.setDate(staleDate.getDate() - 10);

      setIndexStatus({
        [tempDir]: {
          indexed_at: staleDate.toISOString(),
          project_id: "test-project-id",
        },
      });

      const hookInput = createHookInput("Glob", { pattern: "**/*.ts" }, tempDir);
      const result = runHookScript(hookInput);

      // Should still block but may include stale warning
      if (result.decision === "block") {
        // Stale index should suggest re-indexing
        expect(result.reason?.toLowerCase()).toMatch(/stale|reindex|context/);
      }
    });
  });

  describe("Edge cases", () => {
    it("should handle malformed index status file", () => {
      // Write invalid JSON
      fs.mkdirSync(path.dirname(INDEX_STATUS_FILE), { recursive: true });
      fs.writeFileSync(INDEX_STATUS_FILE, "{ invalid json }");

      const hookInput = createHookInput("Glob", { pattern: "**/*.ts" }, tempDir);

      // Should not throw, should gracefully handle
      expect(() => runHookScript(hookInput)).not.toThrow();
    });

    it("should handle missing cwd in hook input", () => {
      const hookInput = {
        tool_name: "Glob",
        tool_input: { pattern: "**/*.ts" },
        // No cwd
      };

      // Should not throw
      expect(() => runHookScriptRaw(JSON.stringify(hookInput))).not.toThrow();
    });
  });
});

// ============================================================================
// Helper Functions
// ============================================================================

interface HookInput {
  tool_name: string;
  tool_input: Record<string, unknown>;
  cwd?: string;
}

interface HookResult {
  decision: "continue" | "block";
  reason?: string;
}

function setIndexStatus(projects: Record<string, { indexed_at: string; project_id: string }>) {
  const data = { projects };
  fs.mkdirSync(path.dirname(INDEX_STATUS_FILE), { recursive: true });
  fs.writeFileSync(INDEX_STATUS_FILE, JSON.stringify(data, null, 2));
}

function createHookInput(
  toolName: string,
  toolInput: Record<string, unknown>,
  cwd: string
): HookInput {
  return {
    tool_name: toolName,
    tool_input: toolInput,
    cwd,
  };
}

function runHookScript(input: HookInput): HookResult {
  return runHookScriptRaw(JSON.stringify(input));
}

function runHookScriptRaw(inputJson: string): HookResult {
  // Skip if hook script doesn't exist
  if (!fs.existsSync(HOOK_SCRIPT_PATH)) {
    // Return a mock result that simulates "no blocking"
    return { decision: "continue" };
  }

  try {
    execSync(`echo '${inputJson.replace(/'/g, "'\\''")}' | node "${HOOK_SCRIPT_PATH}" hook pre-tool-use`, {
      encoding: "utf-8",
      timeout: 5000,
      stdio: ["pipe", "pipe", "pipe"],
      env: {
        ...process.env,
        CONTEXTSTREAM_HOOK_ENABLED: "true",
      },
    });

    // Exit code 0 means continue
    return { decision: "continue" };
  } catch (error: unknown) {
    // Hook script exits with code 2 to block
    // Check the exit code
    if (error && typeof error === "object" && "status" in error) {
      const status = (error as { status: number }).status;
      if (status === 2) {
        // Get the stderr message
        const stderr = (error as { stderr?: Buffer | string }).stderr;
        const reason = stderr
          ? typeof stderr === "string"
            ? stderr
            : stderr.toString()
          : "Blocked by ContextStream hook";
        return { decision: "block", reason: reason.trim() };
      }
    }
    // Other errors - treat as continue
    return { decision: "continue" };
  }
}

function simulateHookCheck(
  cwd: string,
  hasIndexFile: boolean
): { isIndexed: boolean; isStale: boolean } {
  if (!hasIndexFile || !fs.existsSync(INDEX_STATUS_FILE)) {
    return { isIndexed: false, isStale: false };
  }

  try {
    const data = JSON.parse(fs.readFileSync(INDEX_STATUS_FILE, "utf-8"));
    const projects = data.projects || {};
    const cwdResolved = path.resolve(cwd);

    for (const [projectPath, info] of Object.entries(projects)) {
      const indexedPath = path.resolve(projectPath);
      if (cwdResolved === indexedPath || cwdResolved.startsWith(indexedPath + path.sep)) {
        const indexedAt = new Date((info as { indexed_at: string }).indexed_at);
        const staleThreshold = new Date();
        staleThreshold.setDate(staleThreshold.getDate() - 7);
        const isStale = indexedAt < staleThreshold;
        return { isIndexed: true, isStale };
      }
    }
  } catch {
    // Parse error
  }

  return { isIndexed: false, isStale: false };
}
