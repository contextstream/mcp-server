import type { McpServer } from '@modelcontextprotocol/sdk/server/mcp.js';

const ID_NOTES = [
  'Notes:',
  '- If ContextStream is not initialized in this conversation, call `session_init` first (omit ids).',
  '- Do not ask me for `workspace_id`/`project_id` — use session defaults or IDs returned by `session_init`.',
  '- Prefer omitting IDs in tool calls when the tool supports defaults.',
];

const upgradeUrl = process.env.CONTEXTSTREAM_UPGRADE_URL || 'https://contextstream.io/pricing';
const proPrompts = new Set<string>([
  'build-context',
  'generate-plan',
  'generate-tasks',
  'token-budget-context',
]);

function promptAccessLabel(promptName: string): 'Free' | 'PRO' {
  return proPrompts.has(promptName) ? 'PRO' : 'Free';
}

export function registerPrompts(server: McpServer) {
  server.registerPrompt(
    'explore-codebase',
    {
      title: `Explore Codebase (${promptAccessLabel('explore-codebase')})`,
      description: 'Get an overview of a project codebase structure and key components',
    },
    async () => ({
      messages: [
        {
          role: 'user',
          content: {
            type: 'text',
            text: [
              'I want to understand the current codebase.',
              '',
              ...ID_NOTES,
              '',
              'Please help me by:',
              '1. Use `projects_overview` to get a project summary (use session defaults; only pass `project_id` if required).',
              '2. Use `projects_files` to identify key entry points.',
              '3. If a focus area is clear from our conversation, prioritize it; otherwise ask me what to focus on.',
              '4. Use `search_semantic` (and optionally `search_hybrid`) to find the most relevant files.',
              '5. Summarize the architecture, major modules, and where to start editing.',
            ].join('\n'),
          },
        },
      ],
    })
  );

  server.registerPrompt(
    'capture-decision',
    {
      title: `Capture Decision (${promptAccessLabel('capture-decision')})`,
      description: 'Document an architectural or technical decision in workspace memory',
    },
    async () => ({
      messages: [
        {
          role: 'user',
          content: {
            type: 'text',
            text: [
              'Please capture an architectural/technical decision in ContextStream memory.',
              '',
              ...ID_NOTES,
              '',
              'Instructions:',
              '- If the decision is already described in this conversation, extract: title, context, decision, consequences/tradeoffs.',
              '- If anything is missing, ask me 1–3 quick questions to fill the gaps.',
              '- Then call `session_capture` with:',
              '  - event_type: "decision"',
              '  - title: (short ADR title)',
              '  - content: a well-formatted ADR (Context, Decision, Consequences)',
              '  - tags: include relevant tags (e.g., "adr", "architecture")',
              '  - importance: "high"',
              '',
              'After capturing, confirm what was saved.',
            ].join('\n'),
          },
        },
      ],
    })
  );

  server.registerPrompt(
    'review-context',
    {
      title: `Code Review Context (${promptAccessLabel('review-context')})`,
      description: 'Build context for reviewing code changes',
    },
    async () => ({
      messages: [
        {
          role: 'user',
          content: {
            type: 'text',
            text: [
              'I need context to review a set of code changes.',
              '',
              ...ID_NOTES,
              '',
              'First:',
              '- If file paths and a short change description are not already in this conversation, ask me for them.',
              '',
              'Then build review context by:',
              '1. Using `graph_dependencies` to find what depends on the changed areas.',
              '2. Using `graph_impact` to assess potential blast radius.',
              '3. Using `memory_search` to find related decisions/notes.',
              '4. Using `search_semantic` to find related code patterns.',
              '',
              'Provide:',
              '- What the files/components do',
              '- What might be affected',
              '- Relevant prior decisions/lessons',
              '- Review checklist + risks to focus on',
            ].join('\n'),
          },
        },
      ],
    })
  );

  server.registerPrompt(
    'investigate-bug',
    {
      title: `Investigate Bug (${promptAccessLabel('investigate-bug')})`,
      description: 'Build context for debugging an issue',
    },
    async () => ({
      messages: [
        {
          role: 'user',
          content: {
            type: 'text',
            text: [
              'I want help investigating a bug.',
              '',
              ...ID_NOTES,
              '',
              'First:',
              '- If the error/symptom is not already stated, ask me for the exact error message and what I expected vs what happened.',
              '- If an affected area/component is not known, ask me where I noticed it.',
              '',
              'Then:',
              '1. Use `search_semantic` to find code related to the error/symptom.',
              '2. Use `search_pattern` to locate where similar errors are thrown or logged.',
              '3. If you identify key functions, use `graph_call_path` to trace call flows.',
              '4. Use `memory_search` to check if we have prior notes/bugs about this area.',
              '',
              'Return:',
              '- Likely origin locations',
              '- Call flow (if found)',
              '- Related past context',
              '- Suggested debugging steps',
            ].join('\n'),
          },
        },
      ],
    })
  );

  server.registerPrompt(
    'explore-knowledge',
    {
      title: `Explore Knowledge Graph (${promptAccessLabel('explore-knowledge')})`,
      description: 'Navigate and understand the knowledge graph for a workspace',
    },
    async () => ({
      messages: [
        {
          role: 'user',
          content: {
            type: 'text',
            text: [
              'Help me explore the knowledge captured in this workspace.',
              '',
              ...ID_NOTES,
              '',
              'Approach:',
              '1. Use `memory_summary` for a high-level overview.',
              '2. Use `memory_decisions` to see decision history (titles + a few key details).',
              '3. Use `memory_list_nodes` to see available knowledge nodes.',
              '4. If a starting topic is clear from the conversation, use `memory_search` for it.',
              '5. Use `graph_related` on the most relevant nodes to expand connections.',
              '',
              'Provide:',
              '- Key themes and topics',
              '- Important decisions + rationale',
              '- Suggested “next nodes” to explore',
            ].join('\n'),
          },
        },
      ],
    })
  );

  server.registerPrompt(
    'onboard-to-project',
    {
      title: `Project Onboarding (${promptAccessLabel('onboard-to-project')})`,
      description: 'Generate onboarding context for a new team member',
    },
    async () => ({
      messages: [
        {
          role: 'user',
          content: {
            type: 'text',
            text: [
              'Create an onboarding guide for a new team member joining this project.',
              '',
              ...ID_NOTES,
              '',
              'First:',
              '- If the role is not specified, ask me what role they are onboarding into (backend, frontend, fullstack, etc.).',
              '',
              'Gather context:',
              '1. Use `projects_overview` and `projects_statistics` for project summary.',
              '2. Use `projects_files` to identify key entry points.',
              '3. Use `memory_timeline` and `memory_decisions` to understand recent changes and architectural choices.',
              '4. Use `search_semantic` to find READMEs/docs/setup instructions.',
              '',
              'Output:',
              '- Project overview and purpose',
              '- Tech stack + architecture map',
              '- Key files/entry points relevant to the role',
              '- Important decisions + rationale',
              '- Recent changes/current focus',
              '- Step-by-step getting started',
            ].join('\n'),
          },
        },
      ],
    })
  );

  server.registerPrompt(
    'analyze-refactoring',
    {
      title: `Refactoring Analysis (${promptAccessLabel('analyze-refactoring')})`,
      description: 'Analyze a codebase for refactoring opportunities',
    },
    async () => ({
      messages: [
        {
          role: 'user',
          content: {
            type: 'text',
            text: [
              'Analyze the codebase for refactoring opportunities.',
              '',
              ...ID_NOTES,
              '',
              'If a target area is obvious from our conversation, focus there; otherwise ask me what area to analyze.',
              '',
              'Please investigate:',
              '1. `graph_circular_dependencies` (circular deps to break)',
              '2. `graph_unused_code` (dead code to remove)',
              '3. `search_pattern` (duplication patterns)',
              '4. `projects_statistics` (high complexity hotspots)',
              '',
              'Provide a prioritized list with quick wins vs deeper refactors.',
            ].join('\n'),
          },
        },
      ],
    })
  );

  server.registerPrompt(
    'build-context',
    {
      title: `Build LLM Context (${promptAccessLabel('build-context')})`,
      description: 'Build comprehensive context for an LLM task',
    },
    async () => ({
      messages: [
        {
          role: 'user',
          content: {
            type: 'text',
            text: [
              'Build comprehensive context for the task we are working on.',
              '',
              `Access: ${promptAccessLabel('build-context')}${promptAccessLabel('build-context') === 'PRO' ? ` (upgrade: ${upgradeUrl})` : ''}`,
              '',
              ...ID_NOTES,
              '',
              'First:',
              '- If the “query” is clear from the latest user request, use that.',
              '- Otherwise ask me: “What do you need context for?”',
              '',
              'Then:',
              '- Call `ai_enhanced_context` with include_code=true, include_docs=true, include_memory=true (omit IDs unless required).',
              '- Synthesize the returned context into a short briefing with links/file paths and key decisions/risks.',
            ].join('\n'),
          },
        },
      ],
    })
  );

  server.registerPrompt(
    'smart-search',
    {
      title: `Smart Search (${promptAccessLabel('smart-search')})`,
      description: 'Search across memory, decisions, and code for a query',
    },
    async () => ({
      messages: [
        {
          role: 'user',
          content: {
            type: 'text',
            text: [
              'Find the most relevant context for what I am asking about.',
              '',
              ...ID_NOTES,
              '',
              'First:',
              '- If a query is clear from the conversation, use it.',
              '- Otherwise ask me what I want to find.',
              '',
              'Then:',
              '1. Use `session_smart_search` for the query.',
              '2. If results are thin, follow up with `search_hybrid` and `memory_search`.',
              '3. Return the top results with file paths/links and a short synthesis.',
            ].join('\n'),
          },
        },
      ],
    })
  );

  server.registerPrompt(
    'recall-context',
    {
      title: `Recall Context (${promptAccessLabel('recall-context')})`,
      description: 'Retrieve relevant past decisions and memory for a query',
    },
    async () => ({
      messages: [
        {
          role: 'user',
          content: {
            type: 'text',
            text: [
              'Recall relevant past context (decisions, notes, lessons) for what I am asking about.',
              '',
              ...ID_NOTES,
              '',
              'First:',
              '- If a recall query is clear from the conversation, use it.',
              '- Otherwise ask me what topic I want to recall.',
              '',
              'Then:',
              '- Use `session_recall` with the query (omit IDs unless required).',
              '- Summarize the key points and any relevant decisions/lessons.',
            ].join('\n'),
          },
        },
      ],
    })
  );

  server.registerPrompt(
    'session-summary',
    {
      title: `Session Summary (${promptAccessLabel('session-summary')})`,
      description: 'Get a compact summary of workspace/project context',
    },
    async () => ({
      messages: [
        {
          role: 'user',
          content: {
            type: 'text',
            text: [
              'Generate a compact, token-efficient summary of the current workspace/project context.',
              '',
              ...ID_NOTES,
              '',
              'Use `session_summary` (default max_tokens=500 unless I specify otherwise).',
              'Then list:',
              '- Top decisions (titles only)',
              '- Any high-priority lessons to watch for',
            ].join('\n'),
          },
        },
      ],
    })
  );

  server.registerPrompt(
    'capture-lesson',
    {
      title: `Capture Lesson (${promptAccessLabel('capture-lesson')})`,
      description: 'Record a lesson learned from an error or correction',
    },
    async () => ({
      messages: [
        {
          role: 'user',
          content: {
            type: 'text',
            text: [
              'Capture a lesson learned so it is surfaced in future sessions.',
              '',
              ...ID_NOTES,
              '',
              'If the lesson details are not fully present in the conversation, ask me for:',
              '- title (what to remember)',
              '- severity (low|medium|high|critical, default medium)',
              '- category (workflow|code_quality|verification|communication|project_specific)',
              '- trigger (what caused it)',
              '- impact (what went wrong)',
              '- prevention (how to prevent it)',
              '- keywords (optional)',
              '',
              'Then call `session_capture_lesson` with those fields and confirm it was saved.',
            ].join('\n'),
          },
        },
      ],
    })
  );

  server.registerPrompt(
    'capture-preference',
    {
      title: `Capture Preference (${promptAccessLabel('capture-preference')})`,
      description: 'Save a user preference to memory',
    },
    async () => ({
      messages: [
        {
          role: 'user',
          content: {
            type: 'text',
            text: [
              'Save a user preference to ContextStream memory.',
              '',
              ...ID_NOTES,
              '',
              'If the preference is not explicit in the conversation, ask me what to remember.',
              '',
              'Then call `session_capture` with:',
              '- event_type: "preference"',
              '- title: (short title)',
              '- content: (preference text)',
              '- importance: "medium"',
            ].join('\n'),
          },
        },
      ],
    })
  );

  server.registerPrompt(
    'capture-task',
    {
      title: `Capture Task (${promptAccessLabel('capture-task')})`,
      description: 'Capture an action item into memory',
    },
    async () => ({
      messages: [
        {
          role: 'user',
          content: {
            type: 'text',
            text: [
              'Capture an action item into ContextStream memory.',
              '',
              ...ID_NOTES,
              '',
              'If the task is not explicit in the conversation, ask me what to capture.',
              '',
              'Then call `session_capture` with:',
              '- event_type: "task"',
              '- title: (short title)',
              '- content: (task details)',
              '- importance: "medium"',
            ].join('\n'),
          },
        },
      ],
    })
  );

  server.registerPrompt(
    'capture-bug',
    {
      title: `Capture Bug (${promptAccessLabel('capture-bug')})`,
      description: 'Capture a bug report into workspace memory',
    },
    async () => ({
      messages: [
        {
          role: 'user',
          content: {
            type: 'text',
            text: [
              'Capture a bug report in ContextStream memory.',
              '',
              ...ID_NOTES,
              '',
              'If details are missing, ask me for:',
              '- title',
              '- description',
              '- steps to reproduce (optional)',
              '- expected behavior (optional)',
              '- actual behavior (optional)',
              '',
              'Then call `session_capture` with:',
              '- event_type: "bug"',
              '- title: (bug title)',
              '- content: a well-formatted bug report (include all provided details)',
              '- tags: component/area tags',
              '- importance: "high"',
            ].join('\n'),
          },
        },
      ],
    })
  );

  server.registerPrompt(
    'capture-feature',
    {
      title: `Capture Feature (${promptAccessLabel('capture-feature')})`,
      description: 'Capture a feature request into workspace memory',
    },
    async () => ({
      messages: [
        {
          role: 'user',
          content: {
            type: 'text',
            text: [
              'Capture a feature request in ContextStream memory.',
              '',
              ...ID_NOTES,
              '',
              'If details are missing, ask me for:',
              '- title',
              '- description',
              '- rationale (optional)',
              '- acceptance criteria (optional)',
              '',
              'Then call `session_capture` with:',
              '- event_type: "feature"',
              '- title: (feature title)',
              '- content: a well-formatted feature request',
              '- tags: component/area tags',
              '- importance: "medium"',
            ].join('\n'),
          },
        },
      ],
    })
  );

  server.registerPrompt(
    'generate-plan',
    {
      title: `Generate Plan (${promptAccessLabel('generate-plan')})`,
      description: 'Generate a development plan from a description',
    },
    async () => ({
      messages: [
        {
          role: 'user',
          content: {
            type: 'text',
            text: [
              'Generate a development plan for what I am trying to build/fix.',
              '',
              `Access: ${promptAccessLabel('generate-plan')}${promptAccessLabel('generate-plan') === 'PRO' ? ` (upgrade: ${upgradeUrl})` : ''}`,
              '',
              ...ID_NOTES,
              '',
              'Use the most recent user request as the plan description. If unclear, ask me for a one-paragraph description.',
              '',
              'Then call `ai_plan` and present the plan as an ordered list with milestones and risks.',
            ].join('\n'),
          },
        },
      ],
    })
  );

  server.registerPrompt(
    'generate-tasks',
    {
      title: `Generate Tasks (${promptAccessLabel('generate-tasks')})`,
      description: 'Generate actionable tasks from a plan or description',
    },
    async () => ({
      messages: [
        {
          role: 'user',
          content: {
            type: 'text',
            text: [
              'Generate actionable tasks for the work we are discussing.',
              '',
              `Access: ${promptAccessLabel('generate-tasks')}${promptAccessLabel('generate-tasks') === 'PRO' ? ` (upgrade: ${upgradeUrl})` : ''}`,
              '',
              ...ID_NOTES,
              '',
              'If a plan_id exists in the conversation, use it. Otherwise use the latest user request as the description.',
              'If granularity is not specified, default to medium.',
              '',
              'Call `ai_tasks` and return a checklist of tasks with acceptance criteria for each.',
            ].join('\n'),
          },
        },
      ],
    })
  );

  server.registerPrompt(
    'token-budget-context',
    {
      title: `Token-Budget Context (${promptAccessLabel('token-budget-context')})`,
      description: 'Get the most relevant context that fits within a token budget',
    },
    async () => ({
      messages: [
        {
          role: 'user',
          content: {
            type: 'text',
            text: [
              'Build the most relevant context that fits within a token budget.',
              '',
              `Access: ${promptAccessLabel('token-budget-context')}${promptAccessLabel('token-budget-context') === 'PRO' ? ` (upgrade: ${upgradeUrl})` : ''}`,
              '',
              ...ID_NOTES,
              '',
              'First:',
              '- If a query is clear from the conversation, use it; otherwise ask me for a query.',
              '- If max_tokens is not specified, ask me for a token budget (e.g., 500/1000/2000).',
              '',
              'Then call `ai_context_budget` and return the packed context plus a short note about what was included/excluded.',
            ].join('\n'),
          },
        },
      ],
    })
  );

  server.registerPrompt(
    'find-todos',
    {
      title: `Find TODOs (${promptAccessLabel('find-todos')})`,
      description: 'Scan the codebase for TODO/FIXME/HACK notes and summarize',
    },
    async () => ({
      messages: [
        {
          role: 'user',
          content: {
            type: 'text',
            text: [
              'Scan the codebase for TODO/FIXME/HACK notes and summarize them.',
              '',
              ...ID_NOTES,
              '',
              'Use `search_pattern` with query `TODO|FIXME|HACK` (or a pattern inferred from the conversation).',
              'Group results by file path, summarize themes, and propose a small prioritized cleanup list.',
            ].join('\n'),
          },
        },
      ],
    })
  );

  server.registerPrompt(
    'generate-editor-rules',
    {
      title: `Generate Editor Rules (${promptAccessLabel('generate-editor-rules')})`,
      description: 'Generate ContextStream AI rule files for your editor',
    },
    async () => ({
      messages: [
        {
          role: 'user',
          content: {
            type: 'text',
            text: [
              'Generate ContextStream AI rule files for my editor.',
              '',
              ...ID_NOTES,
              '',
              'First:',
              '- If you can infer the project folder path from the environment/IDE roots, use it.',
              '- Otherwise ask me for an absolute folder path.',
              '- Ask which editor(s) (windsurf,cursor,cline,kilo,roo,claude,aider) or default to all.',
              '',
              'Then call `generate_rules` and confirm which files were created/updated.',
              'Ask if the user also wants to apply rules globally (pass apply_global: true).',
            ].join('\n'),
          },
        },
      ],
    })
  );

  server.registerPrompt(
    'index-local-repo',
    {
      title: `Index Local Repo (${promptAccessLabel('index-local-repo')})`,
      description: 'Ingest local files into ContextStream for indexing/search',
    },
    async () => ({
      messages: [
        {
          role: 'user',
          content: {
            type: 'text',
            text: [
              'Ingest local files into ContextStream for indexing/search.',
              '',
              ...ID_NOTES,
              '',
              'First:',
              '- Ask me for the local directory path to ingest if it is not already specified.',
              '',
              'Then:',
              '- Call `projects_ingest_local` with the path (use session defaults for project, or the `project_id` returned by `session_init`).',
              '- Explain how to monitor progress via `projects_index_status`.',
            ].join('\n'),
          },
        },
      ],
    })
  );
}
