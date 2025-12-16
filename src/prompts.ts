import type { McpServer } from '@modelcontextprotocol/sdk/server/mcp.js';
import { z } from 'zod';

export function registerPrompts(server: McpServer) {
  // Code exploration prompt
  server.registerPrompt(
    'explore-codebase',
    {
      title: 'Explore Codebase',
      description: 'Get an overview of a project codebase structure and key components',
      argsSchema: {
        project_id: z.string().uuid().optional().describe('Project ID to explore (optional if session_init has set defaults)'),
        focus_area: z.string().optional().describe('Optional area to focus on (e.g., "authentication", "api routes")'),
      },
    },
    async (args) => ({
      messages: [
        {
          role: 'user',
          content: {
            type: 'text',
            text: `I want to understand the codebase${args.project_id ? ` for project ${args.project_id}` : ''}${args.focus_area ? ` with focus on ${args.focus_area}` : ''}.

If project_id is not provided, first call \`session_init\` (or \`projects_list\`) to resolve the current project ID.

Please help me by:
1. First, use \`projects_overview\` to get the project summary
2. Use \`projects_files\` to list the key files
3. Use \`search_semantic\` to find relevant code${args.focus_area ? ` related to "${args.focus_area}"` : ''}
4. Summarize the architecture and key patterns you observe

Provide a clear, structured overview that helps me navigate this codebase effectively.`,
          },
        },
      ],
    })
  );

  // Memory capture prompt
  server.registerPrompt(
    'capture-decision',
    {
      title: 'Capture Decision',
      description: 'Document an architectural or technical decision in workspace memory',
      argsSchema: {
        workspace_id: z.string().uuid().optional().describe('Workspace ID (optional if session_init has set defaults)'),
        decision_title: z.string().describe('Brief title of the decision'),
        context: z.string().describe('What prompted this decision'),
        decision: z.string().describe('The decision made'),
        consequences: z.string().optional().describe('Expected consequences or tradeoffs'),
      },
    },
    async (args) => ({
      messages: [
        {
          role: 'user',
          content: {
            type: 'text',
            text: `Please document the following decision in workspace memory:

**Title:** ${args.decision_title}
**Context:** ${args.context}
**Decision:** ${args.decision}
${args.consequences ? `**Consequences:** ${args.consequences}` : ''}

If workspace_id is not provided, first call \`session_init\` to resolve the workspace, then capture the decision.

Use \`session_capture\` with:
- event_type: "decision"
${args.workspace_id ? `- workspace_id: "${args.workspace_id}"` : '- workspace_id: (omit to use session defaults)'}
- title: "${args.decision_title}"
- content: A well-formatted ADR (Architecture Decision Record) with context, decision, and consequences
- tags: Include relevant tags (e.g., "adr", "architecture")
- importance: "high"

After creating, confirm the decision was recorded and summarize it.`,
          },
        },
      ],
    })
  );

  // Code review context prompt
  server.registerPrompt(
    'review-context',
    {
      title: 'Code Review Context',
      description: 'Build context for reviewing code changes',
      argsSchema: {
        project_id: z.string().uuid().optional().describe('Project ID (optional if session_init has set defaults)'),
        file_paths: z.string().describe('Comma-separated file paths being changed'),
        change_description: z.string().describe('Brief description of the changes'),
      },
    },
    async (args) => ({
      messages: [
        {
          role: 'user',
          content: {
            type: 'text',
            text: `I need context to review changes in these files: ${args.file_paths}

Change description: ${args.change_description}

Please help me understand the impact by:
1. Use \`graph_dependencies\` to find what depends on these files
2. Use \`graph_impact\` to analyze potential impact
3. Use \`memory_search\` to find related decisions or notes about these areas
4. Use \`search_semantic\` to find related code patterns

Provide:
- Summary of what these files do
- What other parts of the codebase might be affected
- Any relevant past decisions or context from memory
- Potential risks or areas to focus the review on`,
          },
        },
      ],
    })
  );

  // Debug investigation prompt
  server.registerPrompt(
    'investigate-bug',
    {
      title: 'Investigate Bug',
      description: 'Build context for debugging an issue',
      argsSchema: {
        project_id: z.string().uuid().optional().describe('Project ID (optional if session_init has set defaults)'),
        error_message: z.string().describe('Error message or symptom'),
        affected_area: z.string().optional().describe('Known affected area or component'),
      },
    },
    async (args) => ({
      messages: [
        {
          role: 'user',
          content: {
            type: 'text',
            text: `I'm investigating a bug:

**Error/Symptom:** ${args.error_message}
${args.affected_area ? `**Affected Area:** ${args.affected_area}` : ''}

Please help me investigate by:
1. Use \`search_semantic\` to find code related to this error
2. Use \`search_pattern\` to find where similar errors are thrown
3. Use \`graph_call_path\` to trace call flows if we identify key functions
4. Use \`memory_search\` to check if this issue has been encountered before

Provide:
- Likely locations where this error originates
- Call flow analysis
- Any related past issues from memory
- Suggested debugging approach`,
          },
        },
      ],
    })
  );

  // Knowledge graph exploration prompt
  server.registerPrompt(
    'explore-knowledge',
    {
      title: 'Explore Knowledge Graph',
      description: 'Navigate and understand the knowledge graph for a workspace',
      argsSchema: {
        workspace_id: z.string().uuid().optional().describe('Workspace ID (optional if session_init has set defaults)'),
        starting_topic: z.string().optional().describe('Topic to start exploration from'),
      },
    },
    async (args) => ({
      messages: [
        {
          role: 'user',
          content: {
            type: 'text',
            text: `Help me explore the knowledge captured in the current workspace${args.workspace_id ? ` (${args.workspace_id})` : ''}${args.starting_topic ? ` starting from "${args.starting_topic}"` : ''}.

If workspace_id is not provided, first call \`session_init\` to resolve the workspace.

Please:
1. Use \`memory_list_nodes\` to see available knowledge nodes
2. Use \`memory_decisions\` to see decision history
3. ${args.starting_topic ? `Use \`memory_search\` to find nodes related to "${args.starting_topic}"` : 'Use \`memory_summary\` to get an overview'}
4. Use \`graph_related\` to explore connections between nodes

Provide:
- Overview of knowledge captured
- Key themes and topics
- Important decisions and their rationale
- Connections between different pieces of knowledge`,
          },
        },
      ],
    })
  );

  // Onboarding context prompt
  server.registerPrompt(
    'onboard-to-project',
    {
      title: 'Project Onboarding',
      description: 'Generate onboarding context for a new team member',
      argsSchema: {
        project_id: z.string().uuid().optional().describe('Project ID (optional if session_init has set defaults)'),
        workspace_id: z.string().uuid().optional().describe('Workspace ID (optional if session_init has set defaults)'),
        role: z.string().optional().describe('Role of the person being onboarded (e.g., "backend developer", "frontend developer")'),
      },
    },
    async (args) => ({
      messages: [
        {
          role: 'user',
          content: {
            type: 'text',
            text: `Create an onboarding guide for a new team member joining this project.
${args.role ? `They will be working as a ${args.role}.` : ''}

Please gather comprehensive context:
1. Use \`projects_overview\` and \`projects_statistics\` for project summary
2. Use \`projects_files\` to identify key entry points
3. Use \`memory_timeline\` to see recent activity and changes
4. Use \`memory_decisions\` to understand key architectural choices
5. Use \`search_semantic\` to find documentation and READMEs

Provide an onboarding guide that includes:
- Project overview and purpose
- Technology stack and architecture
- Key files and entry points${args.role ? ` relevant to ${args.role}` : ''}
- Important decisions and their rationale
- Recent changes and current focus areas
- Getting started steps`,
          },
        },
      ],
    })
  );

  // Refactoring analysis prompt
  server.registerPrompt(
    'analyze-refactoring',
    {
      title: 'Refactoring Analysis',
      description: 'Analyze a codebase for refactoring opportunities',
      argsSchema: {
        project_id: z.string().uuid().optional().describe('Project ID (optional if session_init has set defaults)'),
        target_area: z.string().optional().describe('Specific area to analyze'),
      },
    },
    async (args) => ({
      messages: [
        {
          role: 'user',
          content: {
            type: 'text',
            text: `Analyze the codebase for refactoring opportunities${args.target_area ? ` in ${args.target_area}` : ''}.

Please investigate:
1. Use \`graph_circular_dependencies\` to find circular dependencies
2. Use \`graph_unused_code\` to find dead code
3. Use \`search_pattern\` to find code duplication patterns
4. Use \`projects_statistics\` to identify complex areas

Provide:
- Circular dependencies that should be broken
- Unused code that can be removed
- Duplicate patterns that could be consolidated
- High complexity areas that need simplification
- Prioritized refactoring recommendations`,
          },
        },
      ],
    })
  );

  // AI context building prompt
  server.registerPrompt(
    'build-context',
    {
      title: 'Build LLM Context',
      description: 'Build comprehensive context for an LLM task',
      argsSchema: {
        query: z.string().describe('What you need context for'),
        workspace_id: z.string().uuid().optional().describe('Workspace ID'),
        project_id: z.string().uuid().optional().describe('Project ID'),
        include_memory: z.string().optional().describe('Include memory/decisions ("true" or "false")'),
      },
    },
    async (args) => ({
      messages: [
        {
          role: 'user',
          content: {
            type: 'text',
            text: `Build comprehensive context for the following task:

**Query:** ${args.query}

Please use \`ai_enhanced_context\` with:
- query: "${args.query}"
${args.workspace_id ? `- workspace_id: "${args.workspace_id}"` : ''}
${args.project_id ? `- project_id: "${args.project_id}"` : ''}
- include_code: true
- include_docs: true
- include_memory: ${args.include_memory ?? true}

Then synthesize the retrieved context into a coherent briefing that will help with the task.`,
          },
        },
      ],
    })
  );

  // Smart search prompt (memory + code)
  server.registerPrompt(
    'smart-search',
    {
      title: 'Smart Search',
      description: 'Search across memory, decisions, and code for a query',
      argsSchema: {
        query: z.string().describe('What you want to find'),
        workspace_id: z.string().uuid().optional().describe('Workspace ID (optional)'),
        project_id: z.string().uuid().optional().describe('Project ID (optional)'),
      },
    },
    async (args) => ({
      messages: [
        {
          role: 'user',
          content: {
            type: 'text',
            text: `Find the most relevant context for: "${args.query}"

If workspace_id/project_id are not provided, call \`session_init\` first to resolve the current workspace/project.

Please:
1. Use \`session_smart_search\` with query "${args.query}"${args.workspace_id ? ` and workspace_id "${args.workspace_id}"` : ''}${args.project_id ? ` and project_id "${args.project_id}"` : ''}
2. If results are thin, follow up with \`search_hybrid\` and \`memory_search\`
3. Return the top results with file paths/links and a short synthesis of what matters`,
          },
        },
      ],
    })
  );

  // Recall prompt (decisions/memory)
  server.registerPrompt(
    'recall-context',
    {
      title: 'Recall Context',
      description: 'Retrieve relevant past decisions and memory for a query',
      argsSchema: {
        query: z.string().describe('What to recall (natural language)'),
        workspace_id: z.string().uuid().optional().describe('Workspace ID (optional)'),
        project_id: z.string().uuid().optional().describe('Project ID (optional)'),
      },
    },
    async (args) => ({
      messages: [
        {
          role: 'user',
          content: {
            type: 'text',
            text: `Recall relevant context for: "${args.query}"

If workspace_id/project_id are not provided, call \`session_init\` first to resolve the current workspace/project.

Use \`session_recall\` with query "${args.query}"${args.workspace_id ? ` and workspace_id "${args.workspace_id}"` : ''}${args.project_id ? ` and project_id "${args.project_id}"` : ''}.
Then summarize the key points and any relevant decisions/lessons.`,
          },
        },
      ],
    })
  );

  // Session summary prompt
  server.registerPrompt(
    'session-summary',
    {
      title: 'Session Summary',
      description: 'Get a compact summary of workspace/project context',
      argsSchema: {
        workspace_id: z.string().uuid().optional().describe('Workspace ID (optional)'),
        project_id: z.string().uuid().optional().describe('Project ID (optional)'),
        max_tokens: z.string().optional().describe('Max tokens for summary (default: 500)'),
      },
    },
    async (args) => ({
      messages: [
        {
          role: 'user',
          content: {
            type: 'text',
            text: `Generate a compact, token-efficient summary of the current workspace/project context.

If workspace_id/project_id are not provided, call \`session_init\` first to resolve the current workspace/project.

Use \`session_summary\`${args.workspace_id ? ` with workspace_id "${args.workspace_id}"` : ''}${args.project_id ? ` and project_id "${args.project_id}"` : ''}${args.max_tokens ? ` and max_tokens ${args.max_tokens} (number)` : ''}.
Then list the top decisions (titles only) and any high-priority lessons to watch for.`,
          },
        },
      ],
    })
  );

  // Lesson capture prompt
  server.registerPrompt(
    'capture-lesson',
    {
      title: 'Capture Lesson',
      description: 'Record a lesson learned from an error or correction',
      argsSchema: {
        workspace_id: z.string().uuid().optional().describe('Workspace ID (optional)'),
        project_id: z.string().uuid().optional().describe('Project ID (optional)'),
        title: z.string().describe('Lesson title (what to remember)'),
        severity: z.string().optional().describe('low|medium|high|critical (default: medium)'),
        category: z.string().describe('workflow|code_quality|verification|communication|project_specific'),
        trigger: z.string().describe('What action caused the problem'),
        impact: z.string().describe('What went wrong'),
        prevention: z.string().describe('How to prevent in future'),
        keywords: z.string().optional().describe('Comma-separated keywords (optional)'),
      },
    },
    async (args) => ({
      messages: [
        {
          role: 'user',
          content: {
            type: 'text',
            text: `Capture this lesson so it is surfaced in future sessions:

Title: ${args.title}
Severity: ${args.severity || 'medium'}
Category: ${args.category}
Trigger: ${args.trigger}
Impact: ${args.impact}
Prevention: ${args.prevention}
${args.keywords ? `Keywords: ${args.keywords}` : ''}

If workspace_id/project_id are not provided, call \`session_init\` first to resolve the current workspace/project.

Use \`session_capture_lesson\` with the fields above. If keywords were provided, split the comma-separated list into an array of strings.`,
          },
        },
      ],
    })
  );

  // Preference capture prompt
  server.registerPrompt(
    'capture-preference',
    {
      title: 'Capture Preference',
      description: 'Save a user preference to memory',
      argsSchema: {
        workspace_id: z.string().uuid().optional().describe('Workspace ID (optional)'),
        project_id: z.string().uuid().optional().describe('Project ID (optional)'),
        title: z.string().optional().describe('Preference title (optional)'),
        preference: z.string().describe('Preference details to remember'),
      },
    },
    async (args) => ({
      messages: [
        {
          role: 'user',
          content: {
            type: 'text',
            text: `Save this preference to memory:

${args.title ? `Title: ${args.title}\n` : ''}Preference: ${args.preference}

If workspace_id/project_id are not provided, call \`session_init\` first to resolve the current workspace/project.

Use \`session_capture\` with:
- event_type: "preference"
- title: ${args.title ? `"${args.title}"` : '(choose a short title)'}
- content: "${args.preference}"
- importance: "medium"`,
          },
        },
      ],
    })
  );

  // Task capture prompt
  server.registerPrompt(
    'capture-task',
    {
      title: 'Capture Task',
      description: 'Capture an action item into memory',
      argsSchema: {
        workspace_id: z.string().uuid().optional().describe('Workspace ID (optional)'),
        project_id: z.string().uuid().optional().describe('Project ID (optional)'),
        title: z.string().optional().describe('Task title (optional)'),
        task: z.string().describe('Task details'),
      },
    },
    async (args) => ({
      messages: [
        {
          role: 'user',
          content: {
            type: 'text',
            text: `Capture this task in memory for tracking:

${args.title ? `Title: ${args.title}\n` : ''}Task: ${args.task}

If workspace_id/project_id are not provided, call \`session_init\` first to resolve the current workspace/project.

Use \`session_capture\` with:
- event_type: "task"
- title: ${args.title ? `"${args.title}"` : '(choose a short title)'}
- content: "${args.task}"
- importance: "medium"`,
          },
        },
      ],
    })
  );

  // Bug capture prompt
  server.registerPrompt(
    'capture-bug',
    {
      title: 'Capture Bug',
      description: 'Capture a bug report into workspace memory',
      argsSchema: {
        workspace_id: z.string().uuid().optional().describe('Workspace ID (optional)'),
        project_id: z.string().uuid().optional().describe('Project ID (optional)'),
        title: z.string().describe('Bug title'),
        description: z.string().describe('Bug description'),
        reproduction_steps: z.string().optional().describe('Steps to reproduce (optional)'),
        expected: z.string().optional().describe('Expected behavior (optional)'),
        actual: z.string().optional().describe('Actual behavior (optional)'),
      },
    },
    async (args) => ({
      messages: [
        {
          role: 'user',
          content: {
            type: 'text',
            text: `Capture this bug report in memory:

Title: ${args.title}
Description: ${args.description}
${args.reproduction_steps ? `Steps to reproduce:\n${args.reproduction_steps}\n` : ''}${args.expected ? `Expected:\n${args.expected}\n` : ''}${args.actual ? `Actual:\n${args.actual}\n` : ''}

If workspace_id/project_id are not provided, call \`session_init\` first to resolve the current workspace/project.

Use \`session_capture\` with:
- event_type: "bug"
- title: "${args.title}"
- content: A well-formatted bug report with the details above
- tags: include relevant component/area tags
- importance: "high"`,
          },
        },
      ],
    })
  );

  // Feature capture prompt
  server.registerPrompt(
    'capture-feature',
    {
      title: 'Capture Feature',
      description: 'Capture a feature request into workspace memory',
      argsSchema: {
        workspace_id: z.string().uuid().optional().describe('Workspace ID (optional)'),
        project_id: z.string().uuid().optional().describe('Project ID (optional)'),
        title: z.string().describe('Feature title'),
        description: z.string().describe('Feature description'),
        rationale: z.string().optional().describe('Why this matters (optional)'),
        acceptance_criteria: z.string().optional().describe('Acceptance criteria (optional)'),
      },
    },
    async (args) => ({
      messages: [
        {
          role: 'user',
          content: {
            type: 'text',
            text: `Capture this feature request in memory:

Title: ${args.title}
Description: ${args.description}
${args.rationale ? `Rationale:\n${args.rationale}\n` : ''}${args.acceptance_criteria ? `Acceptance criteria:\n${args.acceptance_criteria}\n` : ''}

If workspace_id/project_id are not provided, call \`session_init\` first to resolve the current workspace/project.

Use \`session_capture\` with:
- event_type: "feature"
- title: "${args.title}"
- content: A well-formatted feature request with the details above
- tags: include relevant component/area tags
- importance: "medium"`,
          },
        },
      ],
    })
  );

  // Plan generation prompt
  server.registerPrompt(
    'generate-plan',
    {
      title: 'Generate Plan',
      description: 'Generate a development plan from a description',
      argsSchema: {
        description: z.string().describe('What you want to build/fix'),
        project_id: z.string().uuid().optional().describe('Project ID (optional)'),
        complexity: z.string().optional().describe('low|medium|high (optional)'),
      },
    },
    async (args) => ({
      messages: [
        {
          role: 'user',
          content: {
            type: 'text',
            text: `Generate a development plan for:

${args.description}

Use \`ai_plan\` with:
- description: "${args.description}"
${args.project_id ? `- project_id: "${args.project_id}"` : ''}
${args.complexity ? `- complexity: "${args.complexity}"` : ''}

Then present the plan as a concise ordered list with clear milestones and risks.`,
          },
        },
      ],
    })
  );

  // Task generation prompt
  server.registerPrompt(
    'generate-tasks',
    {
      title: 'Generate Tasks',
      description: 'Generate actionable tasks from a plan or description',
      argsSchema: {
        plan_id: z.string().optional().describe('Plan ID (optional)'),
        description: z.string().optional().describe('Description to generate tasks from (optional if plan_id provided)'),
        project_id: z.string().uuid().optional().describe('Project ID (optional)'),
        granularity: z.string().optional().describe('fine|medium|coarse (optional)'),
      },
    },
    async (args) => ({
      messages: [
        {
          role: 'user',
          content: {
            type: 'text',
            text: `Generate actionable tasks.

${args.plan_id ? `Use plan_id: ${args.plan_id}` : ''}${!args.plan_id && args.description ? `Use description: ${args.description}` : ''}

Use \`ai_tasks\` with:
${args.plan_id ? `- plan_id: "${args.plan_id}"` : ''}${!args.plan_id && args.description ? `- description: "${args.description}"` : ''}
${args.project_id ? `- project_id: "${args.project_id}"` : ''}
${args.granularity ? `- granularity: "${args.granularity}"` : ''}

Then output a checklist of tasks with clear acceptance criteria for each.`,
          },
        },
      ],
    })
  );

  // Token-budget context prompt
  server.registerPrompt(
    'token-budget-context',
    {
      title: 'Token-Budget Context',
      description: 'Get the most relevant context that fits within a token budget',
      argsSchema: {
        query: z.string().describe('What you need context for'),
        max_tokens: z.string().describe('Max tokens for context (e.g., 500, 1000, 2000)'),
        workspace_id: z.string().uuid().optional().describe('Workspace ID (optional)'),
        project_id: z.string().uuid().optional().describe('Project ID (optional)'),
        include_code: z.string().optional().describe('Include code ("true" or "false")'),
      },
    },
    async (args) => ({
      messages: [
        {
          role: 'user',
          content: {
            type: 'text',
            text: `Build the most relevant context for:

Query: ${args.query}
Token budget: ${args.max_tokens}

Use \`ai_context_budget\` with:
- query: "${args.query}"
- max_tokens: ${args.max_tokens} (number)
${args.workspace_id ? `- workspace_id: "${args.workspace_id}"` : ''}
${args.project_id ? `- project_id: "${args.project_id}"` : ''}
${args.include_code ? `- include_code: ${args.include_code}` : ''}

Return the packed context plus a short note about what was included/excluded.`,
          },
        },
      ],
    })
  );

  // TODO/FIXME scan prompt
  server.registerPrompt(
    'find-todos',
    {
      title: 'Find TODOs',
      description: 'Scan the codebase for TODO/FIXME/HACK notes and summarize',
      argsSchema: {
        project_id: z.string().uuid().optional().describe('Project ID (optional)'),
        pattern: z.string().optional().describe('Regex/pattern to search (default: TODO|FIXME|HACK)'),
      },
    },
    async (args) => ({
      messages: [
        {
          role: 'user',
          content: {
            type: 'text',
            text: `Find TODO-style notes in the codebase.

If project_id is not provided, first call \`session_init\` (or \`projects_list\`) to resolve the current project ID.

Use \`search_pattern\` with query "${args.pattern || 'TODO|FIXME|HACK'}"${args.project_id ? ` and project_id "${args.project_id}"` : ''}.
Group results by file path, summarize themes, and propose a small prioritized cleanup list.`,
          },
        },
      ],
    })
  );

  // Generate editor rules prompt
  server.registerPrompt(
    'generate-editor-rules',
    {
      title: 'Generate Editor Rules',
      description: 'Generate ContextStream AI rule files for your editor',
      argsSchema: {
        folder_path: z.string().describe('Project folder path (ideally absolute)'),
        editors: z.string().optional().describe('Comma-separated editors or "all" (windsurf,cursor,cline,kilo,roo,claude,aider)'),
        workspace_id: z.string().uuid().optional().describe('Workspace ID (optional)'),
        workspace_name: z.string().optional().describe('Workspace name (optional)'),
        project_name: z.string().optional().describe('Project name (optional)'),
        additional_rules: z.string().optional().describe('Additional project-specific rules (optional)'),
        dry_run: z.string().optional().describe('Dry run ("true" or "false", default: false)'),
      },
    },
    async (args) => ({
      messages: [
        {
          role: 'user',
          content: {
            type: 'text',
            text: `Generate ContextStream editor rule files in: ${args.folder_path}

Use \`generate_editor_rules\` with:
- folder_path: "${args.folder_path}"
${args.editors ? `- editors: "${args.editors}"` : ''}
${args.workspace_id ? `- workspace_id: "${args.workspace_id}"` : ''}
${args.workspace_name ? `- workspace_name: "${args.workspace_name}"` : ''}
${args.project_name ? `- project_name: "${args.project_name}"` : ''}
${args.additional_rules ? `- additional_rules: "${args.additional_rules}"` : ''}
${args.dry_run ? `- dry_run: ${args.dry_run}` : ''}

If editors is provided as a comma-separated string, split it into an array (or use ["all"] to generate for all editors). If dry_run is provided as a string, convert to boolean.`,
          },
        },
      ],
    })
  );

  // Index local repo prompt
  server.registerPrompt(
    'index-local-repo',
    {
      title: 'Index Local Repo',
      description: 'Ingest local files into ContextStream for indexing/search',
      argsSchema: {
        project_id: z.string().uuid().optional().describe('Project ID (optional if session_init has set defaults)'),
        path: z.string().describe('Local directory path to ingest'),
      },
    },
    async (args) => ({
      messages: [
        {
          role: 'user',
          content: {
            type: 'text',
            text: `Ingest local files for indexing/search.

Path: ${args.path}

If project_id is not provided, call \`session_init\` first and use the resolved project_id (or use \`projects_list\` to select).

Use \`projects_ingest_local\` with:
- project_id: ${args.project_id ? `"${args.project_id}"` : '(resolved from session_init)'}
- path: "${args.path}"

Then advise how to monitor progress via \`projects_index_status\`.`,
          },
        },
      ],
    })
  );
}
