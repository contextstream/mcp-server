/**
 * Fast hook runner - single entry point for all hooks
 * Usage: contextstream-hook <hook-name> [args...]
 *
 * This avoids the overhead of loading the full MCP server for hook execution.
 *
 * IMPORTANT: Unknown hooks exit 0 (not 1) to avoid showing "hook error" in
 * editors when users have outdated hooks from a previous version. A silently
 * succeeding hook is far better UX than a broken error message.
 */

const hookName = process.argv[2];

if (!hookName) {
  // No hook name = likely misconfigured, but don't break the editor
  process.exit(0);
}

const hooks: Record<string, () => Promise<unknown>> = {
  "pre-tool-use": async () => (await import("./pre-tool-use.js")).runPreToolUseHook(),
  "post-tool-use": async () => (await import("./post-write.js")).runPostWriteHook(),
  "user-prompt-submit": async () => (await import("./user-prompt-submit.js")).runUserPromptSubmitHook(),
  "media-aware": async () => (await import("./noop.js")).runNoopHook(),
  "pre-compact": async () => (await import("./pre-compact.js")).runPreCompactHook(),
  "post-compact": async () => (await import("./post-compact.js")).runPostCompactHook(),
  "post-write": async () => (await import("./post-write.js")).runPostWriteHook(),
  "auto-rules": async () => (await import("./noop.js")).runNoopHook(),
  "on-bash": async () => (await import("./noop.js")).runNoopHook(),
  "on-task": async () => (await import("./noop.js")).runNoopHook(),
  "on-read": async () => (await import("./noop.js")).runNoopHook(),
  "on-web": async () => (await import("./noop.js")).runNoopHook(),
  "session-start": async () => (await import("./session-init.js")).runSessionInitHook(),
  "session-init": async () => (await import("./session-init.js")).runSessionInitHook(),
  stop: async () => (await import("./stop.js")).runStopHook(),
  "session-end": async () => (await import("./session-end.js")).runSessionEndHook(),
  notification: async () => (await import("./notification.js")).runNotificationHook(),
  "permission-request": async () => (await import("./permission-request.js")).runPermissionRequestHook(),
  "post-tool-use-failure": async () => (await import("./post-tool-use-failure.js")).runPostToolUseFailureHook(),
  "instructions-loaded": async () => (await import("./noop.js")).runNoopHook(),
  "config-change": async () => (await import("./noop.js")).runNoopHook(),
  "cwd-changed": async () => (await import("./noop.js")).runNoopHook(),
  "file-changed": async () => (await import("./noop.js")).runNoopHook(),
  "worktree-create": async () => (await import("./noop.js")).runNoopHook(),
  "worktree-remove": async () => (await import("./noop.js")).runNoopHook(),
  elicitation: async () => (await import("./noop.js")).runNoopHook(),
  "elicitation-result": async () => (await import("./noop.js")).runNoopHook(),
  "subagent-start": async () => (await import("./subagent-start.js")).runSubagentStartHook(),
  "subagent-stop": async () => (await import("./subagent-stop.js")).runSubagentStopHook(),
  "task-created": async () => (await import("./noop.js")).runNoopHook(),
  "task-completed": async () => (await import("./task-completed.js")).runTaskCompletedHook(),
  "teammate-idle": async () => (await import("./teammate-idle.js")).runTeammateIdleHook(),
  "on-save-intent": async () => (await import("./on-save-intent.js")).runOnSaveIntentHook(),
};

const handler = hooks[hookName];

if (!handler) {
  // Unknown hook - exit 0 to avoid "hook error" in editors.
  // This happens when users have hooks from a newer version but run an older cached binary.
  process.exit(0);
}

// Execute the hook - exit 0 on failure to avoid breaking the editor
handler().catch(() => {
  process.exit(0);
});
