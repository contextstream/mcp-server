/**
 * ContextStream Media-Aware Hook
 *
 * Detects media-related prompts (video, clips, Remotion, image, audio) and
 * injects context about the media tool.
 *
 * Usage:
 *   npx @contextstream/mcp-server hook media-aware
 *
 * Input (stdin): JSON with prompt or session messages
 * Output (stdout): JSON with hookSpecificOutput containing media tool guidance
 * Exit: Always 0
 */

const ENABLED = process.env.CONTEXTSTREAM_MEDIA_HOOK_ENABLED !== "false";

// Media patterns (case-insensitive)
const PATTERNS = [
  /\b(video|videos|clip|clips|footage|keyframe)s?\b/i,
  /\b(remotion|timeline|video\s*edit)\b/i,
  /\b(image|images|photo|photos|picture|thumbnail)s?\b/i,
  /\b(audio|podcast|transcript|transcription|voice)\b/i,
  /\b(media|asset|assets|creative|b-roll)\b/i,
  /\b(find|search|show).*(clip|video|image|audio|footage|media)\b/i,
];

const MEDIA_CONTEXT = `[MEDIA TOOLS AVAILABLE]
Your workspace may have indexed media. Use ContextStream media tools:

- **Search**: \`mcp__contextstream__media(action="search", query="description")\`
- **Get clip**: \`mcp__contextstream__media(action="get_clip", content_id="...", start="1:34", end="2:15", output_format="remotion|ffmpeg|raw")\`
- **List assets**: \`mcp__contextstream__media(action="list")\`
- **Index**: \`mcp__contextstream__media(action="index", file_path="...", content_type="video|audio|image|document")\`

For Remotion: use \`output_format="remotion"\` to get frame-based props.
[END MEDIA TOOLS]`;

interface HookInput {
  prompt?: string;
  session?: {
    messages?: Array<{ role: string; content: string | Array<{ type: string; text?: string }> }>;
  };
  hookName?: string;
}

function matchesMediaPattern(text: string): boolean {
  return PATTERNS.some((pattern) => pattern.test(text));
}

function extractPrompt(input: HookInput): string {
  // Direct prompt field
  if (input.prompt) {
    return input.prompt;
  }

  // Extract from session messages (last user message)
  if (input.session?.messages) {
    for (let i = input.session.messages.length - 1; i >= 0; i--) {
      const msg = input.session.messages[i];
      if (msg.role === "user") {
        if (typeof msg.content === "string") {
          return msg.content;
        }
        if (Array.isArray(msg.content)) {
          for (const block of msg.content) {
            if (block.type === "text" && block.text) {
              return block.text;
            }
          }
        }
        break;
      }
    }
  }

  return "";
}

function detectEditorFormat(input: HookInput): "claude" | "cline" {
  if (input.hookName !== undefined) {
    return "cline";
  }
  return "claude";
}

export async function runMediaAwareHook(): Promise<void> {
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

  const prompt = extractPrompt(input);

  // Only inject media context if prompt matches media patterns
  if (!prompt || !matchesMediaPattern(prompt)) {
    process.exit(0);
  }

  const editorFormat = detectEditorFormat(input);

  if (editorFormat === "claude") {
    // Claude Code format
    console.log(
      JSON.stringify({
        hookSpecificOutput: {
          hookEventName: "UserPromptSubmit",
          additionalContext: MEDIA_CONTEXT,
        },
      })
    );
  } else {
    // Cline/Roo/Kilo format
    console.log(
      JSON.stringify({
        cancel: false,
        contextModification: MEDIA_CONTEXT,
      })
    );
  }

  process.exit(0);
}

// Auto-run if executed directly
const isDirectRun = process.argv[1]?.includes("media-aware") || process.argv[2] === "media-aware";
if (isDirectRun) {
  runMediaAwareHook().catch(() => process.exit(0));
}
