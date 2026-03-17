import { afterEach, describe, expect, it, vi } from "vitest";
import { writeHookOutput } from "./common.js";

describe("writeHookOutput", () => {
  afterEach(() => {
    delete process.env.HOOK_EVENT_NAME;
    vi.restoreAllMocks();
  });

  it("includes hookSpecificOutput for supported events", () => {
    const logSpy = vi.spyOn(console, "log").mockImplementation(() => {});

    writeHookOutput({
      additionalContext: "context",
      hookEventName: "PreToolUse",
    });

    const payload = JSON.parse(logSpy.mock.calls[0][0] as string);
    expect(payload.hookSpecificOutput).toEqual({
      hookEventName: "PreToolUse",
      additionalContext: "context",
    });
    expect(payload.additionalContext).toBe("context");
  });

  it("omits hookSpecificOutput for unsupported events", () => {
    const logSpy = vi.spyOn(console, "log").mockImplementation(() => {});

    writeHookOutput({
      additionalContext: "context",
      hookEventName: "SessionStart",
    });

    const payload = JSON.parse(logSpy.mock.calls[0][0] as string);
    expect(payload.hookSpecificOutput).toBeUndefined();
    expect(payload.additionalContext).toBe("context");
  });

  it("falls back to HOOK_EVENT_NAME when hookEventName is omitted", () => {
    process.env.HOOK_EVENT_NAME = "PostToolUse";
    const logSpy = vi.spyOn(console, "log").mockImplementation(() => {});

    writeHookOutput({
      additionalContext: "context",
    });

    const payload = JSON.parse(logSpy.mock.calls[0][0] as string);
    expect(payload.hookSpecificOutput).toEqual({
      hookEventName: "PostToolUse",
      additionalContext: "context",
    });
  });
});
