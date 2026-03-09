import * as fs from "node:fs";
import * as path from "node:path";
import { homedir } from "node:os";
import { extractCwd, isConfigured, loadHookConfig, postMemoryEvent, readHookInput, writeHookOutput } from "./common.js";

const FAILURE_COUNTERS_FILE = path.join(homedir(), ".contextstream", "hook-failure-counts.json");

function extractErrorText(input: Record<string, unknown>): string {
  return (
    (typeof input.error === "string" && input.error) ||
    (typeof input.tool_error === "string" && input.tool_error) ||
    (typeof input.stderr === "string" && input.stderr) ||
    "Tool execution failed"
  );
}

function failureFingerprint(toolName: string, errorText: string): string {
  const compact = errorText
    .split(/\s+/)
    .slice(0, 18)
    .join(" ")
    .toLowerCase();
  return `${toolName.toLowerCase()}:${compact}`;
}

function incrementFailureCounter(fingerprint: string): number {
  let counters: Record<string, number> = {};
  try {
    counters = JSON.parse(fs.readFileSync(FAILURE_COUNTERS_FILE, "utf8")) as Record<string, number>;
  } catch {
    counters = {};
  }

  counters[fingerprint] = (counters[fingerprint] || 0) + 1;
  fs.mkdirSync(path.dirname(FAILURE_COUNTERS_FILE), { recursive: true });
  fs.writeFileSync(FAILURE_COUNTERS_FILE, JSON.stringify(counters, null, 2), "utf8");
  return counters[fingerprint];
}

export async function runPostToolUseFailureHook(): Promise<void> {
  const input = readHookInput<Record<string, unknown>>();
  const cwd = extractCwd(input);
  const config = loadHookConfig(cwd);

  const toolName =
    (typeof input.tool_name === "string" && input.tool_name) ||
    (typeof input.toolName === "string" && input.toolName) ||
    "unknown";
  const errorText = extractErrorText(input);
  const toolUseId =
    (typeof input.tool_use_id === "string" && input.tool_use_id) ||
    (typeof input.toolUseId === "string" && input.toolUseId) ||
    "";
  const fingerprint = failureFingerprint(toolName, errorText);
  const count = incrementFailureCounter(fingerprint);

  if (isConfigured(config)) {
    await postMemoryEvent(
      config,
      `Tool failure: ${toolName}`,
      {
        tool_name: toolName,
        tool_use_id: toolUseId || null,
        error: errorText,
        fingerprint,
        occurrence_count: count,
        tool_input: input.tool_input || {},
        timestamp: new Date().toISOString(),
      },
      ["hook", "post_tool_use_failure", "tool_error"]
    ).catch(() => {});

    const autoLessonEnabled = process.env.CONTEXTSTREAM_FAILURE_AUTO_LESSON !== "false";
    if (autoLessonEnabled && count >= 3) {
      await postMemoryEvent(
        config,
        `Recurring failure lesson: ${toolName}`,
        {
          title: `Recurring failure in ${toolName}`,
          trigger: errorText,
          prevention: "Add guardrails or alternate fallback path for this failure mode.",
          occurrences: count,
          fingerprint,
        },
        ["hook", "lesson", "recurring_failure"],
        "lesson"
      ).catch(() => {});
    }
  }

  writeHookOutput();
}

const isDirectRun =
  process.argv[1]?.includes("post-tool-use-failure") || process.argv[2] === "post-tool-use-failure";
if (isDirectRun) {
  runPostToolUseFailureHook().catch(() => process.exit(0));
}
