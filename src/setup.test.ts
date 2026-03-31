import * as fs from "node:fs/promises";
import * as os from "node:os";
import * as path from "node:path";

import { afterEach, describe, expect, it } from "vitest";

import { __setupTestUtils } from "./setup.js";

describe("OpenCode MCP setup", () => {
  const tempDirs: string[] = [];

  afterEach(async () => {
    await Promise.all(tempDirs.map((dir) => fs.rm(dir, { recursive: true, force: true })));
    tempDirs.length = 0;
  });

  it("builds the default local OpenCode server config", () => {
    const server = __setupTestUtils.buildContextStreamOpenCodeLocalServer({
      apiUrl: "https://api.contextstream.io",
      apiKey: "test-key",
      contextPackEnabled: true,
    });

    expect(server).toEqual({
      type: "local",
      command: ["npx", "-y", "contextstream-mcp"],
      environment: {
        CONTEXTSTREAM_API_KEY: "{env:CONTEXTSTREAM_API_KEY}",
      },
      enabled: true,
    });
  });

  it("adds optional OpenCode environment overrides only when needed", () => {
    const server = __setupTestUtils.buildContextStreamOpenCodeLocalServer({
      apiUrl: "https://self-hosted.contextstream.test",
      apiKey: "test-key",
      contextPackEnabled: false,
    });

    expect(server.environment).toEqual({
      CONTEXTSTREAM_API_KEY: "{env:CONTEXTSTREAM_API_KEY}",
      CONTEXTSTREAM_API_URL: "https://self-hosted.contextstream.test",
      CONTEXTSTREAM_CONTEXT_PACK: "false",
    });
  });

  it("builds the remote OpenCode server config", () => {
    expect(__setupTestUtils.buildContextStreamOpenCodeRemoteServer()).toEqual({
      type: "remote",
      url: "https://mcp.contextstream.com",
      enabled: true,
    });
  });

  it("upserts OpenCode config with schema and preserves other settings", async () => {
    const tempDir = await fs.mkdtemp(path.join(os.tmpdir(), "contextstream-opencode-"));
    tempDirs.push(tempDir);

    const configPath = path.join(tempDir, "opencode.json");
    await fs.writeFile(
      configPath,
      `{
  // Existing OpenCode settings
  "theme": "dark",
  "mcp": {
    "other": {
      "type": "local",
      "command": ["echo", "hello"]
    },
  },
}
`,
      "utf8"
    );

    const server = __setupTestUtils.buildContextStreamOpenCodeLocalServer({
      apiUrl: "https://api.contextstream.io",
      apiKey: "test-key",
      contextPackEnabled: true,
    });

    const firstStatus = await __setupTestUtils.upsertOpenCodeMcpConfig(configPath, server);
    expect(firstStatus).toBe("updated");

    const written = JSON.parse(await fs.readFile(configPath, "utf8"));
    expect(written).toMatchObject({
      $schema: "https://opencode.ai/config.json",
      theme: "dark",
      mcp: {
        other: {
          type: "local",
          command: ["echo", "hello"],
        },
        contextstream: server,
      },
    });

    const secondStatus = await __setupTestUtils.upsertOpenCodeMcpConfig(configPath, server);
    expect(secondStatus).toBe("skipped");
  });
});

describe("Copilot MCP config automation", () => {
  const tempDirs: string[] = [];

  afterEach(async () => {
    await Promise.all(tempDirs.map((dir) => fs.rm(dir, { recursive: true, force: true })));
    tempDirs.length = 0;
  });

  it("writes canonical global and project config shapes", async () => {
    const tempDir = await fs.mkdtemp(path.join(os.tmpdir(), "contextstream-copilot-"));
    tempDirs.push(tempDir);

    const globalPath = path.join(tempDir, "mcp-config.json");
    const projectPath = path.join(tempDir, ".vscode", "mcp.json");
    const mcpServer = __setupTestUtils.buildContextStreamMcpServer({
      apiUrl: "https://api.contextstream.io",
      apiKey: "test-key",
      contextPackEnabled: true,
    });
    const vsCodeServer = __setupTestUtils.buildContextStreamVsCodeServer({
      apiUrl: "https://api.contextstream.io",
      apiKey: "test-key",
      contextPackEnabled: true,
    });

    await __setupTestUtils.upsertJsonMcpConfig(globalPath, mcpServer);
    await __setupTestUtils.upsertJsonVsCodeMcpConfig(projectPath, vsCodeServer);

    const globalConfig = JSON.parse(await fs.readFile(globalPath, "utf8"));
    expect(globalConfig.mcpServers.contextstream).toBeTruthy();

    const projectConfig = JSON.parse(await fs.readFile(projectPath, "utf8"));
    expect(projectConfig.servers.contextstream).toMatchObject({ type: "stdio" });
    expect(projectConfig.mcpServers).toBeUndefined();
  });

  it("is idempotent for canonical copilot writes", async () => {
    const tempDir = await fs.mkdtemp(path.join(os.tmpdir(), "contextstream-copilot-idempotent-"));
    tempDirs.push(tempDir);

    const globalPath = path.join(tempDir, "mcp-config.json");
    const projectPath = path.join(tempDir, ".vscode", "mcp.json");
    const mcpServer = __setupTestUtils.buildContextStreamMcpServer({
      apiUrl: "https://api.contextstream.io",
      apiKey: "test-key",
      contextPackEnabled: true,
    });
    const vsCodeServer = __setupTestUtils.buildContextStreamVsCodeServer({
      apiUrl: "https://api.contextstream.io",
      apiKey: "test-key",
      contextPackEnabled: true,
    });

    await __setupTestUtils.upsertJsonMcpConfig(globalPath, mcpServer);
    await __setupTestUtils.upsertJsonVsCodeMcpConfig(projectPath, vsCodeServer);
    const globalSecond = await __setupTestUtils.upsertJsonMcpConfig(globalPath, mcpServer);
    const projectSecond = await __setupTestUtils.upsertJsonVsCodeMcpConfig(projectPath, vsCodeServer);

    expect(globalSecond).toBe("skipped");
    expect(projectSecond).toBe("skipped");
  });
});
