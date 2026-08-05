import { describe, it, expect } from "vitest";
import { generateAllRuleFiles, generateRuleContent } from "./rules-templates.js";

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

  it("generates Copilot instructions plus a companion skill file", () => {
    const result = generateRuleContent("copilot", { mode: "bootstrap" });
    expect(result).not.toBeNull();
    expect(result!.filename).toBe(".github/copilot-instructions.md");
    expect(result!.content).toContain("contextstream-workflow");

    const copilotFiles = generateAllRuleFiles({ mode: "bootstrap" }).filter(
      (file) => file.editor === "copilot"
    );
    expect(copilotFiles.map((file) => file.filename)).toContain(".github/copilot-instructions.md");
    expect(copilotFiles.map((file) => file.filename)).toContain(
      ".github/skills/contextstream-workflow/SKILL.md"
    );
    expect(copilotFiles.find((file) => file.filename.endsWith("SKILL.md"))?.content).toContain(
      "name: contextstream-workflow"
    );
  });

  it("applies no-hooks guidance to Copilot rules", () => {
    const result = generateRuleContent("copilot", { mode: "bootstrap" });
    expect(result).not.toBeNull();
    expect(result!.content).toContain("No Hooks Available");
    expect(result!.content).toContain("session_id");
  });

  it("includes Antigravity-specific no-hooks reliability guidance", () => {
    const result = generateRuleContent("antigravity", { mode: "bootstrap" });
    expect(result).not.toBeNull();
    expect(result!.content).toContain("Antigravity-Specific Reliability Notes");
    expect(result!.content).toContain("no documented lifecycle hooks");
  });

  it("generates Windsurf rules with always_on frontmatter", () => {
    const result = generateRuleContent("windsurf", { mode: "bootstrap" });
    expect(result).not.toBeNull();
    expect(result!.filename).toBe(".windsurf/rules/contextstream.md");
    expect(result!.content).toContain("trigger: always_on");
    expect(result!.content).toContain("# Windsurf Rules");
  });

  it.each(["bootstrap", "minimal", "full"] as const)(
    "includes current direct-read and canonical handoff guidance in %s mode",
    (mode) => {
      const result = generateRuleContent("codex", { mode });
      expect(result).not.toBeNull();
      const content = result!.content;
      expect(content).toContain("## Fast Direct-Read Lane");
      expect(content).toContain('project(action="list"|"get"|"index_status")');
      expect(content).toContain("Do not use this lane for recall, decisions, searches");
      expect(content).toContain("## Canonical Agent Handoffs");
      expect(content).toContain('entity(kind="handoff", action="create"');
      expect(content).toContain("HANDOFF.md");
    }
  );

  it("treats fresh grounding as the first continuation lookup", () => {
    const result = generateRuleContent("codex", { mode: "bootstrap" });
    expect(result).not.toBeNull();
    expect(result!.content).toContain('do not immediately call `session(action="recall")`');
    expect(result!.content).toContain("First explicit escalation after insufficient grounding");
  });
});
