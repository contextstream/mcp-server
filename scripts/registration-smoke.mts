process.env.CONTEXTSTREAM_API_KEY ||= "cs_test_dummy";
process.env.CONTEXTSTREAM_API_URL ||= "https://api.contextstream.io";
process.env.CONTEXTSTREAM_LOG_LEVEL = "quiet";

import { McpServer } from "@modelcontextprotocol/sdk/server/mcp.js";

const { loadConfig } = await import("./src/config.js");
const { ContextStreamClient } = await import("./src/client.js");
const { registerTools } = await import("./src/tools.js");
const { SessionManager } = await import("./src/session-manager.js");

const config = loadConfig();
const client = new ContextStreamClient(config);
const server = new McpServer({ name: "smoke", version: "0.0.0" });
const sessionManager = new SessionManager(server, client);
registerTools(server, client, sessionManager, { toolSurfaceProfile: config.toolSurfaceProfile });

const registered = (server as any)._registeredTools;
const names = registered ? Object.keys(registered).sort() : [];
console.log(`TOOL_COUNT=${names.length}`);
console.log(names.join("\n"));

const mustHave = [
  "qa",
  "capture_plan",
  "session_capture",
  "session_capture_lesson",
  "session_remember",
  "memory_create_doc",
  "memory_update_doc",
  "memory_delete_doc",
  "memory_create_task",
  "memory_update_task",
  "memory_create_todo",
  "memory_complete_todo",
  "memory_create_event",
];
const mustNotHave = ["ram", "mem", "chart", "async_job", "atlas_chart", "atlas_job"];
const missing = mustHave.filter((n) => !names.includes(n));
const forbidden = mustNotHave.filter((n) => names.includes(n));
if (missing.length || forbidden.length) {
  console.error(`SMOKE_FAIL missing=[${missing.join(",")}] forbidden=[${forbidden.join(",")}]`);
  process.exit(1);
}
console.log("SMOKE_PASS");
