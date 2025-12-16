/**
 * Editor-specific rule templates for ContextStream integration.
 * These instruct AI assistants to automatically use ContextStream for memory and context.
 */

export interface RuleTemplate {
  filename: string;
  content: string;
  description: string;
}

const CONTEXTSTREAM_RULES = `
## ContextStream Integration

You have access to ContextStream MCP tools for persistent memory and context.

## TL;DR - REQUIRED EVERY MESSAGE

| Message | What to Call |
|---------|--------------|
| **1st message** | \`session_init(folder_path="...", context_hint="<user's message>")\` |
| **2nd+ messages** | \`context_smart(user_message="<user's message>")\` |
| **After completing task** | \`session_capture(...)\` - MUST capture decisions/insights |
| **User frustration/correction** | \`session_capture_lesson(...)\` - MUST capture lessons |
| **Command/tool error + fix** | \`session_capture_lesson(...)\` - MUST capture lessons |

**NO EXCEPTIONS.** Do not skip even if you think you have enough context.

---

### ⚠️ Why context_smart is Required (Even After session_init)

**Common mistake:** "session_init already gave me context, I don't need context_smart"

**This is WRONG. Here's why:**
- \`session_init\` returns the last ~10 items **BY TIME** (chronological)
- \`context_smart\` **SEARCHES** for items **RELEVANT to THIS message** (semantic)

**Example failure:**
- User asks: "how should I implement authentication?"
- Auth decisions were made 20 conversations ago
- ❌ \`session_init\` won't have it (too old, not in recent 10)
- ✅ \`context_smart\` FINDS it via semantic search

**Without context_smart, you WILL miss relevant older context.**

---

### When to Capture (MANDATORY)

| When | Tool | Example |
|------|------|---------|
| User makes a decision | \`session_capture\` | "Let's use PostgreSQL" → capture as decision |
| User states preference | \`session_capture\` | "I prefer TypeScript" → capture as preference |
| You complete a task | \`session_capture\` | Capture what was done, decisions made |
| Need past context | \`session_recall\` | "What did we decide about X?" |

**You MUST capture after completing any significant task.** This ensures future sessions have context.

---

### Behavior Rules

**First message of conversation:**
1. Call \`session_init(folder_path="<cwd>", context_hint="<user's message>")\`
2. Then respond

**Every subsequent message:**
1. Call \`context_smart(user_message="<user's message>")\` FIRST
2. Then respond

**After completing a task:**
1. Call \`session_capture\` to save decisions, preferences, or insights
2. This is NOT optional

**When user asks about past decisions:**
- Use \`session_recall\` - do NOT ask user to repeat themselves

---

### Lesson Capture (MANDATORY)

When:
1. **Expresses frustration** (caps, profanity, "COME ON", "WTF", repeated corrections)
2. **Corrects you** ("No, you should...", "That's wrong", "Fix this")
3. **Points out a mistake** (broken code, wrong approach, production issue)
4. **A command/tool call fails and you learn the correct fix** (even if the user didn’t explicitly correct you)

You MUST immediately call \`session_capture_lesson\` with:

| Field | Description | Example |
|-------|-------------|---------|
| \`title\` | What to remember | "Verify assets in git before pushing" |
| \`severity\` | \`critical\`/\`high\`/\`medium\`/\`low\` | \`critical\` for production issues |
| \`category\` | \`workflow\`/\`code_quality\`/\`verification\`/\`communication\`/\`project_specific\` | \`workflow\` |
| \`trigger\` | What action caused the problem | "Pushed code referencing images without committing them" |
| \`impact\` | What went wrong | "Production 404 errors - broken landing page" |
| \`prevention\` | How to prevent in future | "Run git status to check untracked files before pushing" |
| \`keywords\` | Keywords for matching | \`["git", "images", "assets", "push"]\` |

**Example call:**
\`\`\`json
{
  "title": "Always verify assets in git before pushing code references",
  "severity": "critical",
  "category": "workflow",
  "trigger": "Pushed code referencing /screenshots/*.png without committing images",
  "impact": "Production 404 errors - broken landing page",
  "prevention": "Run 'git status' to check untracked files before pushing code that references static assets",
  "keywords": ["git", "images", "assets", "push", "404", "static"]
}
\`\`\`

**Why this matters:**
- Lessons are surfaced automatically in \`session_init\` and \`context_smart\`
- Future sessions will warn you before repeating the same mistake
- This prevents production issues and user frustration

**Severity guide:**
- \`critical\`: Production outages, data loss, security issues
- \`high\`: Breaking changes, significant user impact
- \`medium\`: Workflow inefficiencies, minor bugs
- \`low\`: Style/preference corrections

---

### Quick Examples

\`\`\`
# First message - user asks about auth
session_init(folder_path="/path/to/project", context_hint="how should I implement auth?")
# Returns workspace info + semantically relevant auth decisions from ANY time

# Second message - user asks about database
context_smart(user_message="what database should I use?")
# Returns: W:Maker|P:myproject|D:Use PostgreSQL|D:No ORMs|M:DB schema at...

# User says "Let's use Redis for caching"
session_capture(event_type="decision", title="Caching Choice", content="Using Redis for caching layer")

# After completing implementation
session_capture(event_type="decision", title="Auth Implementation Complete", content="Implemented JWT auth with refresh tokens...")

# Check past decisions
session_recall(query="what did we decide about caching?")
\`\`\`
`.trim();

