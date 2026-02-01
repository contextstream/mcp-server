/**
 * Fast hook runner - single entry point for all hooks
 * Usage: contextstream-hook <hook-name> [args...]
 *
 * This avoids the overhead of loading the full MCP server for hook execution.
 */

const hookName = process.argv[2];

if (!hookName) {
  console.error("Usage: contextstream-hook <hook-name>");
  console.error(
    "Available hooks: pre-tool-use, user-prompt-submit, media-aware, pre-compact, post-write, auto-rules"
  );
  process.exit(1);
}

const hooks: Record<string, () => Promise<unknown>> = {
  "pre-tool-use": () => import("./pre-tool-use.js"),
  "user-prompt-submit": () => import("./user-prompt-submit.js"),
  "media-aware": () => import("./media-aware.js"),
  "pre-compact": () => import("./pre-compact.js"),
  "post-write": () => import("./post-write.js"),
  "auto-rules": () => import("./auto-rules.js"),
};

const handler = hooks[hookName];

if (!handler) {
  console.error(`Unknown hook: ${hookName}`);
  console.error(`Available: ${Object.keys(hooks).join(", ")}`);
  process.exit(1);
}

// Execute the hook
handler().catch((err: Error) => {
  console.error(`Hook ${hookName} failed:`, err.message);
  process.exit(1);
});
