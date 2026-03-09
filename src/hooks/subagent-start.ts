import { extractCwd, fetchFastContext, isConfigured, loadHookConfig, readHookInput, writeHookOutput } from "./common.js";

const SEARCH_PROTOCOL = `[CONTEXTSTREAM SEARCH]
When searching code, prefer ContextStream search tools first:
- mcp__contextstream__search(mode="auto", query="...")
- mcp__contextstream__search(mode="keyword", query="...", include_content=true)
- mcp__contextstream__graph(...) for dependency analysis
Fall back to local discovery tools only if ContextStream search returns no results.`;

const EXPLORE_SEARCH_FIRST = `[CRITICAL: SEARCH-FIRST PROTOCOL]
You MUST call mcp__contextstream__search(mode="auto", query="...") BEFORE reading source files.
Read only the small set of files and line ranges identified by search.`;

const PLAN_SEARCH_FIRST = `[PLAN MODE: SEARCH-FIRST]
Plan mode does NOT justify file-by-file repository scans.
Start with ContextStream search, then read only the narrowed files and ranges.`;

function fallbackContext(): string {
  return `${SEARCH_PROTOCOL}\n\n[CONTEXTSTREAM] Call mcp__contextstream__context(user_message="...") for task-specific context.`;
}

export async function runSubagentStartHook(): Promise<void> {
  if (process.env.CONTEXTSTREAM_SUBAGENT_CONTEXT_ENABLED === "false") {
    writeHookOutput();
    return;
  }

  const input = readHookInput<Record<string, unknown>>();
  const cwd = extractCwd(input);
  const config = loadHookConfig(cwd);
  const agentType =
    (typeof input.agent_type === "string" && input.agent_type) ||
    (typeof input.subagent_type === "string" && input.subagent_type) ||
    "unknown";

  if (!isConfigured(config)) {
    writeHookOutput({ additionalContext: fallbackContext(), hookEventName: "SubagentStart" });
    return;
  }

  const context = await fetchFastContext(config, {
    session_id:
      (typeof input.session_id === "string" && input.session_id) ||
      (typeof input.sessionId === "string" && input.sessionId) ||
      undefined,
    user_message: `SubagentStart:${agentType}`,
  });

  const parts: string[] = [];
  if (context) parts.push(context);
  if (agentType.toLowerCase() === "explore") {
    parts.push(EXPLORE_SEARCH_FIRST);
  } else if (agentType.toLowerCase() === "plan") {
    parts.push(PLAN_SEARCH_FIRST);
    parts.push(
      `[CONTEXTSTREAM PLAN]
Save plans to ContextStream with mcp__contextstream__session(action="capture_plan", ...).
Create tasks with mcp__contextstream__memory(action="create_task", ...).`
    );
  }
  parts.push(SEARCH_PROTOCOL);

  writeHookOutput({
    additionalContext: parts.filter(Boolean).join("\n\n") || fallbackContext(),
    hookEventName: "SubagentStart",
  });
}

const isDirectRun =
  process.argv[1]?.includes("subagent-start") || process.argv[2] === "subagent-start";
if (isDirectRun) {
  runSubagentStartHook().catch(() => process.exit(0));
}
