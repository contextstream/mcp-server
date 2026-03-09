import { extractCwd, isConfigured, loadHookConfig, postMemoryEvent, readHookInput, writeHookOutput } from "./common.js";

export async function runStopHook(): Promise<void> {
  if (process.env.CONTEXTSTREAM_STOP_ENABLED === "false") {
    writeHookOutput();
    return;
  }

  const input = readHookInput<Record<string, unknown>>();
  const cwd = extractCwd(input);
  const config = loadHookConfig(cwd);

  if (isConfigured(config)) {
    const sessionId = (typeof input.session_id === "string" && input.session_id) || "unknown";
    const reason =
      (typeof input.reason === "string" && input.reason) ||
      (typeof input.stop_reason === "string" && input.stop_reason) ||
      "response_complete";

    await postMemoryEvent(
      config,
      "Stop checkpoint",
      {
        session_id: sessionId,
        reason,
        hook: "stop",
        timestamp: new Date().toISOString(),
        tool_name: typeof input.tool_name === "string" ? input.tool_name : null,
        model: typeof input.model === "string" ? input.model : null,
      },
      ["hook", "stop", "checkpoint"]
    ).catch(() => {});
  }

  writeHookOutput();
}

const isDirectRun = process.argv[1]?.includes("stop") || process.argv[2] === "stop";
if (isDirectRun) {
  runStopHook().catch(() => process.exit(0));
}
