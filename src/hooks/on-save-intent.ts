/**
 * ContextStream on-save-intent Hook - Redirects document saves to ContextStream
 *
 * UserPromptSubmit hook that detects when users want to save/store documents,
 * notes, decisions, or other content and injects guidance to use ContextStream
 * storage instead of local files.
 *
 * Usage:
 *   npx @contextstream/mcp-server hook on-save-intent
 *
 * Input (stdin): JSON with prompt
 * Output (stdout): JSON with hookSpecificOutput containing context injection
 * Exit: Always 0
 */

const ENABLED = process.env.CONTEXTSTREAM_SAVE_INTENT_ENABLED !== "false";

interface HookInput {
  prompt?: string;
  session?: {
    messages?: Array<{
      role: string;
      content: string | Array<{ type: string; text?: string }>;
    }>;
  };
}

// Patterns that indicate user wants to save/store something
const SAVE_PATTERNS = [
  // Direct save requests
  /\b(save|store|record|capture|log|document|write down|note down|keep track)\b.*\b(this|that|it|the)\b/i,
  /\b(save|store|record|capture|log)\b.*\b(to|in|for)\b.*\b(contextstream|memory|later|reference|future)\b/i,

  // Document creation
  /\b(create|make|write|draft)\b.*\b(a|the)\b.*\b(document|doc|note|summary|report|spec|design)\b/i,
  /\b(document|summarize|write up)\b.*\b(this|that|the|our)\b.*\b(decision|discussion|conversation|meeting|finding)\b/i,

  // Memory/reference requests
  /\b(remember|don't forget|keep in mind|note that|important to remember)\b/i,
  /\bfor\s+(future|later)\s+reference\b/i,
  /\b(add|put)\s+(this|it|that)\s+(to|in)\s+(memory|notes|docs)\b/i,

  // Decision tracking
  /\b(we\s+)?(decided|agreed|concluded|determined)\b.*\b(to|that)\b/i,
  /\blet('s|s)\s+document\b/i,
  /\bsave\s+(this|the)\s+(decision|choice|approach)\b/i,

  // Implementation/design docs
  /\b(implementation|design|architecture|spec)\s+(doc|document|plan)\b/i,
  /\bwrite\s+(the|a|an)\s+.*(md|markdown|readme)\b/i,
];

// Patterns that indicate local file save (which we want to intercept)
const LOCAL_FILE_PATTERNS = [
  /\b(save|write|create)\s+(it|this|the\s+\w+)\s+(to|in|as)\s+[./~]/i,
  /\b(save|write)\s+to\s+.*(\.md|\.txt|\.json|docs\/|notes\/)/i,
  /\bcreate\s+(a|the)\s+file\b/i,
];

function detectsSaveIntent(text: string): { hasSaveIntent: boolean; isLocalFile: boolean } {
  const hasSaveIntent = SAVE_PATTERNS.some(p => p.test(text));
  const isLocalFile = LOCAL_FILE_PATTERNS.some(p => p.test(text));
  return { hasSaveIntent, isLocalFile };
}

const SAVE_GUIDANCE = `[CONTEXTSTREAM DOCUMENT STORAGE]
The user wants to save/store content. Use ContextStream instead of local files:

**For decisions/notes:**
\`\`\`
mcp__contextstream__session(
  action="capture",
  event_type="decision|note|insight",
  title="...",
  content="...",
  importance="high|medium|low"
)
\`\`\`

**For documents/specs:**
\`\`\`
mcp__contextstream__docs(
  action="create",
  title="...",
  content="...",
  doc_type="implementation|design|spec|guide"
)
\`\`\`

**For plans:**
\`\`\`
mcp__contextstream__session(
  action="capture_plan",
  title="...",
  steps=[...]
)
\`\`\`

**Why ContextStream?**
- Persists across sessions (local files don't)
- Searchable and retrievable
- Shows up in context automatically
- Can be shared with team

Only save to local files if user explicitly requests a specific file path.
[END GUIDANCE]`;

export async function runOnSaveIntentHook(): Promise<void> {
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

  // Get the user's prompt
  let prompt = input.prompt || "";

  // If no direct prompt, try to get from session messages
  if (!prompt && input.session?.messages) {
    for (const msg of [...input.session.messages].reverse()) {
      if (msg.role === "user") {
        if (typeof msg.content === "string") {
          prompt = msg.content;
        } else if (Array.isArray(msg.content)) {
          for (const block of msg.content) {
            if (block.type === "text" && block.text) {
              prompt = block.text;
              break;
            }
          }
        }
        break;
      }
    }
  }

  if (!prompt) {
    process.exit(0);
  }

  // Check for save intent
  const { hasSaveIntent, isLocalFile } = detectsSaveIntent(prompt);

  // Only inject guidance if there's a save intent (especially for local files)
  if (hasSaveIntent || isLocalFile) {
    console.log(
      JSON.stringify({
        hookSpecificOutput: {
          hookEventName: "UserPromptSubmit",
          additionalContext: SAVE_GUIDANCE,
        },
      })
    );
  }

  process.exit(0);
}

// Auto-run if executed directly
const isDirectRun = process.argv[1]?.includes("on-save-intent") || process.argv[2] === "on-save-intent";
if (isDirectRun) {
  runOnSaveIntentHook().catch(() => process.exit(0));
}
