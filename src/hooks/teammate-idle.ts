import { extractCwd, isConfigured, listPendingTasks, loadHookConfig, postMemoryEvent, readHookInput, writeHookOutput } from "./common.js";

function firstString(input: Record<string, unknown>, keys: string[]): string | undefined {
  for (const key of keys) {
    const value = input[key];
    if (typeof value === "string" && value.trim()) return value.trim();
  }
  return undefined;
}

export async function runTeammateIdleHook(): Promise<void> {
  const input = readHookInput<Record<string, unknown>>();
  const cwd = extractCwd(input);
  const config = loadHookConfig(cwd);

  const teammateName =
    firstString(input, ["teammate_name", "teammateName", "agent_name"]) || "teammate";
  const teamName = firstString(input, ["team_name", "teamName"]) || "team";

  if (!isConfigured(config)) {
    writeHookOutput();
    return;
  }

  const pendingTasks = await listPendingTasks(config, 5);

  await postMemoryEvent(
    config,
    "Teammate idle",
    {
      teammate_name: teammateName,
      team_name: teamName,
      pending_tasks: pendingTasks.slice(0, 5),
      timestamp: new Date().toISOString(),
    },
    ["hook", "teammate", "idle"]
  ).catch(() => {});

  const shouldRedirect = process.env.CONTEXTSTREAM_TEAMMATE_IDLE_REDIRECT !== "false";
  if (shouldRedirect && pendingTasks.length > 0) {
    const firstTask = pendingTasks[0] as Record<string, unknown>;
    const taskTitle =
      (typeof firstTask.title === "string" && firstTask.title) ||
      (typeof firstTask.subject === "string" && firstTask.subject) ||
      (typeof firstTask.name === "string" && firstTask.name) ||
      "pending task";
    const message = `Pending ContextStream task available: ${taskTitle}. Continue and complete this task before idling.`;
    writeHookOutput({ additionalContext: message, blocked: true, reason: message });
    return;
  }

  writeHookOutput();
}

const isDirectRun =
  process.argv[1]?.includes("teammate-idle") || process.argv[2] === "teammate-idle";
if (isDirectRun) {
  runTeammateIdleHook().catch(() => process.exit(0));
}
