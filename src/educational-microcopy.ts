/**
 * Educational Microcopy - Teaching users about the AI-to-human artifact bridge
 *
 * These one-liners help users understand the value of persistent context
 * and artifacts that survive ephemeral AI conversations.
 *
 * Principles:
 * - Token-efficient (< 15 words)
 * - Actionable where possible
 * - Show value, don't lecture
 */

// ============================================================================
// Session Init Tips - Shown on every session start (rotating)
// ============================================================================

export const SESSION_INIT_TIPS = [
  "AI work that doesn't disappear.",
  "Every conversation builds context. Every artifact persists.",
  "Your AI remembers. Now you can see what it knows.",
  "Context that survives sessions.",
  "The bridge between AI conversations and human artifacts.",
];

/**
 * Get a rotating tip for session init based on simple hash of session ID
 */
export function getSessionInitTip(sessionId?: string): string {
  const index = sessionId
    ? Math.abs(hashString(sessionId)) % SESSION_INIT_TIPS.length
    : Math.floor(Math.random() * SESSION_INIT_TIPS.length);
  return SESSION_INIT_TIPS[index];
}

// ============================================================================
// Capture Success Hints - Contextual hints based on what was captured
// ============================================================================

export const CAPTURE_HINTS: Record<string, string> = {
  // Core event types
  decision: "Future you will thank present you.",
  preference: "Noted. This will inform future suggestions.",
  insight: "Captured for future reference.",
  note: "Saved. Won't disappear when the chat does.",
  implementation: "Implementation recorded.",
  task: "Task tracked.",
  bug: "Bug logged for tracking.",
  feature: "Feature request captured.",
  plan: "This plan will be here when you come back.",
  correction: "Correction noted. Learning from this.",
  lesson: "Learn once, remember forever.",
  warning: "Warning logged.",
  frustration: "Feedback captured. We're listening.",
  conversation: "Conversation preserved.",
  session_snapshot: "Session state saved. Ready to resume anytime.",
};

/**
 * Get the appropriate hint for a captured event type
 */
export function getCaptureHint(eventType: string): string {
  return CAPTURE_HINTS[eventType] || "Captured. This will survive when the chat disappears.";
}

// ============================================================================
// Empty State Messages - When queries return no results
// ============================================================================

export const EMPTY_STATE_HINTS: Record<string, string> = {
  // Plans
  list_plans: "No plans yet. Plans live here, not in chat transcripts.",
  get_plan: "Plan not found. Create one to track implementation across sessions.",

  // Memory & Search
  recall: "Nothing found yet. Context builds over time.",
  search: "No matches. Try a different query or let context accumulate.",
  list_events: "No events yet. Start capturing decisions and insights.",
  decisions: "No decisions captured yet. Record the 'why' behind your choices.",
  timeline: "Timeline empty. Events will appear as you work.",

  // Lessons
  get_lessons: "No lessons yet. Capture mistakes to learn from them.",

  // Tasks
  list_tasks: "No tasks yet. Break plans into trackable tasks.",

  // Todos
  list_todos: "No todos yet. Action items will appear here.",

  // Diagrams
  list_diagrams: "No diagrams yet. AI-generated diagrams persist here.",

  // Docs
  list_docs: "No docs yet. Turn AI explanations into shareable documentation.",

  // Reminders
  list_reminders: "No reminders set. Schedule follow-ups that won't be forgotten.",

  // Generic
  default: "Nothing yet. Start a conversation—context builds over time.",
};

/**
 * Get the appropriate empty state hint for a given action
 */
export function getEmptyStateHint(action: string): string {
  return EMPTY_STATE_HINTS[action] || EMPTY_STATE_HINTS.default;
}

// ============================================================================
// Post-Compaction Messages - When restoring context after session end
// ============================================================================

export const POST_COMPACT_HINTS = {
  restored: "Picked up where you left off. Session ended, memory didn't.",
  restored_with_session: (sessionId: string) =>
    `Restored from session ${sessionId}. Session ended, memory didn't.`,
  no_snapshot: "No prior session found. Fresh start—context will build as you work.",
  failed: "Couldn't restore previous session. Use context to retrieve what you need.",
};

// ============================================================================
// Plan Lifecycle Hints - For plan state transitions
// ============================================================================

