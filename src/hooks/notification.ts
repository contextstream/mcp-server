import { extractCwd, isConfigured, loadHookConfig, postMemoryEvent, readHookInput, writeHookOutput } from "./common.js";

export async function runNotificationHook(): Promise<void> {
  const input = readHookInput<Record<string, unknown>>();
  const cwd = extractCwd(input);
  const config = loadHookConfig(cwd);

  if (isConfigured(config)) {
    const message =
      (typeof input.title === "string" && input.title) ||
      (typeof input.message === "string" && input.message) ||
      "Notification event";

    await postMemoryEvent(
      config,
      `Notification: ${message}`,
      {
        notification: input,
        captured_at: new Date().toISOString(),
      },
      ["hook", "notification"]
    ).catch(() => {});
  }

  writeHookOutput();
}

const isDirectRun =
  process.argv[1]?.includes("notification") || process.argv[2] === "notification";
if (isDirectRun) {
  runNotificationHook().catch(() => process.exit(0));
}
