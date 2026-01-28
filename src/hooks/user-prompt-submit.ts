/**
 * ContextStream UserPromptSubmit Hook - Injects rules reminder
 *
 * Injects a reminder about ContextStream rules on every message.
 * Supports multiple editor formats: Claude Code, Cursor, Cline/Roo/Kilo.
 *
 * Usage:
 *   npx @contextstream/mcp-server hook user-prompt-submit
 *
 * Input (stdin): JSON hook event data
 * Output (stdout): JSON with hookSpecificOutput/contextModification
 * Exit: Always 0
 */

const ENABLED = process.env.CONTEXTSTREAM_REMINDER_ENABLED !== "false";

const REMINDER = `[CONTEXTSTREAM RULES]
1. BEFORE Glob/Grep/Read/Search: mcp__contextstream__search(mode="hybrid") FIRST
2. Call context_smart at start of EVERY response
3. Local tools ONLY if ContextStream returns 0 results
[END RULES]`;

interface HookInput {
  // Claude Code format
  hook_event_name?: string;
  prompt?: string;
  session?: {
    messages?: Array<{ role: string; content: string | Array<{ type: string; text?: string }> }>;
  };

  // Cline/Roo/Kilo format
  hookName?: string;
}

function detectEditorFormat(input: HookInput): "claude" | "cline" | "cursor" {
  // Cline/Roo/Kilo format
  if (input.hookName !== undefined) {
    return "cline";
  }
  // Cursor format (uses hook_event_name with different casing/structure)
  if (input.hook_event_name === "beforeSubmitPrompt") {
    return "cursor";
  }
  // Default to Claude Code format
  return "claude";
}

export async function runUserPromptSubmitHook(): Promise<void> {
  if (!ENABLED) {
    process.exit(0);
  }

  // Read stdin
  let inputData = "";
  for await (const chunk of process.stdin) {
    inputData += chunk;
  }

  if (!inputData.trim()) {
    process.exit(0);
  }

  let input: HookInput;
  try {
    input = JSON.parse(inputData);
  } catch {
    process.exit(0);
  }

  const editorFormat = detectEditorFormat(input);

  // Output format depends on editor
  if (editorFormat === "claude") {
    // Claude Code format
    console.log(
      JSON.stringify({
        hookSpecificOutput: {
          hookEventName: "UserPromptSubmit",
          additionalContext: REMINDER,
        },
      })
    );
  } else if (editorFormat === "cline") {
    // Cline/Roo/Kilo format
    console.log(
      JSON.stringify({
        cancel: false,
        contextModification: REMINDER,
      })
    );
  } else if (editorFormat === "cursor") {
    // Cursor format
    console.log(
      JSON.stringify({
        continue: true,
        user_message: "[CONTEXTSTREAM] Search with mcp__contextstream__search before using Glob/Grep/Read",
      })
    );
  }

  process.exit(0);
}

// Auto-run if executed directly
const isDirectRun =
  process.argv[1]?.includes("user-prompt-submit") || process.argv[2] === "user-prompt-submit";
if (isDirectRun) {
  runUserPromptSubmitHook().catch(() => process.exit(0));
}
