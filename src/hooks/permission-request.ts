import { extractCwd, isConfigured, loadHookConfig, postMemoryEvent, readHookInput, writeHookOutput } from "./common.js";

function isHighRiskCommand(command: string): boolean {
  const lower = command.toLowerCase();
  return ["rm -rf", "git reset --hard", "mkfs", "dd if=", "shutdown", "reboot"].some((pattern) =>
    lower.includes(pattern)
  );
}

export async function runPermissionRequestHook(): Promise<void> {
  const input = readHookInput<Record<string, unknown>>();
  const cwd = extractCwd(input);
  const config = loadHookConfig(cwd);
  const command =
    (typeof input.command === "string" && input.command) ||
    (typeof input.cmd === "string" && input.cmd) ||
    "";

  if (isConfigured(config)) {
    await postMemoryEvent(
      config,
      "Permission request",
      {
        request: input,
        captured_at: new Date().toISOString(),
      },
      ["hook", "permission_request"]
    ).catch(() => {});
  }

  if (isHighRiskCommand(command)) {
    writeHookOutput({
      additionalContext:
        "High-risk command detected. Confirm scope and prefer least-privilege execution.",
    });
    return;
  }

  writeHookOutput();
}

const isDirectRun =
  process.argv[1]?.includes("permission-request") || process.argv[2] === "permission-request";
if (isDirectRun) {
  runPermissionRequestHook().catch(() => process.exit(0));
}
