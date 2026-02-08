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
  "pre-tool-use": () => import("./pre-tool-use.js"),
  "user-prompt-submit": () => import("./user-prompt-submit.js"),
  "media-aware": () => import("./media-aware.js"),
  "pre-compact": () => import("./pre-compact.js"),
  "post-compact": () => import("./post-compact.js"),
  "post-write": () => import("./post-write.js"),
  "auto-rules": () => import("./auto-rules.js"),
  "on-bash": () => import("./on-bash.js"),
  "on-task": () => import("./on-task.js"),
  "on-read": () => import("./on-read.js"),
  "on-web": () => import("./on-web.js"),
  "session-init": () => import("./session-init.js"),
  "session-end": () => import("./session-end.js"),
  "on-save-intent": () => import("./on-save-intent.js"),
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
