import { describe, it, expect } from "vitest";
import { generateRuleContent } from "./rules-templates.js";

describe("rules-templates plan-mode guidance", () => {
  it("bootstrap rules mention plan-mode discovery tools in search-first guidance", () => {
    const result = generateRuleContent("claude", { mode: "bootstrap" });
    expect(result).not.toBeNull();

    const content = result!.content;
    expect(content).toContain("Glob/Grep/Read/Explore/Task/EnterPlanMode");
  });

  it("no-hooks supplement discourages Explore file-by-file scans during planning", () => {
    const result = generateRuleContent("codex", { mode: "bootstrap" });
    expect(result).not.toBeNull();

    const content = result!.content;
    expect(content).toContain("Task(subagent_type=\"Explore\")");
    expect(content).toContain("search(mode=\"auto\", query=\"...\", output_format=\"paths\")");
  });
});
