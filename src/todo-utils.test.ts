import { describe, expect, it } from "vitest";
import { resolveTodoCompletionUpdate } from "./todo-utils.js";

describe("resolveTodoCompletionUpdate", () => {
  it("maps completed=true to completed status", () => {
    expect(resolveTodoCompletionUpdate({ completed: true })).toEqual({
      completed: true,
      status: "completed",
    });
  });

  it("maps completed=false to pending status", () => {
    expect(resolveTodoCompletionUpdate({ completed: false })).toEqual({
      completed: false,
      status: "pending",
    });
  });

  it("maps todo_status to completed flag", () => {
    expect(resolveTodoCompletionUpdate({ todo_status: "completed" })).toEqual({
      completed: true,
      status: "completed",
    });
    expect(resolveTodoCompletionUpdate({ todo_status: "pending" })).toEqual({
      completed: false,
      status: "pending",
    });
  });

  it("maps status alias when it matches todo states", () => {
    expect(resolveTodoCompletionUpdate({ status: "completed" })).toEqual({
      completed: true,
      status: "completed",
    });
    expect(resolveTodoCompletionUpdate({ status: "pending" })).toEqual({
      completed: false,
      status: "pending",
    });
  });

  it("prioritizes explicit completed over status aliases", () => {
    expect(resolveTodoCompletionUpdate({ completed: false, todo_status: "completed" })).toEqual({
      completed: false,
      status: "pending",
    });
  });

  it("ignores non-todo status aliases", () => {
    expect(resolveTodoCompletionUpdate({ status: "in_progress" })).toEqual({});
  });
});