export const TEMPLATES: Record<string, RuleTemplate> = {
  windsurf: {
    filename: '.windsurfrules',
    description: 'Windsurf AI rules',
    content: `# Windsurf Rules
${CONTEXTSTREAM_RULES}
`,
  },

  cursor: {
    filename: '.cursorrules',
    description: 'Cursor AI rules', 
    content: `# Cursor Rules
${CONTEXTSTREAM_RULES}
`,
  },

  cline: {
    filename: '.clinerules',
    description: 'Cline AI rules',
    content: `# Cline Rules
${CONTEXTSTREAM_RULES}
`,
  },

  kilo: {
    filename: '.kilocode/rules/contextstream.md',
    description: 'Kilo Code AI rules',
    content: `# Kilo Code Rules
${CONTEXTSTREAM_RULES}
`,
  },

  roo: {
    filename: '.roo/rules/contextstream.md',
    description: 'Roo Code AI rules',
    content: `# Roo Code Rules
${CONTEXTSTREAM_RULES}
`,
  },

  claude: {
    filename: 'CLAUDE.md',
    description: 'Claude Code instructions',
    content: `# Claude Code Instructions
${CONTEXTSTREAM_RULES}
`,
  },

  aider: {
    filename: '.aider.conf.yml',
    description: 'Aider configuration with system prompt',
    content: `# Aider Configuration
# Note: Aider uses different config format - this adds to the system prompt

# Add ContextStream guidance to conventions
conventions: |
${CONTEXTSTREAM_RULES.split('\n').map(line => '  ' + line).join('\n')}
`,
  },
};

/**
 * Get all available editor types
 */
export function getAvailableEditors(): string[] {
  return Object.keys(TEMPLATES);
}

/**
 * Get template for a specific editor
 */
export function getTemplate(editor: string): RuleTemplate | null {
  return TEMPLATES[editor.toLowerCase()] || null;
}

/**
 * Generate rule content with workspace-specific customizations
 */
export function generateRuleContent(
  editor: string,
  options?: {
    workspaceName?: string;
    workspaceId?: string;
    projectName?: string;
    additionalRules?: string;
  }
): { filename: string; content: string } | null {
  const template = getTemplate(editor);
  if (!template) return null;

  let content = template.content;

  // Add workspace header if provided
  if (options?.workspaceName || options?.projectName) {
    const header = `
# Workspace: ${options.workspaceName || 'Unknown'}
${options.projectName ? `# Project: ${options.projectName}` : ''}
${options.workspaceId ? `# Workspace ID: ${options.workspaceId}` : ''}

`;
    content = header + content;
  }

  // Append additional rules if provided
  if (options?.additionalRules) {
    content += '\n\n## Project-Specific Rules\n\n' + options.additionalRules;
  }

  return {
    filename: template.filename,
    content: content.trim() + '\n',
  };
}

/**
 * Generate all rule files for a project
 */
export function generateAllRuleFiles(options?: {
  workspaceName?: string;
  workspaceId?: string;
  projectName?: string;
  additionalRules?: string;
}): Array<{ editor: string; filename: string; content: string }> {
  return getAvailableEditors()
    .map(editor => {
      const result = generateRuleContent(editor, options);
      if (!result) return null;
      return { editor, ...result };
    })
    .filter((r): r is { editor: string; filename: string; content: string } => r !== null);
}
