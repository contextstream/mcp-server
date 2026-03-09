import {
  createTask,
  extractCwd,
  isConfigured,
  loadHookConfig,
  postMemoryEvent,
  readHookInput,
  updateTaskStatus,
  writeHookOutput,
} from "./common.js";

function firstString(input: Record<string, unknown>, keys: string[]): string | undefined {
  for (const key of keys) {
    const value = input[key];
    if (typeof value === "string" && value.trim()) return value.trim();
  }
  const task = input.task;
  if (task && typeof task === "object") {
    return firstString(task as Record<string, unknown>, keys);
  }
  return undefined;
}

function looksLikeRecoveryTask(description: string): boolean {
  const lower = description.toLowerCase();
  return ["error", "failure", "retry", "recover", "fix", "incident"].some((keyword) =>
    lower.includes(keyword)
  );
}

export async function runTaskCompletedHook(): Promise<void> {
  const input = readHookInput<Record<string, unknown>>();
  const cwd = extractCwd(input);
  const config = loadHookConfig(cwd);

  const taskId = firstString(input, ["task_id", "taskId"]) || "";
  const taskSubject =
    firstString(input, ["task_subject", "task_title", "title", "subject", "description"]) ||
    "Completed task";
  const taskDescription = firstString(input, ["task_description", "details", "content"]);
  const planId = firstString(input, ["plan_id", "planId"]);

  if (process.env.CONTEXTSTREAM_TASK_COMPLETED_REQUIRE_SUBJECT === "true" && !taskSubject.trim()) {
    writeHookOutput({ blocked: true, reason: "TaskCompleted requires a non-empty task subject" });
    return;
  }

  if (isConfigured(config)) {
    let updated = false;
    if (taskId) {
      updated = await updateTaskStatus(
        config,
        taskId,
        "completed",
        taskSubject,
        taskDescription
      );
    }

    if (!updated) {
      await createTask(config, {
        title: taskSubject,
        description: taskDescription,
        planId: planId || undefined,
        status: "completed",
      });
    }

    await postMemoryEvent(
      config,
      "Task completed",
      {
        task_id: taskId || null,
        title: taskSubject,
        description: taskDescription,
        plan_id: planId || null,
        agent_id: firstString(input, ["agent_id", "agentId"]) || null,
        team_name: firstString(input, ["team_name", "teamName"]) || null,
        completed_at: new Date().toISOString(),
        source: "task_completed_hook",
      },
      ["hook", "task", "completed"],
      "task"
    ).catch(() => {});

    if (taskDescription && looksLikeRecoveryTask(taskDescription)) {
      await postMemoryEvent(
        config,
        "Lesson from task completion",
        {
          task: taskSubject,
          description: taskDescription,
          lesson: "Recovered from an execution issue; consider codifying this into tests/guards.",
        },
        ["hook", "lesson", "task_completed"],
        "lesson"
      ).catch(() => {});
    }
  }

  writeHookOutput();
}

const isDirectRun =
  process.argv[1]?.includes("task-completed") || process.argv[2] === "task-completed";
if (isDirectRun) {
  runTaskCompletedHook().catch(() => process.exit(0));
}
