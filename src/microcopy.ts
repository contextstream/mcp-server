/**
 * Educational microcopy and hints for ContextStream MCP tools.
 * Provides helpful messages to guide users on next steps after tool operations.
 */

/**
 * Get a session init tip based on session state
 */
export function getSessionInitTip(sessionId: string): string {
  return `Session ${sessionId.slice(0, 8)}... initialized. Use context(user_message="...") to get relevant context.`;
}

/**
 * Get a capture hint based on event type
 */
export function getCaptureHint(eventType: string): string {
  switch (eventType) {
    case "decision":
      return "Decision captured. It will surface in future context() calls when relevant.";
    case "preference":
      return "Preference saved. Future sessions will respect this preference.";
    case "insight":
      return "Insight captured. Use recall() to retrieve it later.";
    case "task":
      return "Task captured. Use list_tasks() to view all tasks.";
    case "lesson":
      return "Lesson saved. It will warn you before similar mistakes.";
    case "session_snapshot":
      return "Session state saved. Use restore_context() after compaction.";
    default:
      return "Event captured. Use recall() to retrieve related events.";
  }
}

/**
 * Get an empty state hint for various operations
 */
export function getEmptyStateHint(operation: string): string {
  switch (operation) {
    case "get_lessons":
      return "No lessons found. Lessons are captured when mistakes occur.";
    case "recall":
      return "No memories found. Use capture() or remember() to save context.";
    case "list_plans":
      return "No plans found. Use capture_plan() to create an implementation plan.";
    case "list_events":
      return "No events found. Events are captured as you work.";
    case "list_tasks":
      return "No tasks found. Use create_task() to add tasks.";
    case "list_todos":
      return "No todos found. Use create_todo() to add quick todos.";
    case "list_diagrams":
      return "No diagrams found. Use create_diagram() to save a Mermaid diagram.";
    case "list_docs":
      return "No docs found. Use create_doc() to save documentation.";
    default:
      return "No results found.";
  }
}

/**
 * Get a hint for plan status updates
 */
export function getPlanStatusHint(status: string): string {
  switch (status) {
    case "draft":
      return "Plan saved as draft. Update status to 'active' when ready.";
    case "active":
      return "Plan is now active. Create tasks to track implementation.";
    case "completed":
      return "Plan completed. Great work!";
    case "archived":
      return "Plan archived. It will still appear in searches.";
    case "abandoned":
      return "Plan abandoned. Consider capturing lessons learned.";
    default:
      return "Plan updated. Changes are preserved.";
  }
}

/**
 * Post-compact operation hints
 */
export const POST_COMPACT_HINTS = {
  restored: "Context restored from pre-compaction snapshot.",
  restored_with_session: (sessionId: string) =>
    `Context restored from session ${sessionId.slice(0, 8)}... snapshot.`,
  no_snapshot: "No snapshot found. Session state may be incomplete.",
  failed: "Failed to restore context. Try recall() to find relevant memories.",
};

/**
 * Integration status hints
 */
export const INTEGRATION_HINTS = {
  connected: "Integration connected and syncing.",
  not_connected: "Integration not connected. Connect at:",
  sync_in_progress: "Sync in progress. Results may be incomplete.",
  sync_complete: "Sync complete. All data is current.",
};

/**
 * Task management hints
 */
export const TASK_HINTS = {
  created: "Task created. Use update_task() to change status.",
  completed: "Task completed. Well done!",
  blocked: "Task blocked. Add blocked_reason for context.",
  linked_to_plan: "Task linked to plan. Progress will be tracked.",
};

/**
 * Workspace management hints
 */
export const WORKSPACE_HINTS = {
  created: "Workspace created. Associate projects with associate().",
  associated: "Folder associated with workspace. Context will be scoped here.",
};

/**
 * Project management hints
 */
export const PROJECT_HINTS = {
  created: "Project created. Use index() to enable code search.",
  indexed: "Project indexed. Code search is now available.",
  indexing: "Indexing in progress. Search will improve as files are processed.",
};
