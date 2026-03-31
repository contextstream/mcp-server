import { describe, it, expect, vi, beforeEach } from "vitest";
import {
  buildHooksConfig,
  mergeHooksIntoSettings,
  getClaudeSettingsPath,
  getHooksDir,
  ClaudeHooksConfig,
  CLAUDE_ENFORCEMENT_CRITICAL_HOOKS,
  CURSOR_ENFORCEMENT_CRITICAL_HOOKS,
  installCursorHookScripts,
  readCursorHooksConfig,
  installWindsurfHookScripts,
  readWindsurfHooksConfig,
} from "./hooks-config.js";
import { homedir } from "node:os";
import * as fs from "node:fs/promises";
import * as os from "node:os";
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

      // Rust parity uses a wildcard matcher so the hook can both allow init/context
      // and block any non-context-first tool call.
      expect(matcher).toBe("*");
    });

    it("should use valid hook commands", () => {
      const config = buildHooksConfig();

      for (const hook of config?.PreToolUse?.[0]?.hooks || []) {
        expect(hook.command).toContain("hook pre-tool-use");
        expect(hook.type).toBe("command");
      }

      for (const hook of config?.UserPromptSubmit?.[0]?.hooks || []) {
        expect(hook.command).toContain("hook user-prompt-submit");
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

    it("should use 15s timeout for SessionStart hook", () => {
      const config = buildHooksConfig();
      const sessionStart = config?.SessionStart?.[0]?.hooks?.[0];
      expect(sessionStart?.command).toContain("hook session-start");
      expect(sessionStart?.timeout).toBe(15);
    });

    it("should include expanded Claude lifecycle hooks", () => {
      const config = buildHooksConfig();
      expect(config?.InstructionsLoaded).toBeDefined();
      expect(config?.ConfigChange).toBeDefined();
      expect(config?.CwdChanged).toBeDefined();
      expect(config?.FileChanged).toBeDefined();
      expect(config?.WorktreeCreate).toBeDefined();
      expect(config?.WorktreeRemove).toBeDefined();
      expect(config?.Elicitation).toBeDefined();
      expect(config?.ElicitationResult).toBeDefined();
      expect(config?.StopFailure).toBeDefined();
      expect(config?.PostCompact).toBeDefined();
      expect(config?.TaskCreated).toBeDefined();
    });

    it("should include all enforcement-critical Claude hooks", () => {
      const config = buildHooksConfig();
      for (const hookName of CLAUDE_ENFORCEMENT_CRITICAL_HOOKS) {
        expect((config as Record<string, unknown>)[hookName]).toBeDefined();
      }
    });
  });

  describe("cursor hook coverage", () => {
    it("installs all enforcement-critical Cursor hook events", async () => {
      const projectDir = await fs.mkdtemp(path.join(os.tmpdir(), "contextstream-cursor-hooks-"));
      try {
        await installCursorHookScripts({ scope: "project", projectPath: projectDir });
        const config = await readCursorHooksConfig("project", projectDir);

        for (const hookName of CURSOR_ENFORCEMENT_CRITICAL_HOOKS) {
          expect((config.hooks as Record<string, unknown>)[hookName]).toBeDefined();
        }
      } finally {
        await fs.rm(projectDir, { recursive: true, force: true });
      }
    });
  });

  describe("windsurf hook coverage", () => {
    it("installs Windsurf pre/post hook matrix", async () => {
      const projectDir = await fs.mkdtemp(path.join(os.tmpdir(), "contextstream-windsurf-hooks-"));
      try {
        await installWindsurfHookScripts({ scope: "project", projectPath: projectDir });
        const config = await readWindsurfHooksConfig("project", projectDir);

        expect(config.hooks.pre_mcp_tool_use).toBeDefined();
        expect(config.hooks.pre_user_prompt).toBeDefined();
        expect(config.hooks.pre_read_code).toBeDefined();
        expect(config.hooks.pre_write_code).toBeDefined();
        expect(config.hooks.pre_run_command).toBeDefined();
        expect(config.hooks.post_write_code).toBeDefined();
        expect(config.hooks.post_mcp_tool_use).toBeDefined();
        expect(config.hooks.post_cascade_response_with_transcript).toBeDefined();
      } finally {
        await fs.rm(projectDir, { recursive: true, force: true });
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
