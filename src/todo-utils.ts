export type TodoStatus = "pending" | "completed";

function normalizeTodoStatus(status: unknown): TodoStatus | undefined {
  if (status === "pending" || status === "completed") {
    return status;
  }
  return undefined;
}

/**
 * Resolve todo completion updates from legacy and current input fields.
 *
 * Supports:
 * - completed: boolean
 * - todo_status: "pending" | "completed"
 * - status alias where values are "pending" | "completed"
 */
export function resolveTodoCompletionUpdate(input: {
  completed?: boolean;
  todo_status?: TodoStatus;
  status?: string;
}): {
  completed?: boolean;
  status?: TodoStatus;
} {
  if (typeof input.completed === "boolean") {
    return {
      completed: input.completed,
      status: input.completed ? "completed" : "pending",
    };
  }

  const normalizedStatus =
    normalizeTodoStatus(input.todo_status) ?? normalizeTodoStatus(input.status);
  if (!normalizedStatus) {
    return {};
  }

  return {
    completed: normalizedStatus === "completed",
    status: normalizedStatus,
  };
}
