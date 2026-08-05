import { describe, expect, it } from "vitest";
import { CAPSULE_TOOL_DESCRIPTION, ENTITY_TOOL_DESCRIPTION } from "./tools.js";

describe("handoff tool discovery metadata", () => {
  it("routes generic handoffs to the durable entity", () => {
    for (const phrase of [
      "prepare a handoff",
      "hand work over",
      "continue in another agent/session",
      "canonical durable handoff",
      "HANDOFF.md",
      "omit to_user_id",
    ]) {
      expect(ENTITY_TOOL_DESCRIPTION).toContain(phrase);
    }
  });

  it("keeps capsules as optional portable artifacts", () => {
    expect(CAPSULE_TOOL_DESCRIPTION).toContain("first create entity(kind=handoff)");
    expect(CAPSULE_TOOL_DESCRIPTION).toContain("not a replacement for the canonical handoff");
    expect(CAPSULE_TOOL_DESCRIPTION).toContain("Do not use capsule for normal turn-by-turn");
  });
});