export const PLAN_HINTS: Record<string, string> = {
  created: "Plan saved. It will be here when you come back.",
  activated: "Plan is now active. Track progress across sessions.",
  completed: "Plan completed. The journey is preserved for future reference.",
  abandoned: "Plan archived. Abandoned plans still teach.",
  updated: "Plan updated. Changes are preserved.",
};

/**
 * Get hint for plan status change
 */
export function getPlanStatusHint(status: string): string {
  const statusMap: Record<string, string> = {
    draft: PLAN_HINTS.created,
    active: PLAN_HINTS.activated,
    completed: PLAN_HINTS.completed,
    abandoned: PLAN_HINTS.abandoned,
    archived: PLAN_HINTS.abandoned,
  };
  return statusMap[status] || PLAN_HINTS.updated;
}

// ============================================================================
// Task Lifecycle Hints
// ============================================================================

export const TASK_HINTS: Record<string, string> = {
  created: "Task tracked. It won't disappear when the chat does.",
  completed: "Task done. Progress is preserved.",
  blocked: "Task blocked. Capture what's blocking for future reference.",
  cancelled: "Task cancelled. The history remains.",
};

// ============================================================================
// Integration Hints - For connected services
// ============================================================================

export const INTEGRATION_HINTS: Record<string, string> = {
  not_connected:
    "Connect integrations to sync context where your team already works.",
  slack: "Context from Slack conversations, connected.",
  github: "Issues and PRs linked to the decisions behind them.",
  notion: "Decisions synced to where your team documents.",
};

// ============================================================================
// Search Result Hints - When search finds results
// ============================================================================

export const SEARCH_HINTS = {
  found: (count: number) =>
    count === 1
      ? "Found 1 match from your AI history."
      : `Found ${count} matches from your AI history.`,
  semantic: "Results ranked by meaning, not just keywords.",
  exhaustive: "Complete search across all indexed content.",
};

// ============================================================================
// Context Tips - Occasional tips in context responses (used sparingly)
// ============================================================================

export const CONTEXT_TIPS = [
  "Search everything AI ever helped you with.",
  "Find that thing the AI explained three weeks ago.",
  "Decisions captured here inform future AI suggestions.",
];

/**
 * Get a context tip (returns undefined most of the time to avoid noise)
 * Only returns a tip ~10% of the time
 */
export function getContextTip(callCount?: number): string | undefined {
  // If we have a call count, show tip every 10th call
  if (callCount !== undefined) {
    return callCount % 10 === 0 ? CONTEXT_TIPS[callCount % CONTEXT_TIPS.length] : undefined;
  }
  // Random 10% chance
  if (Math.random() < 0.1) {
    return CONTEXT_TIPS[Math.floor(Math.random() * CONTEXT_TIPS.length)];
  }
  return undefined;
}

// ============================================================================
// Memory/Artifact Specific Hints
// ============================================================================

export const ARTIFACT_HINTS: Record<string, string> = {
  diagram: "AI drew this. Now it's yours to keep and share.",
  doc: "From conversation to documentation.",
  roadmap: "Roadmap persisted. Track progress across sessions.",
  todo: "Action item captured. It won't disappear when the chat does.",
};

// ============================================================================
// Workspace & Project Hints
// ============================================================================

export const WORKSPACE_HINTS = {
  created: "Workspace ready. Context that spans projects.",
  associated: "Folder linked. Decisions here inform AI across sessions.",
  switched: "Workspace switched. Each workspace has its own memory.",
};

export const PROJECT_HINTS = {
  created: "Project indexed. Your codebase, searchable.",
  indexed: "Indexing complete. AI now understands your code structure.",
  ingest_started: "Indexing started. Code context builds in the background.",
};

// ============================================================================
// Helper Functions
// ============================================================================

/**
 * Simple string hash function for deterministic tip selection
 */
function hashString(str: string): number {
  let hash = 0;
  for (let i = 0; i < str.length; i++) {
    const char = str.charCodeAt(i);
    hash = (hash << 5) - hash + char;
    hash = hash & hash; // Convert to 32bit integer
  }
  return hash;
}

/**
 * Format a hint for inclusion in a response
 * Adds the hint field if hint is provided
 */
export function addHintToResponse<T extends Record<string, unknown>>(
  response: T,
  hint: string | undefined
): T & { hint?: string } {
  if (hint) {
    return { ...response, hint };
  }
  return response;
}
