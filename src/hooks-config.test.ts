import { describe, it, expect, vi, beforeEach } from "vitest";
import {
  buildHooksConfig,
  mergeHooksIntoSettings,
  getClaudeSettingsPath,
  getHooksDir,
  ClaudeHooksConfig,
} from "./hooks-config.js";
import { homedir } from "node:os";
import * as path from "node:path";

describe("hooks-config", () => {
  describe("buildHooksConfig", () => {
    it("should return PreToolUse and UserPromptSubmit hooks", () => {
      const config = buildHooksConfig();

      expect(config).toBeDefined();
      expect(config?.PreToolUse).toBeDefined();
      expect(config?.UserPromptSubmit).toBeDefined();
    });

    it("should NOT intercept MCP tools to prevent hook loops", () => {
      // CRITICAL: Hooks must not intercept MCP calls or we get infinite loops
      const config = buildHooksConfig();
      const preToolUse = config?.PreToolUse?.[0];

      expect(preToolUse).toBeDefined();
      const matcher = preToolUse!.matcher;

      // MCP tools should NOT be in the matcher - this would cause loops
      expect(matcher).not.toContain("mcp__");
      expect(matcher).not.toContain("contextstream");
      expect(matcher).not.toContain("MCP");
    });

    it("should intercept local discovery tools", () => {
      const config = buildHooksConfig();
      const matcher = config?.PreToolUse?.[0]?.matcher || "";

      // These tools should be intercepted
      expect(matcher).toContain("Glob");
      expect(matcher).toContain("Grep");
      expect(matcher).toContain("Search");
      expect(matcher).toContain("Task");
    });

    it("should use npx command for hooks", () => {
      const config = buildHooksConfig();

      for (const hook of config?.PreToolUse?.[0]?.hooks || []) {
        expect(hook.command).toContain("npx @contextstream/mcp-server hook");
        expect(hook.type).toBe("command");
      }

      for (const hook of config?.UserPromptSubmit?.[0]?.hooks || []) {
        expect(hook.command).toContain("npx @contextstream/mcp-server hook");
        expect(hook.type).toBe("command");
      }
    });

    it("should have reasonable timeout values", () => {
      const config = buildHooksConfig();

      // Hooks should timeout quickly to avoid blocking Claude
      for (const matcher of config?.PreToolUse || []) {
        for (const hook of matcher.hooks) {
          expect(hook.timeout).toBeLessThanOrEqual(10);
        }
      }
    });
  });

  describe("mergeHooksIntoSettings", () => {
    it("should add hooks to empty settings", () => {
      const newHooks: ClaudeHooksConfig["hooks"] = {
        PreToolUse: [
          {
            matcher: "Glob",
            hooks: [{ type: "command", command: "echo test" }],
          },
        ],
      };

      const result = mergeHooksIntoSettings({}, newHooks);

      expect(result.hooks).toBeDefined();
      expect((result.hooks as any).PreToolUse).toHaveLength(1);
    });

    it("should preserve existing non-contextstream hooks", () => {
      const existingSettings = {
        hooks: {
          PreToolUse: [
            {
              matcher: "Glob",
              hooks: [{ type: "command", command: "my-custom-hook.sh" }],
            },
          ],
        },
      };

      const newHooks: ClaudeHooksConfig["hooks"] = {
        PreToolUse: [
          {
            matcher: "Glob",
            hooks: [
              { type: "command", command: "path/to/contextstream-redirect.py" },
            ],
          },
        ],
      };

      const result = mergeHooksIntoSettings(existingSettings, newHooks);
      const preToolUse = (result.hooks as any).PreToolUse;

      // Should have both: user's custom hook AND contextstream hook
      expect(preToolUse.length).toBe(2);

      // User's custom hook should be preserved
      const hasCustomHook = preToolUse.some(
        (m: any) => m.hooks?.[0]?.command === "my-custom-hook.sh"
      );
      expect(hasCustomHook).toBe(true);

      // ContextStream hook should be added
      const hasContextStreamHook = preToolUse.some((m: any) =>
        m.hooks?.[0]?.command?.includes("contextstream")
      );
      expect(hasContextStreamHook).toBe(true);
    });

    it("should replace existing contextstream hooks (no duplicates)", () => {
      const existingSettings = {
        hooks: {
          PreToolUse: [
            {
              matcher: "Glob",
              hooks: [
                {
                  type: "command",
                  command: "old/path/contextstream-redirect.py",
                },
              ],
            },
          ],
        },
      };

      const newHooks: ClaudeHooksConfig["hooks"] = {
        PreToolUse: [
          {
            matcher: "Glob|Grep",
            hooks: [
              {
                type: "command",
                command: "new/path/contextstream-redirect.py",
              },
            ],
          },
        ],
      };

      const result = mergeHooksIntoSettings(existingSettings, newHooks);
      const preToolUse = (result.hooks as any).PreToolUse;

      // Should only have 1 contextstream hook, not 2
      const contextStreamHooks = preToolUse.filter((m: any) =>
        m.hooks?.[0]?.command?.includes("contextstream")
      );
      expect(contextStreamHooks.length).toBe(1);

      // Should be the NEW hook
      expect(contextStreamHooks[0].hooks[0].command).toContain("new/path");
    });

    it("should preserve other settings fields", () => {
      const existingSettings = {
        someOtherSetting: "value",
        hooks: {},
      };

      const newHooks = buildHooksConfig();
      const result = mergeHooksIntoSettings(existingSettings, newHooks);

      expect(result.someOtherSetting).toBe("value");
    });
  });

  describe("getClaudeSettingsPath", () => {
    it("should return user-level settings path", () => {
      const settingsPath = getClaudeSettingsPath("user");
      expect(settingsPath).toBe(path.join(homedir(), ".claude", "settings.json"));
    });

    it("should return project-level settings path", () => {
      const projectPath = "/home/user/myproject";
      const settingsPath = getClaudeSettingsPath("project", projectPath);
      expect(settingsPath).toBe(
        path.join(projectPath, ".claude", "settings.json")
      );
    });

    it("should throw if project scope without projectPath", () => {
      expect(() => getClaudeSettingsPath("project")).toThrow(
        "projectPath required"
      );
    });
  });

  describe("getHooksDir", () => {
    it("should return hooks directory in user home", () => {
      const hooksDir = getHooksDir();
      expect(hooksDir).toBe(path.join(homedir(), ".claude", "hooks"));
    });
  });

  // ======================================================================
  // REGRESSION TESTS: Hook loop prevention
  // These tests ensure hooks don't create recursive call patterns
  // ======================================================================

  describe("hook loop prevention", () => {
    it("should NOT intercept Read tool to allow targeted file reads", () => {
      const config = buildHooksConfig();
      const matcher = config?.PreToolUse?.[0]?.matcher || "";

      // Read is intentionally NOT blocked - Claude needs to read files after
      // discovering them via ContextStream search
      expect(matcher).not.toContain("Read");
    });

    it("should NOT intercept Write/Edit tools", () => {
      const config = buildHooksConfig();
      const matcher = config?.PreToolUse?.[0]?.matcher || "";

      expect(matcher).not.toContain("Write");
      expect(matcher).not.toContain("Edit");
    });

    it("should NOT intercept Bash tool", () => {
      const config = buildHooksConfig();
      const matcher = config?.PreToolUse?.[0]?.matcher || "";

      // Bash is needed for git, npm, etc.
      expect(matcher).not.toContain("Bash");
    });

    it("UserPromptSubmit hook should have wildcard matcher", () => {
      const config = buildHooksConfig();
      const matcher = config?.UserPromptSubmit?.[0]?.matcher;

      // UserPromptSubmit fires on all prompts, uses * matcher
      expect(matcher).toBe("*");
    });
  });
});
